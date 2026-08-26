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

    // Force 256-color support in the PTY so commands emit rich colors, set on
    // the child rather than mutating this process's environment.
    let cmd = pty_process::Command::new(shell)
        .args(["-l", "-i"])
        .env("TERM", "xterm-256color");
    let mut child = cmd.spawn(pts).context("Failed to spawn shell")?;

    let (mut reader, mut writer) = pty.into_split();

    // Phase 1: Wait for the initial prompt to appear.
    let prompt_bytes = read_until_idle(
        &mut reader,
        Duration::from_millis(500),
        Duration::from_secs(5),
    )
    .await?;

    // Phase 2: Send the commands, then the bookkeeping line.
    let nonce = format!("{:06x}", rand_u32() & 0x00ff_ffff);
    let sentinel = format!("{}S_{}__", SENTINEL_HEAD, nonce);
    // Marker appended to the bookkeeping input line. Deliberately *not* the
    // sentinel itself, so echoing this line does not trip the "command
    // finished" check below before the exit status has been printed.
    let cut_marker = format!("#TS{}", nonce);

    let mut raw_after_prompt: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;

    // Strip terminal title-setting sequences before feeding vt100, which does
    // not handle them and would render the title text as visible characters
    // (e.g. ESC k ls ESC \ sets a GNU Screen window title to "ls", but vt100
    // would print "ls" on screen).
    let clean_prompt = strip_title_sequences(&prompt_bytes);
    let mut check_parser = vt100::Parser::new(rows, cols, 0);
    check_parser.process(&clean_prompt);

    // Phase 1b: clear the screen, which does two jobs at once.
    //
    // It drops shell startup noise (MOTD, version banners) so the capture
    // begins with a single prompt at the top, and it puts a freshly drawn
    // prompt at a known origin: the cursor then sits on that prompt's input
    // line, so `cursor_row + 1` is exactly how many rows the prompt occupies.
    // That height is what lets the cut below remove the *whole* trailing
    // prompt, including the upper rows of a multi-line PS1 - measuring it from
    // the startup screen instead would be thrown off by any banner.
    writer
        .write_all(b"clear 2>/dev/null || printf '\\033[H\\033[2J'\n")
        .await
        .context("Failed to write screen reset to PTY")?;
    read_until_quiet(
        &mut reader,
        &mut raw_after_prompt,
        &mut check_parser,
        QUIET_PERIOD,
        deadline,
    )
    .await?;

    let prompt_rows = prompt_height(check_parser.screen());
    // Column the cursor rests at once the prompt is drawn. Used below to tell
    // "the command finished and the shell is waiting for input" apart from
    // "the command is simply quiet for a moment".
    let prompt_col = check_parser.screen().cursor_position().1;
    // Text of the prompt's upper rows, used as a second, independent signal
    // when stripping the trailing prompt.
    let prompt_prefix = prompt_prefix_rows(check_parser.screen(), cols);

    // Each command is sent on its own line - the shell prints a PS1 prompt
    // before each one - and we wait for its output to go quiet before queueing
    // the next. Sending everything up front let a command that reads stdin
    // (nmap's runtime keypress status, a pager, an interactive prompt) swallow
    // the input meant for the shell, which silently lost the rest of the
    // capture.
    for cmd in commands {
        writer
            .write_all(format!("{}\n", cmd).as_bytes())
            .await
            .context("Failed to write command to PTY")?;
        read_until_prompt(
            &mut reader,
            &mut raw_after_prompt,
            &mut check_parser,
            prompt_col,
            deadline,
        )
        .await?;
    }

    // Capture the exit status of the last command and echo a sentinel carrying
    // it back to us, on a single input line ending in a comment that holds the
    // nonce. Keeping it to one line leaves the smallest possible bookkeeping
    // footprint on screen, and the nonce comment is what the cut below keys
    // off: the old marker was a generic `echo ''`, which truncated any capture
    // that merely contained that text (e.g. `cat` of a shell script).
    //
    // The sentinel is spelled as two concatenated string literals so the
    // *echoed input line* never contains the token itself - only the line the
    // shell prints does. Otherwise the reader would stop the moment the shell
    // redrew the input, before the exit status had been printed.
    writer
        .write_all(
            format!(
                "{BOOKKEEPING_PREFIX}$?; echo \"{SENTINEL_HEAD}\"\"S_{nonce}__:${BOOKKEEPING_VAR}\" {cut_marker}\n"
            )
            .as_bytes(),
        )
        .await
        .context("Failed to write sentinel to PTY")?;

    // Phase 3: Read until the sentinel value appears in the parsed screen. The
    // parser (rather than the raw byte stream) is checked so ANSI escapes
    // interspersed in the echoed text cannot hide it.
    let mut buf = [0u8; 4096];
    let mut found_sentinel = false;

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
                if sentinel_printed(check_parser.screen(), cols, &sentinel) {
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
                // Idle timeout: re-check the screen, then keep waiting until
                // the deadline.
                if sentinel_printed(check_parser.screen(), cols, &sentinel) {
                    found_sentinel = true;
                    break;
                }
                continue;
            }
        }
    }

    // Phase 4: Build the final output.
    //
    // `check_parser` now holds the full screen state. The visible text is cut
    // just before the internal bookkeeping line (the `__TS_EC` capture
    // and the sentinel echo), which also drops the trailing PS1 prompt, and
    // the remaining rows are re-emitted as raw bytes for rendering.

    let mut captured_exit: Option<i32> = None;

    let final_output = if found_sentinel {
        // Use the screen rows to find where to cut.
        // We use screen.rows() rather than screen.contents() because
        // contents() merges wrapped lines, causing row index mismatches.
        let screen = check_parser.screen();

        // Find the first row that belongs to termshot's own bookkeeping input.
        // Both internal lines contain the random nonce, so nothing the captured
        // command prints can be mistaken for them.
        let screen_rows: Vec<String> = screen.rows(0, cols).collect();

        // Parse the captured exit status from the sentinel output line
        // (`<sentinel>:<code>`). The shell also echoes the input line that
        // produced it, which carries the nonce comment, so rows containing the
        // cut marker are skipped.
        let status_prefix = format!("{}:", sentinel);
        captured_exit = screen_rows.iter().find_map(|line| {
            if line.contains(&cut_marker) {
                return None;
            }
            line.trim()
                .strip_prefix(&status_prefix)
                .and_then(|rest| rest.trim().parse::<i32>().ok())
        });

        let cut_line = find_cut_line(
            screen,
            cols,
            &cut_marker,
            &sentinel,
            &prompt_prefix,
            prompt_rows,
        );

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

    // Phase 5: Clean up. On success, ask the login shell to exit; on timeout,
    // forcibly terminate the whole session/process group so nothing is left
    // running. Either way we always reap the child.
    if found_sentinel {
        let _ = writer.write_all(b"exit\n").await;
    }
    // Closing PTY input signals EOF to the shell.
    drop(writer);

    let shell_status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => terminate_and_reap(&mut child).await,
    };

    // Prefer the captured command exit status; fall back to the shell's.
    let exit_code = captured_exit.or_else(|| shell_status.and_then(|s| s.code()));

    let timed_out = !found_sentinel;

    Ok(ExecResult {
        raw_output: final_output,
        exit_code,
        timed_out,
    })
}

