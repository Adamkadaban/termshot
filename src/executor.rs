use anyhow::{Context, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Result of executing a command in a PTY.
pub struct ExecResult {
    /// Raw bytes captured from the PTY (includes ANSI escape sequences).
    pub raw_output: Vec<u8>,
    /// Exit code of the command (None if timed out or unknown).
    pub exit_code: Option<i32>,
    /// Whether the command timed out.
    pub timed_out: bool,
}

/// Execute one or more commands in an interactive login shell, capturing the
/// PS1 prompt and full output including ANSI escape sequences.
///
/// When multiple commands are provided, each is sent on its own line so the
/// shell displays a fresh PS1 prompt before each one. The trailing prompt
/// after the last command is stripped from the output.
///
/// Strategy:
///   1. Spawn interactive shell, wait for initial prompt
///   2. Send all user commands, then a sentinel `echo`
///   3. Read output until sentinel value appears
///   4. Use vt100 to parse and re-render only the content up to (but not
///      including) the sentinel echo command, which also removes the
///      trailing PS1 prompt
pub async fn execute_command(
    commands: &[&str],
    shell: &str,
    rows: u16,
    cols: u16,
    timeout: Duration,
) -> Result<ExecResult> {
    let size = pty_process::Size::new(rows, cols);
    let (pty, pts) = pty_process::open().context("Failed to open PTY")?;
    pty.resize(size).context("Failed to resize PTY")?;

    let cmd = pty_process::Command::new(shell).args(["-l", "-i"]);
    let mut child = cmd.spawn(pts).context("Failed to spawn shell")?;

    let (mut reader, mut writer) = pty.into_split();

    // Phase 1: Wait for the initial prompt to appear.
    let prompt_bytes =
        read_until_idle(&mut reader, Duration::from_millis(500), Duration::from_secs(5)).await?;

    // Phase 2: Send all commands, then the sentinel.
    let sentinel = format!("__TERMSHOT_{:08x}__", rand_u32());
    // Send each command on its own line. The shell will display a PS1
    // prompt before each one (except the first, which already has the
    // prompt from Phase 1). After all commands, send the sentinel echo.
    let mut to_send = String::new();
    for cmd in commands {
        to_send.push_str(cmd);
        to_send.push('\n');
    }
    to_send.push_str(&format!("echo ''\necho '{sentinel}'\n"));
    writer
        .write_all(to_send.as_bytes())
        .await
        .context("Failed to write command to PTY")?;

    // Phase 3: Read until sentinel value appears in parsed screen contents.
    let mut raw_after_prompt: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + timeout;
    let mut found_sentinel = false;

    // We use a vt100 parser to check the screen contents for the sentinel.
    // This handles the case where ANSI escape sequences are interspersed
    // in the echoed text.
    //
    // Strip terminal title-setting sequences before feeding to vt100,
    // because vt100 doesn't handle them and would render the title text
    // as visible characters (e.g., ESC k ls ESC \ sets a GNU Screen
    // window title to "ls" but vt100 would print "ls" on screen).
    let clean_prompt = strip_title_sequences(&prompt_bytes);
    let mut check_parser = vt100::Parser::new(rows, cols, 0);
    check_parser.process(&clean_prompt);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        let read_timeout = remaining.min(Duration::from_millis(300));
        match tokio::time::timeout(read_timeout, reader.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let chunk = &buf[..n];
                raw_after_prompt.extend_from_slice(chunk);
                let clean_chunk = strip_title_sequences(chunk);
                check_parser.process(&clean_chunk);
                // Check the screen contents for the sentinel string
                let screen_text = check_parser.screen().contents();
                if screen_text.contains(&sentinel) {
                    found_sentinel = true;
                    break;
                }
            }
            Ok(Err(e)) => {
                if e.raw_os_error() == Some(5) {
                    break;
                }
                return Err(anyhow::anyhow!("PTY read error: {}", e));
            }
            Err(_) => {
                // Idle timeout -- check screen contents
                let screen_text = check_parser.screen().contents();
                if screen_text.contains(&sentinel) {
                    found_sentinel = true;
                    break;
                }
                // Keep waiting if we haven't hit the deadline
                continue;
            }
        }
    }

    // Phase 4: Build the final output.
    //
    // We now have the full screen state in check_parser. We need to rebuild
    // the raw bytes that produce just the prompt + command + output, WITHOUT
    // the sentinel commands and trailing prompt.
    //
    // Strategy: parse the screen text to find where the sentinel's echo
    // command starts, then use a fresh vt100 pass feeding only the bytes
    // up to that point.
    //
    // Simpler approach: use the screen contents from check_parser, find the
    // sentinel and the echo command preceding it, and reconstruct clean
    // output by replaying raw bytes through vt100 and checking screen
    // contents at each chunk boundary.
    //
    // Simplest approach: feed prompt_bytes + raw_after_prompt into a new
    // parser, but truncate the raw_after_prompt by finding where the
    // "echo" sentinel command was echoed in the raw stream.
    //
    // Actually, the cleanest approach is: we have the screen state. Let's
    // extract the relevant rows directly from the screen, skipping the
    // sentinel lines and trailing prompt.

    let final_output = if found_sentinel {
        // Use the screen rows to find where to cut.
        // We use screen.rows() rather than screen.contents() because
        // contents() merges wrapped lines, causing row index mismatches.
        let screen = check_parser.screen();

        let echo_sentinel_marker = format!("echo '{}'", sentinel);
        let echo_blank_marker = "echo ''";

        // Find the first row that contains sentinel-related echo commands
        let screen_rows: Vec<String> = screen.rows(0, cols).collect();

        let cut_line = screen_rows.iter().position(|line| {
            line.contains(&echo_sentinel_marker) || line.trim_end().ends_with(echo_blank_marker)
        });

        if let Some(idx) = cut_line {
            rebuild_raw_from_screen(screen, idx, rows, cols)
        } else {
            // Sentinel line not found in screen text; use raw output as-is
            let mut combined = prompt_bytes.clone();
            combined.extend_from_slice(&raw_after_prompt);
            combined
        }
    } else {
        let mut combined = prompt_bytes.clone();
        combined.extend_from_slice(&raw_after_prompt);
        combined
    };

    // Phase 5: Exit cleanly.
    let _ = writer.write_all(b"exit\n").await;

    let exit_code = tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .ok()
        .and_then(|r| r.ok())
        .and_then(|status| status.code());

    let timed_out = !found_sentinel && exit_code.is_none();

    Ok(ExecResult {
        raw_output: final_output,
        exit_code,
        timed_out,
    })
}

