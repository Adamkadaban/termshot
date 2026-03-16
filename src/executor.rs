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

/// Execute a command in an interactive login shell, capturing the PS1 prompt
/// and full output including ANSI escape sequences.
pub async fn execute_command(
    command: &str,
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

    let mut all_output: Vec<u8> = Vec::new();

    // Phase 1: Wait for the initial prompt to appear.
    let prompt_output =
        read_until_idle(&mut reader, Duration::from_millis(500), Duration::from_secs(5)).await?;
    all_output.extend_from_slice(&prompt_output);

    // Phase 2: Type the command.
    let cmd_line = format!("{}\n", command);
    writer
        .write_all(cmd_line.as_bytes())
        .await
        .context("Failed to write command to PTY")?;

    // Phase 3: Read command output until idle or timeout.
    let cmd_output =
        read_until_idle(&mut reader, Duration::from_millis(800), timeout).await;

    let (cmd_bytes, timed_out) = match cmd_output {
        Ok(bytes) => (bytes, false),
        Err(_) => (Vec::new(), true),
    };
    all_output.extend_from_slice(&cmd_bytes);

    // Phase 4: Exit the shell cleanly.
    let _ = writer.write_all(b"exit\n").await;

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
                    // EIO is expected when the child exits on Linux
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
            Err(_) => break, // idle timeout
        }
    }

    Ok(output)
}