/// The rows a multi-line prompt draws *above* its input line, sampled from a
/// freshly drawn prompt (the cursor sits on the input line). Empty for the
/// common single-line prompt. Used as a text signal when stripping the
/// trailing prompt, alongside the measured prompt height.
fn prompt_prefix_rows(screen: &vt100::Screen, cols: u16) -> Vec<String> {
    /// Never look further than this many rows back: enough for realistic
    /// multi-line prompts, small enough to never eat real output.
    const MAX_PROMPT_ROWS: usize = 4;

    let cursor_row = screen.cursor_position().0 as usize;
    let rows: Vec<String> = screen.rows(0, cols).collect();
    let mut prefix = Vec::new();
    for offset in 1..=MAX_PROMPT_ROWS.min(cursor_row) {
        let text = rows[cursor_row - offset].trim_end().to_string();
        if text.is_empty() {
            break;
        }
        prefix.push(text);
    }
    prefix
}

/// Move the cut point up over the leading rows of a multi-line prompt whose
/// text still matches the sampled prompt.
///
/// `cut` is the row holding termshot's bookkeeping input, which the shell drew
/// after printing its final prompt. This is the text-based half of the trailing
/// prompt strip; the height-based half in [`find_cut_line`] covers prompts
/// whose text changed (git state, timers) since it was sampled.
fn strip_trailing_prompt(rows: &[String], cut: usize, prompt_prefix: &[String]) -> usize {
    let mut idx = cut;
    for expected in prompt_prefix {
        // Some prompts print a blank line before themselves; skip it.
        let mut candidate = idx;
        while candidate > 0 && rows[candidate - 1].trim_end().is_empty() {
            candidate -= 1;
        }
        if candidate == 0 || rows[candidate - 1].trim_end() != *expected {
            break;
        }
        idx = candidate - 1;
    }
    idx
}