/// Rebuild raw ANSI bytes from a vt100 Screen, taking only the first
/// `keep_rows` rows. This preserves colors and attributes by emitting
/// SGR escape sequences for each cell.
fn rebuild_raw_from_screen(screen: &vt100::Screen, keep_rows: usize, rows: u16, cols: u16) -> Vec<u8> {
    let mut output = Vec::new();
    let total_rows = keep_rows.min(rows as usize);

    for row in 0..total_rows {
        // Track the last-emitted style to avoid redundant SGR codes
        let mut last_fg = vt100::Color::Default;
        let mut last_bg = vt100::Color::Default;
        let mut last_bold = false;
        let mut last_dim = false;
        let mut last_italic = false;
        let mut last_underline = false;
        let mut last_inverse = false;
        let mut first_cell = true;

        // Find the last non-empty column in this row to avoid trailing spaces
        let mut last_col = 0usize;
        for col in 0..cols as usize {
            if let Some(cell) = screen.cell(row as u16, col as u16) {
                if cell.has_contents() {
                    last_col = col + 1;
                }
            }
        }

        for col in 0..last_col {
            if let Some(cell) = screen.cell(row as u16, col as u16) {
                // Skip wide-char continuation cells
                if cell.is_wide_continuation() {
                    continue;
                }

                let fg = cell.fgcolor();
                let bg = cell.bgcolor();
                let bold = cell.bold();
                let dim = cell.dim();
                let italic = cell.italic();
                let underline = cell.underline();
                let inverse = cell.inverse();

                // Emit SGR if style changed
                if first_cell
                    || fg != last_fg
                    || bg != last_bg
                    || bold != last_bold
                    || dim != last_dim
                    || italic != last_italic
                    || underline != last_underline
                    || inverse != last_inverse
                {
                    let sgr = build_sgr(fg, bg, bold, dim, italic, underline, inverse);
                    output.extend_from_slice(sgr.as_bytes());
                    last_fg = fg;
                    last_bg = bg;
                    last_bold = bold;
                    last_dim = dim;
                    last_italic = italic;
                    last_underline = underline;
                    last_inverse = inverse;
                    first_cell = false;
                }

                let contents = cell.contents();
                if contents.is_empty() {
                    output.push(b' ');
                } else {
                    output.extend_from_slice(contents.as_bytes());
                }
            }
        }

        // Reset style at end of line and add newline
        output.extend_from_slice(b"\x1b[0m");
        if row + 1 < total_rows {
            output.extend_from_slice(b"\r\n");
        }
    }

    output
}

