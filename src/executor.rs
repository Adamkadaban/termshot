use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
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
///   4. Return the *original* PTY bytes, sliced to the capture: from the
///      screen-clearing escape up to the byte offset where termshot's own
///      bookkeeping line began, followed by an erase sequence that removes
///      the trailing PS1 prompt
///
/// Step 4 deliberately keeps the raw stream instead of re-emitting the parsed
/// screen row by row. Re-emitting turned every physical row into a hard
/// newline, which destroyed the terminal's soft-wrap information: a secret that
/// crossed the right margin then looked like two unrelated lines to the
/// renderer and to the redaction pass, so only one of its occurrences was
/// masked and the PNG metadata leaked the other.
pub async fn execute_command(
    commands: &[&str],
    shell: &str,
    rows: u16,
    cols: u16,
    timeout: Duration,
) -> Result<ExecResult> {
    execute_command_in_dir(commands, shell, rows, cols, timeout, None).await
}

/// Execute commands in an interactive login shell whose initial prompt starts
/// in `working_dir`. Passing `None` preserves [`execute_command`] behavior.
pub async fn execute_command_in_dir(
    commands: &[&str],
    shell: &str,
    rows: u16,
    cols: u16,
    timeout: Duration,
    working_dir: Option<&Path>,
) -> Result<ExecResult> {
    let size = pty_process::Size::new(rows, cols);
    let (pty, pts) = pty_process::open().context("Failed to open PTY")?;
    pty.resize(size).context("Failed to resize PTY")?;

    // Force 256-color support in the PTY so commands emit rich colors, set on
    // the child rather than mutating this process's environment.
    let cmd = pty_process::Command::new(shell)
        .args(["-l", "-i"])
        .env("TERM", "xterm-256color");
    let cmd = match working_dir {
        Some(dir) => cmd.current_dir(dir),
        None => cmd,
    };
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
    let initial_prompt_col = check_parser.screen().cursor_position().1;

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
    read_until_prompt(
        &mut reader,
        &mut raw_after_prompt,
        &mut check_parser,
        initial_prompt_col,
        deadline,
    )
    .await?;

    let prompt_rows = prompt_height(check_parser.screen());
    // Byte offset in `raw_after_prompt` where the captured screen begins: the
    // screen-clearing escape the `clear` above emitted. Everything before it is
    // shell startup noise (MOTD, banners) that the clear wiped, so slicing here
    // gives exactly what is on screen - without rebuilding any of it.
    let capture_start = find_clear_sequence(&raw_after_prompt);
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
    // footprint on screen, and the nonce comment identifies the sentinel line
    // in the parsed screen when the exit status is read back.
    //
    // The sentinel is spelled as two concatenated string literals so the
    // *echoed input line* never contains the token itself - only the line the
    // shell prints does. Otherwise the reader would stop the moment the shell
    // redrew the input, before the exit status had been printed.
    //
    // Nothing the shell echoes from here on belongs in the screenshot, so the
    // current length of the capture buffer is the exact byte offset to cut at.
    // Cutting by *offset* (rather than by searching the rendered screen for the
    // marker) is what lets the capture keep the original bytes: no captured
    // output can be mistaken for bookkeeping, and no screen row has to be
    // rebuilt.
    let bookkeeping_start = raw_after_prompt.len();
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
    // The capture is a *slice of the original PTY bytes*: from the clear escape
    // (so shell startup noise is left out) up to the offset recorded before the
    // bookkeeping line was sent (so neither the exit-status capture nor the
    // sentinel echo can appear). A short erase sequence is appended to remove
    // the trailing PS1 prompt, which the shell had already drawn by then.
    //
    // Nothing is re-emitted row by row, so every soft wrap the terminal
    // performed is still a soft wrap when the renderer re-parses these bytes -
    // which is what lets redaction see a value that crossed the right margin as
    // one string rather than two unrelated lines.

    let mut captured_exit: Option<i32> = None;

    let final_output = if found_sentinel {
        // Parse the captured exit status from the sentinel output line
        // (`<sentinel>:<code>`). The shell also echoes the input line that
        // produced it, which carries the nonce comment, so rows containing the
        // cut marker are skipped. `screen.rows()` is used rather than
        // `contents()` because the latter merges wrapped lines.
        let screen_rows: Vec<String> = check_parser.screen().rows(0, cols).collect();
        let status_prefix = format!("{}:", sentinel);
        captured_exit = screen_rows.iter().find_map(|line| {
            if line.contains(&cut_marker) {
                return None;
            }
            line.trim()
                .strip_prefix(&status_prefix)
                .and_then(|rest| rest.trim().parse::<i32>().ok())
        });

        let mut kept = captured_bytes(
            &prompt_bytes,
            &raw_after_prompt,
            capture_start,
            bookkeeping_start,
        );
        let erase = erase_trailing_prompt(&kept, rows, cols, prompt_rows, &prompt_prefix);
        kept.extend_from_slice(&erase);
        kept
    } else {
        // Timed out: there is no bookkeeping line to cut, so keep everything
        // captured so far - still as original bytes.
        captured_bytes(
            &prompt_bytes,
            &raw_after_prompt,
            capture_start,
            raw_after_prompt.len(),
        )
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
/// `cut` is the row holding the trailing prompt's input line. This is the
/// text-based half of the trailing prompt strip; the height-based half in
/// [`erase_trailing_prompt`] covers prompts whose text changed (git state,
/// timers) since it was sampled.
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

/// The screen-clearing escape termshot emits before the captured commands run.
const CLEAR_SCREEN: &[u8] = b"\x1b[2J";

/// Byte offset of the screen-clearing escape in a captured PTY stream, i.e.
/// where the visible capture begins.
///
/// The command that produces it is written as `printf '\033[H\033[2J'` with a
/// *literal* backslash, so the shell's echo of the input line contains no real
/// escape byte and cannot be mistaken for the clear itself.
fn find_clear_sequence(data: &[u8]) -> Option<usize> {
    data.windows(CLEAR_SCREEN.len())
        .position(|window| window == CLEAR_SCREEN)
}

/// Slice the original PTY bytes down to the visible capture: `[capture_start,
/// end)` of `raw`, with title-setting sequences removed (vt100 renders their
/// payload as visible text).
///
/// The bytes are passed through untouched apart from that, so soft wraps,
/// in-place redraws and every escape sequence the commands emitted survive into
/// the renderer exactly as the terminal saw them.
///
/// When the clear escape is missing (the `clear` failed, or output was
/// truncated) the whole session - startup banner included - is kept rather than
/// silently dropping content.
fn captured_bytes(
    prompt_bytes: &[u8],
    raw: &[u8],
    capture_start: Option<usize>,
    end: usize,
) -> Vec<u8> {
    let end = end.min(raw.len());
    match capture_start {
        Some(start) if start < end => {
            // The clear escape is preceded by a cursor-home in the stream;
            // re-emit one so the capture starts at the origin even if the slice
            // began at the erase itself.
            let mut out = Vec::with_capacity(end - start + CURSOR_HOME.len());
            out.extend_from_slice(CURSOR_HOME);
            out.extend_from_slice(&strip_title_sequences(&raw[start..end]));
            out
        }
        _ => {
            let mut out = strip_title_sequences(prompt_bytes);
            out.extend_from_slice(&strip_title_sequences(&raw[..end]));
            out
        }
    }
}

/// Cursor-home escape, emitted at the head of a capture.
const CURSOR_HOME: &[u8] = b"\x1b[H";

/// Bytes that erase the shell's trailing PS1 prompt from an already captured
/// stream, leaving everything above it byte-for-byte untouched.
///
/// `data` ends where termshot's bookkeeping input was about to be echoed, so
/// the cursor rests on the input line of the prompt the shell drew after the
/// last command. Two independent signals decide how far up that prompt starts,
/// and the one that removes more wins:
///
/// * its measured height (`prompt_rows`, sampled on a freshly cleared screen),
///   which still works when the prompt's *text* changed since - a prompt that
///   shows git state changes the moment the captured command stages a file;
/// * its sampled upper rows (`prompt_prefix`), which still work when the prompt
///   wrapped or grew taller than it was when measured.
///
/// Erasing (rather than rewriting the kept rows) is what preserves soft-wrap
/// information: the rows above are never re-emitted, so the wrap flags the
/// terminal set on them survive.
fn erase_trailing_prompt(
    data: &[u8],
    rows: u16,
    cols: u16,
    prompt_rows: usize,
    prompt_prefix: &[String],
) -> Vec<u8> {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(data);
    let screen = parser.screen();

    let cursor_row = screen.cursor_position().0;
    // A long prompt can itself wrap; start from its first physical row.
    let input_row = logical_line_start(screen, cursor_row) as usize;
    let screen_rows: Vec<String> = screen.rows(0, cols).collect();
    let by_height = input_row.saturating_sub(prompt_rows.saturating_sub(1));
    let by_text = strip_trailing_prompt(&screen_rows, input_row, prompt_prefix);
    let first_prompt_row = by_height.min(by_text);

    // Reset the style first: `ESC [ J` erases using the *current* background,
    // so a colored prompt would otherwise leave tinted blank rows behind.
    let mut out = Vec::from(b"\r\x1b[0m".as_slice());
    let up = cursor_row as usize - first_prompt_row.min(cursor_row as usize);
    if up > 0 {
        out.extend_from_slice(format!("\x1b[{}A", up).as_bytes());
    }
    out.extend_from_slice(b"\x1b[J");
    out
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

/// Join the screen rows (each padded to `cols`) into one string, so text a soft
/// wrap split across two rows can still be found.
fn flatten_screen(rows: &[String], cols: u16) -> String {
    let mut flat = String::new();
    for row in rows {
        let mut width = 0usize;
        for ch in row.chars() {
            flat.push(ch);
            width += 1;
        }
        for _ in width..cols as usize {
            flat.push(' ');
        }
    }
    flat
}

/// True once the sentinel's *output* line is on screen.
///
/// The shell command spells the token as two concatenated literals, so the
/// input line the shell echoes back never contains it - only the line the
/// command printed does. The screen is flattened first so a wrapped output
/// line still matches.
fn sentinel_printed(screen: &vt100::Screen, cols: u16, sentinel: &str) -> bool {
    let rows: Vec<String> = screen.rows(0, cols).collect();
    flatten_screen(&rows, cols).contains(sentinel)
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

/// Execute a command non-interactively (no shell prompt).
pub async fn execute_command_simple(
    command: &str,
    shell: &str,
    rows: u16,
    cols: u16,
    timeout: Duration,
) -> Result<ExecResult> {
    execute_command_simple_in_dir(command, shell, rows, cols, timeout, None).await
}

/// Execute a command non-interactively with `working_dir` as the child
/// process's current directory. Passing `None` preserves
/// [`execute_command_simple`] behavior.
pub async fn execute_command_simple_in_dir(
    command: &str,
    shell: &str,
    rows: u16,
    cols: u16,
    timeout: Duration,
    working_dir: Option<&Path>,
) -> Result<ExecResult> {
    let size = pty_process::Size::new(rows, cols);
    let (pty, pts) = pty_process::open().context("Failed to open PTY")?;
    pty.resize(size).context("Failed to resize PTY")?;

    let cmd = pty_process::Command::new(shell)
        .args(["-c", command])
        .env("TERM", "xterm-256color");
    let cmd = match working_dir {
        Some(dir) => cmd.current_dir(dir),
        None => cmd,
    };
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

/// Resolve a CLI or MCP working-directory path before any command executes.
///
/// A leading `~` is expanded for convenience; other relative paths are
/// resolved from the termshot process's current directory. The canonical path
/// is returned so the child shell and fallback filename use the same location.
pub fn resolve_working_directory(path: &Path) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    let expanded = if raw == "~" {
        PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| anyhow::anyhow!("Cannot expand '~': HOME is not set"))?,
        )
    } else if let Some(rest) = raw.strip_prefix("~/") {
        PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| anyhow::anyhow!("Cannot expand '{}': HOME is not set", raw))?,
        )
        .join(rest)
    } else if raw.starts_with('~') {
        anyhow::bail!(
            "Unsupported working directory '{}': only '~' and '~/' expansion are supported",
            raw
        );
    } else {
        path.to_path_buf()
    };

    let resolved = expanded.canonicalize().with_context(|| {
        format!(
            "Working directory '{}' does not exist or cannot be accessed",
            path.display()
        )
    })?;
    if !resolved.is_dir() {
        anyhow::bail!("Working directory '{}' is not a directory", path.display());
    }
    Ok(resolved)
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
) -> Result<bool> {
    let mut buf = [0u8; 4096];
    let mut saw_output = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(saw_output);
        }
        match tokio::time::timeout(quiet_period.min(remaining), reader.read(&mut buf)).await {
            Ok(Ok(0)) => return Ok(saw_output),
            Ok(Ok(n)) => {
                saw_output = true;
                let chunk = &buf[..n];
                raw.extend_from_slice(chunk);
                parser.process(&strip_title_sequences(chunk));
            }
            Ok(Err(e)) => {
                if e.raw_os_error() == Some(5) {
                    return Ok(saw_output);
                }
                return Err(anyhow::anyhow!("PTY read error: {}", e));
            }
            // Quiet period elapsed with no new output.
            Err(_) => return Ok(saw_output),
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
    // Before the shell has echoed or otherwise reacted to the command, the
    // parser still shows the old prompt at `prompt_col`. Under scheduler load,
    // accepting that untouched state after one quiet period races the child
    // and can skip the command entirely.
    let mut saw_command_activity = false;
    loop {
        saw_command_activity |=
            read_until_quiet(reader, raw, parser, QUIET_PERIOD, deadline).await?;
        if saw_command_activity && parser.screen().cursor_position().1 == prompt_col {
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
    let mut saw_output = false;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        let read_timeout = idle_timeout.min(remaining);
        match tokio::time::timeout(read_timeout, reader.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                saw_output = true;
                output.extend_from_slice(&buf[..n]);
            }
            Ok(Err(e)) => {
                if e.raw_os_error() == Some(5) {
                    break;
                }
                return Err(anyhow::anyhow!("PTY read error: {}", e));
            }
            // Before the child emits anything, an idle timeout only means it
            // has not been scheduled yet. Once output has arrived, the same
            // quiet period is the prompt-ready signal.
            Err(_) if saw_output => break,
            Err(_) => continue,
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

    /// Join a parsed screen the way the redaction pass does: physical rows the
    /// terminal soft-wrapped belong to one logical line.
    fn logical_lines_of(screen: &vt100::Screen, cols: u16) -> Vec<String> {
        let rows: Vec<String> = screen
            .rows(0, cols)
            .map(|r| format!("{:width$}", r, width = cols as usize))
            .collect();
        let mut lines = Vec::new();
        let mut idx = 0usize;
        while idx < rows.len() {
            let mut line = String::new();
            loop {
                line.push_str(&rows[idx]);
                if idx + 1 < rows.len() && screen.row_wrapped(idx as u16) {
                    idx += 1;
                } else {
                    break;
                }
            }
            lines.push(line);
            idx += 1;
        }
        lines
    }

    /// Build a synthetic capture buffer shaped like a real PTY session:
    /// startup noise, the echoed `clear` input, the clear escape itself, then
    /// `session`. Returns the startup bytes, the capture buffer, and the offset
    /// where termshot's bookkeeping line would start.
    fn pty_session(session: &str) -> (Vec<u8>, Vec<u8>, usize) {
        let prompt_bytes = b"Welcome to Ubuntu 24.04 LTS\r\nLast login: never\r\n$ ".to_vec();
        let mut raw = Vec::new();
        raw.extend_from_slice(b"clear 2>/dev/null || printf '\\033[H\\033[2J'\r\n");
        raw.extend_from_slice(b"\x1b[H\x1b[2J\x1b[3J");
        raw.extend_from_slice(session.as_bytes());
        let bookkeeping_start = raw.len();
        raw.extend_from_slice(
            b"__TS_EC=$?; echo \"__T\"\"S_abc123__:$__TS_EC\" #TSabc123\r\n__TS_abc123__:0\r\n$ ",
        );
        (prompt_bytes, raw, bookkeeping_start)
    }

    /// Slice + trim a synthetic session exactly the way `execute_command` does.
    fn capture(session: &str, rows: u16, cols: u16, prompt_prefix: &[String]) -> Vec<u8> {
        let (prompt_bytes, raw, bookkeeping_start) = pty_session(session);
        let mut kept = captured_bytes(
            &prompt_bytes,
            &raw,
            find_clear_sequence(&raw),
            bookkeeping_start,
        );
        let prompt_rows = prompt_prefix.len() + 1;
        kept.extend_from_slice(&erase_trailing_prompt(
            &kept,
            rows,
            cols,
            prompt_rows,
            prompt_prefix,
        ));
        kept
    }

    fn screen_rows(data: &[u8], rows: u16, cols: u16) -> Vec<String> {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(data);
        parser
            .screen()
            .rows(0, cols)
            .map(|r| r.trim_end().to_string())
            .collect()
    }

    /// The hash a 32-hex-character redaction rule looks for.
    const SECRET: &str = "8846f7eaee8fb117ad06bdd830b7586c";

    /// The regression this whole rework exists for: a value the terminal
    /// soft-wrapped across two rows must still be *one* logical line after the
    /// capture is sliced. Re-emitting screen rows as hard newlines split it in
    /// two, so redaction masked only the unwrapped occurrence and the leftover
    /// half leaked into the PNG metadata.
    #[test]
    fn capture_preserves_soft_wrapped_values() {
        let session = format!("printf 'Secret hash: {SECRET}\\n'\r\nSecret hash: {SECRET}\r\n$ ",);
        let kept = capture(&session, 10, 40, &[]);

        let mut parser = vt100::Parser::new(10, 40, 0);
        parser.process(&kept);
        let screen = parser.screen();

        // Both the echoed command and the printed output wrapped at 40 columns.
        let plain: Vec<String> = screen.rows(0, 40).collect();
        assert!(
            !plain.iter().any(|r| r.contains(SECRET)),
            "test setup: the secret should be split across rows:\n{:#?}",
            plain
        );

        let logical = logical_lines_of(screen, 40);
        let occurrences = logical.iter().filter(|l| l.contains(SECRET)).count();
        assert_eq!(
            occurrences, 2,
            "both the echoed command and the output must rejoin:\n{:#?}",
            logical
        );
    }

    /// Startup banners are wiped by the `clear`, so the capture starts there -
    /// the returned bytes must not carry the MOTD or the clear's own echoed
    /// input line.
    #[test]
    fn capture_starts_at_the_clear_escape() {
        let kept = capture("echo hi\r\nhi\r\n$ ", 10, 40, &[]);
        let text = String::from_utf8_lossy(&kept);

        assert!(!text.contains("Welcome to Ubuntu"), "startup banner kept");
        assert!(!text.contains("Last login"), "startup banner kept");
        assert!(!text.contains("clear 2>/dev/null"), "clear echo kept");
        assert!(text.contains("echo hi"), "command echo missing:\n{}", text);
    }

    /// Everything from the bookkeeping offset on - the exit-status capture, the
    /// sentinel echo, the sentinel output line - is cut, and so is the trailing
    /// PS1 prompt the shell drew before it.
    #[test]
    fn capture_cuts_bookkeeping_and_trailing_prompt() {
        let kept = capture("whoami\r\nadam\r\n$ ", 10, 40, &[]);
        let text = String::from_utf8_lossy(&kept);
        assert!(!text.contains("__TS_"), "bookkeeping leaked:\n{}", text);
        assert!(!text.contains("#TS"), "nonce marker leaked:\n{}", text);

        let rows = screen_rows(&kept, 10, 40);
        assert_eq!(rows[0], "whoami");
        assert_eq!(rows[1], "adam");
        assert_eq!(rows[2], "", "trailing prompt was not erased: {:?}", rows);
    }

    /// Captured output that merely *looks* like termshot's bookkeeping (a shell
    /// script, `history`, this crate's own source) must never truncate the
    /// capture: the cut is a byte offset recorded before the bookkeeping line
    /// was ever sent, not a search for its text.
    #[test]
    fn capture_ignores_bookkeeping_text_in_command_output() {
        let session = "cat capture.sh\r\n#!/bin/bash\r\n__TS_EC=$?; echo done #TSabc123\r\n\
                       echo 'after the marker'\r\n$ ";
        let kept = capture(session, 10, 60, &[]);
        let rows = screen_rows(&kept, 10, 60);

        assert_eq!(rows[0], "cat capture.sh");
        assert_eq!(rows[2], "__TS_EC=$?; echo done #TSabc123");
        assert_eq!(rows[3], "echo 'after the marker'");
        assert_eq!(rows[4], "", "capture was truncated by its own output");
    }

    /// A multi-line PS1 must be erased whole: its earlier rows would otherwise
    /// dangle under the last command's output.
    #[test]
    fn multiline_trailing_prompt_is_erased_whole() {
        let mut startup = vt100::Parser::new(10, 60, 0);
        startup.process(b"adam@host ~/src\r\n$ ");
        let prefix = prompt_prefix_rows(startup.screen(), 60);
        assert_eq!(prefix, vec!["adam@host ~/src".to_string()]);

        let session = "pwd\r\n/home/adam/src\r\nadam@host ~/src\r\n$ ";
        let kept = capture(session, 10, 60, &prefix);
        let rows = screen_rows(&kept, 10, 60);

        assert_eq!(rows[0], "pwd");
        assert_eq!(rows[1], "/home/adam/src");
        assert_eq!(rows[2], "", "the whole trailing prompt should be erased");
        assert_eq!(rows[3], "");
    }

    /// The measured prompt height alone must erase the trailing prompt even
    /// when its text changed since it was sampled: a prompt that shows git
    /// state changes the moment the captured command stages a file.
    #[test]
    fn trailing_prompt_is_erased_when_its_text_changed() {
        let sampled = vec!["adam@host ~/src (main)".to_string()];
        let session = "git add -A\r\nadam@host ~/src (main*)\r\n$ ";
        let kept = capture(session, 10, 60, &sampled);
        let rows = screen_rows(&kept, 10, 60);

        assert_eq!(rows[0], "git add -A");
        assert_eq!(rows[1], "", "changed trailing prompt must still be erased");
    }

    /// A prompt long enough to wrap ends on a continuation row; the erase must
    /// start at the first physical row of that logical line.
    #[test]
    fn wrapped_trailing_prompt_is_erased_whole() {
        let session = "ls\r\nfile1\r\nadam@host /very/deep/directory/path$ ";
        let kept = capture(session, 10, 20, &[]);
        let rows = screen_rows(&kept, 10, 20);

        assert_eq!(rows[0], "ls");
        assert_eq!(rows[1], "file1");
        assert_eq!(rows[2], "", "wrapped prompt left a fragment: {:?}", rows);
        assert_eq!(rows[3], "");
    }

    /// A colored prompt must not leave tinted blank rows behind: the erase
    /// resets the style first, since `ESC [ J` clears with the current
    /// background.
    #[test]
    fn erasing_a_colored_prompt_leaves_unstyled_cells() {
        let session = "echo hi\r\nhi\r\n\x1b[41m$\x1b[49m ";
        let kept = capture(session, 6, 20, &[]);
        let mut parser = vt100::Parser::new(6, 20, 0);
        parser.process(&kept);
        let screen = parser.screen();
        for col in 0..20 {
            let cell = screen.cell(2, col).expect("cell");
            assert_eq!(cell.bgcolor(), vt100::Color::Default, "col {} tinted", col);
        }
    }

    /// Without the clear escape (a shell where `clear` failed) nothing is
    /// dropped: the whole session, banner included, is kept rather than
    /// silently losing content.
    #[test]
    fn capture_without_a_clear_keeps_everything() {
        let raw = b"echo hi\r\nhi\r\n$ ".to_vec();
        assert_eq!(find_clear_sequence(&raw), None);
        let kept = captured_bytes(b"MOTD\r\n", &raw, None, raw.len());
        let text = String::from_utf8_lossy(&kept);
        assert!(text.starts_with("MOTD"), "banner dropped:\n{}", text);
        assert!(text.contains("echo hi"));
    }

    /// The clear escape is found by its real bytes; the echoed input line that
    /// produced it spells the sequence with literal backslashes and must not
    /// match.
    #[test]
    fn clear_sequence_is_found_by_its_escape_bytes() {
        let raw = b"clear 2>/dev/null || printf '\\033[H\\033[2J'\r\n\x1b[H\x1b[2Jrest";
        let at = find_clear_sequence(raw).expect("clear escape");
        assert_eq!(&raw[at..at + 4], b"\x1b[2J");
        assert!(String::from_utf8_lossy(&raw[at..]).ends_with("rest"));
    }

    /// Title-setting sequences are stripped from the capture: vt100 renders
    /// their payload as visible text.
    #[test]
    fn capture_strips_title_sequences() {
        let kept = capture("ls\r\n\x1bkls\x1b\\file1\r\n$ ", 10, 40, &[]);
        let rows = screen_rows(&kept, 10, 40);
        assert_eq!(rows[0], "ls");
        assert_eq!(rows[1], "file1");
    }

    /// The read loop must not stop on the shell's echo of the sentinel input
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

    /// A sentinel line the terminal wrapped must still be recognized.
    #[test]
    fn sentinel_detection_survives_a_soft_wrap() {
        let mut parser = vt100::Parser::new(10, 12, 0);
        parser.process(b"__TS_abcd12__:0\r\n");
        assert!(sentinel_printed(parser.screen(), 12, "__TS_abcd12__"));
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