/// Find the row where termshot's own bookkeeping input begins, so everything
/// from there on (the exit-status capture, the sentinel echo, and the trailing
/// PS1 prompt) can be cut from the screenshot.
///
/// Both internal lines carry the random nonce, so captured output can never be
/// mistaken for them - matching a generic `echo ''` used to truncate any
/// capture that merely contained that text. When a narrow terminal wraps the
/// bookkeeping line, the cut moves back to the logical line's first row so no
/// fragment of it survives.
fn find_cut_line(
    screen: &vt100::Screen,
    cols: u16,
    cut_marker: &str,
    sentinel: &str,
    prompt_prefix: &[String],
    prompt_rows: usize,
) -> Option<usize> {
    let rows: Vec<String> = screen.rows(0, cols).collect();
    let (flat, row_of) = flatten_screen(&rows, cols);

    // Search the flattened screen rather than individual rows: a narrow
    // terminal wraps the bookkeeping input, and a per-row search would miss a
    // marker split across the margin.
    let marker_at = flat.find(cut_marker);
    let anchored_at = marker_at
        // The input line begins with the exit-status capture; anchoring there
        // cuts the whole line even when the marker itself wrapped.
        .and_then(|at| flat[..at].rfind(BOOKKEEPING_PREFIX))
        .or(marker_at);

    match anchored_at {
        Some(at) => {
            let idx = logical_line_start(screen, row_of[at] as u16) as usize;
            // That row is the input line of the prompt the shell drew after the
            // last command, so the `prompt_rows - 1` rows above it are the rest
            // of that prompt and must go too - otherwise a multi-line PS1
            // leaves its first line dangling at the bottom of the screenshot.
            let by_height = idx.saturating_sub(prompt_rows.saturating_sub(1));
            let by_text = strip_trailing_prompt(&rows, idx, prompt_prefix);
            Some(by_height.min(by_text))
        }
        // The input has already scrolled off: fall back to the line the
        // sentinel printed. Its position says nothing about the prompt height,
        // so only the text signal is used.
        None => {
            let at = flat.find(sentinel)?;
            let idx = logical_line_start(screen, row_of[at] as u16) as usize;
            Some(strip_trailing_prompt(&rows, idx, prompt_prefix))
        }
    }
}

/// How many rows the shell's prompt occupies, measured on a freshly cleared
/// screen: the prompt starts at row 0 and the cursor rests on its input line.
/// Clamped so a surprising screen state can never cut real output.
fn prompt_height(screen: &vt100::Screen) -> usize {
    /// Realistic multi-line prompts are two or three rows; refuse to treat
    /// more than this as prompt.
    const MAX_PROMPT_ROWS: usize = 4;

    (screen.cursor_position().0 as usize + 1).clamp(1, MAX_PROMPT_ROWS)
}

/// Join the screen rows (each padded to `cols`) into one string, plus a map
/// from byte offset back to the row it came from. Used to find text that a
/// soft wrap split across two rows.
fn flatten_screen(rows: &[String], cols: u16) -> (String, Vec<usize>) {
    let mut flat = String::new();
    let mut row_of = Vec::new();
    for (idx, row) in rows.iter().enumerate() {
        let mut width = 0usize;
        for ch in row.chars() {
            for _ in 0..ch.len_utf8() {
                row_of.push(idx);
            }
            flat.push(ch);
            width += 1;
        }
        for _ in width..cols as usize {
            row_of.push(idx);
            flat.push(' ');
        }
    }
    row_of.push(rows.len().saturating_sub(1));
    (flat, row_of)
}