/// Build an SGR (Select Graphic Rendition) escape sequence for the given style.
fn build_sgr(
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
) -> String {
    let mut params: Vec<String> = vec!["0".to_string()]; // reset first

    if bold {
        params.push("1".to_string());
    }
    if dim {
        params.push("2".to_string());
    }
    if italic {
        params.push("3".to_string());
    }
    if underline {
        params.push("4".to_string());
    }
    if inverse {
        params.push("7".to_string());
    }

    match fg {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) if i < 8 => params.push(format!("{}", 30 + i)),
        vt100::Color::Idx(i) if i < 16 => params.push(format!("{}", 90 + i - 8)),
        vt100::Color::Idx(i) => params.push(format!("38;5;{}", i)),
        vt100::Color::Rgb(r, g, b) => params.push(format!("38;2;{};{};{}", r, g, b)),
    }

    match bg {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) if i < 8 => params.push(format!("{}", 40 + i)),
        vt100::Color::Idx(i) if i < 16 => params.push(format!("{}", 100 + i - 8)),
        vt100::Color::Idx(i) => params.push(format!("48;5;{}", i)),
        vt100::Color::Rgb(r, g, b) => params.push(format!("48;2;{};{};{}", r, g, b)),
    }

    format!("\x1b[{}m", params.join(";"))
}

/// Execute a command non-interactively (no shell prompt).
pub async fn execute_command_simple(
    command: &str,
    shell: &str,
    rows: u16,
    cols: u16,
    timeout: Duration,
) -> Result<ExecResult> {
    let size = pty_process::Size::new(rows, cols);
    let (pty, pts) = pty_process::open().context("Failed to open PTY")?;
    pty.resize(size).context("Failed to resize PTY")?;

    let cmd = pty_process::Command::new(shell).args(["-c", command]);
    let mut child = cmd.spawn(pts).context("Failed to spawn command")?;

    let (mut reader, _writer) = pty.into_split();

    let mut all_output: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];

    let result = tokio::time::timeout(timeout, async {
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => all_output.extend_from_slice(&buf[..n]),
                Err(e) => {
                    if e.raw_os_error() == Some(5) {
                        break;
                    }
                    return Err(anyhow::anyhow!("Read error: {}", e));
                }
            }
        }
        Ok(())
    })
    .await;

    let timed_out = result.is_err();

    let exit_code = tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .ok()
        .and_then(|r| r.ok())
        .and_then(|status| status.code());

    Ok(ExecResult {
        raw_output: all_output,
        exit_code,
        timed_out,
    })
}

/// Read from a PTY until no new data arrives for `idle_timeout`, or
/// `max_wait` elapses.
async fn read_until_idle(
    reader: &mut pty_process::OwnedReadPty,
    idle_timeout: Duration,
    max_wait: Duration,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + max_wait;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        let read_timeout = idle_timeout.min(remaining);
        match tokio::time::timeout(read_timeout, reader.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                output.extend_from_slice(&buf[..n]);
            }
            Ok(Err(e)) => {
                if e.raw_os_error() == Some(5) {
                    break;
                }
                return Err(anyhow::anyhow!("PTY read error: {}", e));
            }
            Err(_) => break,
        }
    }

    Ok(output)
}

/// Generate a random u32 for sentinel uniqueness.
fn rand_u32() -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::SystemTime;

    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    hasher.finish() as u32
}

/// Strip terminal title-setting escape sequences that vt100 doesn't handle.
///
/// These sequences set the terminal/window title and should be invisible,
/// but vt100 doesn't recognize them and renders their payload as visible text.
///
/// Sequences stripped:
/// - `ESC k <title> ESC \` (GNU Screen window title)
/// - `ESC ] 0 ; <title> BEL` (xterm/OSC window title, BEL-terminated)
/// - `ESC ] 0 ; <title> ESC \` (xterm/OSC window title, ST-terminated)
/// - `ESC ] 1 ; <title> ...`   (xterm icon name)
/// - `ESC ] 2 ; <title> ...`   (xterm window title)
fn strip_title_sequences(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;

    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() {
            match data[i + 1] {
                // ESC k ... ESC \ (GNU Screen title)
                b'k' => {
                    i += 2;
                    // Skip until ESC \ or end of data
                    while i < data.len() {
                        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
                // ESC ] (OSC sequence)
                b']' => {
                    // Check if it's ESC ] 0; or ESC ] 1; or ESC ] 2;
                    if i + 2 < data.len()
                        && (data[i + 2] == b'0' || data[i + 2] == b'1' || data[i + 2] == b'2')
                    {
                        if i + 3 < data.len() && data[i + 3] == b';' {
                            // This is a title-setting OSC. Skip until BEL or ST.
                            i += 4;
                            while i < data.len() {
                                if data[i] == 0x07 {
                                    // BEL terminator
                                    i += 1;
                                    break;
                                }
                                if data[i] == 0x1b
                                    && i + 1 < data.len()
                                    && data[i + 1] == b'\\'
                                {
                                    // ST terminator
                                    i += 2;
                                    break;
                                }
                                i += 1;
                            }
                            continue;
                        }
                    }
                    // Not a title OSC; pass through
                    result.push(data[i]);
                    i += 1;
                }
                _ => {
                    result.push(data[i]);
                    i += 1;
                }
            }
        } else {
            result.push(data[i]);
            i += 1;
        }
    }

    result
}