/// True once the sentinel's *output* line is on screen.
///
/// The shell command spells the token as two concatenated literals, so the
/// input line the shell echoes back never contains it - only the line the
/// command printed does. The screen is flattened first so a wrapped output
/// line still matches.
fn sentinel_printed(screen: &vt100::Screen, cols: u16, sentinel: &str) -> bool {
    let rows: Vec<String> = screen.rows(0, cols).collect();
    let (flat, _) = flatten_screen(&rows, cols);
    flat.contains(sentinel)
}

/// Walk back from `row` to the first physical row of its logical line, i.e.
/// across any soft wraps that produced it. Used so a cut point found on a
/// wrapped continuation row removes the whole line, not just its tail.
fn logical_line_start(screen: &vt100::Screen, row: u16) -> u16 {
    let mut start = row;
    while start > 0 && screen.row_wrapped(start - 1) {
        start -= 1;
    }
    start
}

/// Forcibly terminate a PTY child and everything in its session/process
/// group, then reap it. The child is spawned as a session leader (setsid),
/// so its PID equals the process-group ID and `kill(-pid, ...)` reaches all
/// descendants, not just the shell itself.
async fn terminate_and_reap(child: &mut tokio::process::Child) -> Option<std::process::ExitStatus> {
    if let Some(pid) = child.id() {
        let pgid = pid as i32;
        // SIGTERM the group, give it a brief grace period, then SIGKILL.
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
        if let Ok(Ok(status)) = tokio::time::timeout(Duration::from_millis(500), child.wait()).await
        {
            return Some(status);
        }
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    } else {
        let _ = child.start_kill();
    }
    tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .ok()
        .and_then(|r| r.ok())
}

/// Rebuild raw ANSI bytes from a vt100 Screen, taking only the first
/// `keep_rows` rows. This preserves colors and attributes by emitting
/// SGR escape sequences for each cell.
fn rebuild_raw_from_screen(
    screen: &vt100::Screen,
    keep_rows: usize,
    rows: u16,
    cols: u16,
) -> Vec<u8> {
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

    let cmd = pty_process::Command::new(shell)
        .args(["-c", command])
        .env("TERM", "xterm-256color");
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

    // On timeout, forcibly terminate the whole process group before reaping.
    let exit_code = if timed_out {
        terminate_and_reap(&mut child).await.and_then(|s| s.code())
    } else {
        // The PTY reached EOF, but a process that closed its descriptors can
        // still be running: give it a short grace period, then terminate the
        // group so nothing is left behind unreaped.
        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(Ok(status)) => status.code(),
            _ => terminate_and_reap(&mut child).await.and_then(|s| s.code()),
        }
    };

    Ok(ExecResult {
        raw_output: all_output,
        exit_code,
        timed_out,
    })
}

/// Leading fragment of the sentinel token. The shell command that prints it
/// concatenates this with the rest, so the token only ever appears whole in the
/// shell's *output*, never in the input line it echoes back.
const SENTINEL_HEAD: &str = "__T";

/// Shell variable the last command's exit status is captured into.
const BOOKKEEPING_VAR: &str = "__TS_EC";

/// Start of the bookkeeping input line, used to anchor the cut point.
///
/// The whole line is kept short deliberately: the shell echoes it back, and a
/// line long enough to wrap costs an extra screen row, which on a small capture
/// scrolls the first prompt line off the top.
const BOOKKEEPING_PREFIX: &str = "__TS_EC=";

/// How long the PTY must stay silent before a command is considered finished
/// and the next line of input is queued.
const QUIET_PERIOD: Duration = Duration::from_millis(400);

/// Read from a PTY until it has produced nothing for `quiet_period` (or
/// `deadline` passes), appending the raw bytes to `raw` and feeding a
/// title-sequence-stripped copy to `parser`.
///
/// This is what keeps a running command from swallowing input meant for the
/// shell: the next line is only written once the current command has gone
/// quiet.
async fn read_until_quiet(
    reader: &mut pty_process::OwnedReadPty,
    raw: &mut Vec<u8>,
    parser: &mut vt100::Parser,
    quiet_period: Duration,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let mut buf = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        match tokio::time::timeout(quiet_period.min(remaining), reader.read(&mut buf)).await {
            Ok(Ok(0)) => return Ok(()),
            Ok(Ok(n)) => {
                let chunk = &buf[..n];
                raw.extend_from_slice(chunk);
                parser.process(&strip_title_sequences(chunk));
            }
            Ok(Err(e)) => {
                if e.raw_os_error() == Some(5) {
                    return Ok(());
                }
                return Err(anyhow::anyhow!("PTY read error: {}", e));
            }
            // Quiet period elapsed with no new output: the command is done.
            Err(_) => return Ok(()),
        }
    }
}

/// Read until the shell is back at its prompt: the PTY has been quiet for
/// [`QUIET_PERIOD`] *and* the cursor rests at the prompt's input column
/// (`prompt_col`, measured on the freshly drawn prompt).
///
/// Waiting for the prompt - not merely for silence - is what keeps a slow but
/// silent command (`sleep 5`, a long build) from having the next line of input
/// typed into it: a running command would otherwise have the terminal echo
/// that input into the middle of its output, or swallow it outright.
async fn read_until_prompt(
    reader: &mut pty_process::OwnedReadPty,
    raw: &mut Vec<u8>,
    parser: &mut vt100::Parser,
    prompt_col: u16,
    deadline: tokio::time::Instant,
) -> Result<()> {
    loop {
        read_until_quiet(reader, raw, parser, QUIET_PERIOD, deadline).await?;
        if parser.screen().cursor_position().1 == prompt_col {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(());
        }
    }
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
                    if i + 3 < data.len()
                        && (data[i + 2] == b'0' || data[i + 2] == b'1' || data[i + 2] == b'2')
                        && data[i + 3] == b';'
                    {
                        // This is a title-setting OSC. Skip until BEL or ST.
                        i += 4;
                        while i < data.len() {
                            if data[i] == 0x07 {
                                // BEL terminator
                                i += 1;
                                break;
                            }
                            if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
                                // ST terminator
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                        continue;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_gnu_screen_title_sequence() {
        let input = b"ls -la\r\n\x1bkls\x1b\\total 123\r\n";
        let output = strip_title_sequences(input);
        assert_eq!(String::from_utf8_lossy(&output), "ls -la\r\ntotal 123\r\n");
    }

    #[test]
    fn strips_osc_title_sequences_bel_and_st_terminated() {
        let input = b"a\x1b]0;hello world\x07b\x1b]2;other title\x1b\\c";
        let output = strip_title_sequences(input);
        assert_eq!(String::from_utf8_lossy(&output), "abc");
    }

    #[test]
    fn rebuild_raw_preserves_separate_rows() {
        let mut parser = vt100::Parser::new(10, 80, 0);
        parser.process(b"prompt$ ls -la\r\ntotal 616K\r\nwhoami\r\nadam\r\n");
        let screen = parser.screen();

        let rebuilt = rebuild_raw_from_screen(screen, 4, 10, 80);

        let mut reparsed = vt100::Parser::new(10, 80, 0);
        reparsed.process(&rebuilt);
        let rows: Vec<String> = reparsed.screen().rows(0, 80).collect();

        assert_eq!(rows[0].trim_end(), "prompt$ ls -la");
        assert_eq!(rows[1].trim_end(), "total 616K");
        assert_eq!(rows[2].trim_end(), "whoami");
        assert_eq!(rows[3].trim_end(), "adam");
    }

    #[test]
    fn cut_line_logic_keeps_multiple_prompts_but_not_trailing_prompt() {
        let mut parser = vt100::Parser::new(20, 120, 0);
        parser.process(
            b"prompt$ ls\r\nfile1\r\nprompt$ whoami\r\nadam\r\nprompt$ __TS_EC=$?; echo \"__T\"\"S_deadbe__:$__TS_EC\" #TSdeadbe\r\n__TS_deadbe__:0\r\nprompt$ ",
        );

        let screen = parser.screen();
        let cut_line = find_cut_line(screen, 120, "#TSdeadbe", "__TS_deadbe__", &[], 1);

        assert_eq!(cut_line, Some(4));

        let rebuilt = rebuild_raw_from_screen(screen, cut_line.unwrap(), 20, 120);
        let mut reparsed = vt100::Parser::new(20, 120, 0);
        reparsed.process(&rebuilt);
        let rebuilt_rows: Vec<String> = reparsed.screen().rows(0, 120).collect();

        assert_eq!(rebuilt_rows[0].trim_end(), "prompt$ ls");
        assert_eq!(rebuilt_rows[1].trim_end(), "file1");
        assert_eq!(rebuilt_rows[2].trim_end(), "prompt$ whoami");
        assert_eq!(rebuilt_rows[3].trim_end(), "adam");
        assert!(rebuilt_rows.iter().all(|r| !r.contains("__TS_")));
        assert!(rebuilt_rows.iter().all(|r| !r.contains("#TS")));
    }

    /// Captured output that merely *contains* `echo \'\'` (a shell script, a
    /// Makefile, `history`) must not be mistaken for termshot's own
    /// bookkeeping line and silently truncated.
    #[test]
    fn cut_line_ignores_user_output_containing_echo_blank() {
        let mut parser = vt100::Parser::new(20, 120, 0);
        parser.process(
            b"prompt$ cat script.sh\r\n#!/bin/bash\r\necho \'\'\r\necho \'after the blank\'\r\nprompt$ __TS_EC=$?; echo \"__T\"\"S_0badc0__:$__TS_EC\" #TS0badc0\r\n__TS_0badc0__:0\r\nprompt$ ",
        );

        let screen = parser.screen();
        let cut_line = find_cut_line(screen, 120, "#TS0badc0", "__TS_0badc0__", &[], 1);

        // The cut is the bookkeeping line (row 4), not the script\'s own
        // `echo \'\'` on row 2.
        assert_eq!(cut_line, Some(4));
        let rebuilt = rebuild_raw_from_screen(screen, cut_line.unwrap(), 20, 120);
        let mut reparsed = vt100::Parser::new(20, 120, 0);
        reparsed.process(&rebuilt);
        let rows: Vec<String> = reparsed.screen().rows(0, 120).collect();
        assert_eq!(rows[2].trim_end(), "echo \'\'");
        assert_eq!(rows[3].trim_end(), "echo \'after the blank\'");
    }

    /// When the bookkeeping line wraps on a narrow terminal, the cut must move
    /// back to the first row of that logical line so no fragment survives.
    #[test]
    fn cut_line_walks_back_over_wrapped_bookkeeping_line() {
        let mut parser = vt100::Parser::new(10, 20, 0);
        parser.process(
            b"$ ls\r\nfile1\r\n$ __TS_EC=$?; echo \"__T\"\"S_feedfa__:$__TS_EC\" #TSfeedfa\r\n",
        );

        let screen = parser.screen();
        let cut_line = find_cut_line(screen, 20, "#TSfeedfa", "__TS_feedfa__", &[], 1);

        assert_eq!(cut_line, Some(2), "cut should start at the wrapped line");
    }

    /// The nonce marker itself can be split across the right margin; the cut
    /// must still find it (a per-row search silently missed it and left the
    /// bookkeeping line in the screenshot).
    #[test]
    fn cut_line_finds_marker_split_across_the_margin() {
        // 50 columns puts the tail of the marker on the following row.
        let mut parser = vt100::Parser::new(8, 50, 0);
        parser.process(
            b"$ git log\r\ncommit one\r\n$ __TS_EC=$?; echo \"__T\"\"S_8ee506__:$__TS_EC\" #TS8ee506\r\n",
        );
        let screen = parser.screen();
        let rows: Vec<String> = screen.rows(0, 50).collect();
        assert!(
            !rows.iter().any(|r| r.contains("#TS8ee506")),
            "test setup: the marker should be split across rows"
        );

        let cut_line = find_cut_line(screen, 50, "#TS8ee506", "__TS_8ee506__", &[], 1);
        assert_eq!(cut_line, Some(2));
    }

    /// The read loop must not stop on the shell\'s echo of the sentinel input
    /// line, only on the line that command actually printed - otherwise the
    /// exit status is missed.
    #[test]
    fn sentinel_detection_ignores_the_echoed_input_line() {
        let mut parser = vt100::Parser::new(10, 120, 0);
        parser.process(b"prompt$ __TS_EC=$?; echo \"__T\"\"S_abcd12__:$__TS_EC\" #TSabcd12\r\n");
        assert!(!sentinel_printed(parser.screen(), 120, "__TS_abcd12__"));

        parser.process(b"__TS_abcd12__:0\r\n");
        assert!(sentinel_printed(parser.screen(), 120, "__TS_abcd12__"));
    }

    /// A multi-line PS1 must be stripped whole: its earlier rows would
    /// otherwise dangle under the last command\'s output.
    #[test]
    fn multiline_prompt_is_stripped_whole() {
        // Startup screen: a two-row prompt with the cursor on the input line.
        let mut startup = vt100::Parser::new(10, 60, 0);
        startup.process(b"adam@host ~/src\r\n$ ");
        let prefix = prompt_prefix_rows(startup.screen(), 60);
        assert_eq!(prefix, vec!["adam@host ~/src".to_string()]);

        // Final screen: output, then the trailing two-row prompt whose input
        // line carries the bookkeeping marker.
        let mut parser = vt100::Parser::new(10, 60, 0);
        parser.process(
            b"adam@host ~/src\r\n$ pwd\r\n/home/adam/src\r\nadam@host ~/src\r\n$ __TS_EC=$?; #TS123456\r\n",
        );
        let screen = parser.screen();
        let cut = find_cut_line(screen, 60, "#TS123456", "__TS_123456__", &prefix, 2);
        assert_eq!(cut, Some(3), "the whole trailing prompt should be cut");
    }

    /// The measured prompt height alone must remove the trailing prompt, even
    /// when its text changed since it was sampled: a prompt that shows git
    /// state changes the moment the captured command stages a file.
    #[test]
    fn trailing_prompt_is_stripped_when_its_text_changed() {
        let sampled = vec!["adam@host ~/src (main)".to_string()];

        let mut parser = vt100::Parser::new(10, 60, 0);
        parser.process(
            b"adam@host ~/src (main)\r\n$ git add -A\r\nadam@host ~/src (main*)\r\n$ __TS_EC=$?; #TSc0ffee\r\n",
        );
        let screen = parser.screen();
        let rows: Vec<String> = screen.rows(0, 60).collect();
        assert!(rows[2].contains("(main*)"), "test setup: prompt changed");

        // Text matching alone cannot recognize row 2; the measured height can.
        let cut = find_cut_line(screen, 60, "#TSc0ffee", "__TS_c0ffee__", &sampled, 2);
        assert_eq!(cut, Some(2), "changed trailing prompt must still be cut");
    }

    /// A single-line prompt shares its row with the bookkeeping input, so the
    /// height must not remove anything above it.
    #[test]
    fn single_line_prompt_height_cuts_only_its_own_row() {
        let mut parser = vt100::Parser::new(10, 60, 0);
        parser.process(b"$ echo hi\r\nhi\r\n$ __TS_EC=$?; #TS0f0f0f\r\n");
        let screen = parser.screen();
        let cut = find_cut_line(screen, 60, "#TS0f0f0f", "__TS_0f0f0f__", &[], 1);
        assert_eq!(cut, Some(2));
    }

    /// The prompt height is read off a freshly cleared screen, where the
    /// prompt starts at row 0 and the cursor rests on its input line.
    #[test]
    fn prompt_height_measures_rows_on_a_cleared_screen() {
        let mut one_line = vt100::Parser::new(10, 60, 0);
        one_line.process(b"adam@host:~$ ");
        assert_eq!(prompt_height(one_line.screen()), 1);

        let mut two_line = vt100::Parser::new(10, 60, 0);
        two_line.process(b"adam@host ~/src\r\n$ ");
        assert_eq!(prompt_height(two_line.screen()), 2);
    }

    #[test]
    fn single_line_prompt_has_no_prefix_rows() {
        let mut startup = vt100::Parser::new(10, 60, 0);
        startup.process(b"prompt$ ");
        assert!(prompt_prefix_rows(startup.screen(), 60).is_empty());
    }
}
