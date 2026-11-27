use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rmcp::handler::server::wrapper::Parameters;
use termshot::backend::Backend;
use termshot::config::Config;
use termshot::renderer::{FontSelection, Renderer, read_png_description};
use termshot::server::{ExecuteAndScreenshotRequest, RedactScreenshotRequest, ScreenshotServer};

struct Case {
    dir: PathBuf,
    config: PathBuf,
}

impl Case {
    fn new(name: &str, config: &str) -> Self {
        let dir = Path::new("target/vte-backend").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!("output_dir = {:?}\n{config}", dir.display().to_string()),
        )
        .unwrap();
        Self {
            dir,
            config: config_path,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_termshot"));
        command
            .arg("--config")
            .arg(&self.config)
            .env_remove("TERMSHOT_BACKEND");
        command
    }

    fn assert_rejected_before_execution(&self, options: &[&str], expected: &str) {
        let marker = self.dir.join("must-not-run");
        if marker.exists() {
            std::fs::remove_file(&marker).unwrap();
        }
        let output = self
            .command()
            .args(["--backend", "vte", "exec", "--no-prompt"])
            .args(options)
            .arg(format!("touch {}", marker.display()))
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{error}");
        assert!(!marker.exists(), "unsupported request executed its command");
    }

    fn assert_success(output: &Output) -> PathBuf {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        assert!(path.exists());
        path
    }
}

#[test]
fn vte_rejects_redaction_and_line_selection_before_execution() {
    let case = Case::new("unsupported", "");
    for (options, expected) in [
        (vec!["--head-lines", "1"], "current viewport"),
        (vec!["--tail-lines", "1"], "current viewport"),
        (vec!["--redact"], "does not support redaction"),
        (vec!["--redact-text"], "does not support redaction"),
        (
            vec!["--redaction", r#"{"pattern":"secret"}"#],
            "does not support redaction",
        ),
    ] {
        case.assert_rejected_before_execution(&options, expected);
    }
    let auto = Case::new("auto-redaction", "[redaction]\nauto = true\n");
    auto.assert_rejected_before_execution(&[], "does not support redaction");
}

#[test]
fn backend_config_env_and_cli_precedence() {
    let case = Case::new("selection", "backend = \"vte\"\n");
    let selected = case
        .command()
        .args(["exec", "--head-lines", "1", "true"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&selected.stderr).contains("current viewport"));
    let env = case
        .command()
        .env("TERMSHOT_BACKEND", "builtin")
        .args(["exec", "--no-prompt", "printf 'builtin via env'"])
        .output()
        .unwrap();
    let path = Case::assert_success(&env);
    assert_eq!(read_png_description(&path).unwrap(), "builtin via env");
    let cli = case
        .command()
        .env("TERMSHOT_BACKEND", "vte")
        .args([
            "--backend",
            "builtin",
            "exec",
            "--no-prompt",
            "printf 'builtin via cli'",
        ])
        .output()
        .unwrap();
    let path = Case::assert_success(&cli);
    assert_eq!(read_png_description(&path).unwrap(), "builtin via cli");
    let invalid = case
        .command()
        .env("TERMSHOT_BACKEND", "unknown")
        .args(["exec", "--no-prompt", "true"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unknown backend"));
    let invalid_config = Case::new("invalid-selection", "backend = \"unknown\"\n");
    let invalid = invalid_config
        .command()
        .args(["exec", "--no-prompt", "true"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("backend"));
}

#[test]
fn unavailable_vte_display_fails_before_execution() {
    let case = Case::new("no-display", "");
    let marker = case.dir.join("must-not-run");
    if marker.exists() {
        std::fs::remove_file(&marker).unwrap();
    }
    let output = case
        .command()
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("GDK_BACKEND", "x11")
        .args(["--backend", "vte", "exec", "--no-prompt"])
        .arg(format!("touch {}", marker.display()))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("VTE"));
    assert!(!marker.exists());
}

fn vte_server(dir: &Path) -> ScreenshotServer {
    let config = Config {
        output_dir: dir.to_path_buf(),
        ..Config::default()
    };
    let renderer = Renderer::new(
        &FontSelection::default(),
        config.font_size,
        &config.themes,
        &config.default_theme,
        &config.chrome,
    )
    .unwrap()
    .with_backend(Backend::Vte);
    ScreenshotServer::new(config, renderer)
}

#[tokio::test]
async fn vte_mcp_rejects_redaction_without_executing_or_rewriting() {
    let case = Case::new("mcp-reject", "");
    let marker = case.dir.join("must-not-run");
    if marker.exists() {
        std::fs::remove_file(&marker).unwrap();
    }
    let server = vte_server(&case.dir);
    let request: ExecuteAndScreenshotRequest = serde_json::from_value(serde_json::json!({
        "command": format!("touch {}", marker.display()),
        "show_prompt": false, "redact": true
    }))
    .unwrap();
    let error = server
        .execute_and_screenshot_tool(Parameters(request))
        .await
        .unwrap_err();
    assert_eq!(error.code.0, -32602);
    assert!(error.message.contains("does not support redaction"));
    assert!(!marker.exists());

    let image = case.dir.join("existing.png");
    std::fs::write(&image, b"must remain unchanged").unwrap();
    let request: RedactScreenshotRequest = serde_json::from_value(serde_json::json!({
        "screenshot_path": image,
        "redactions": [{"pattern": "secret"}]
    }))
    .unwrap();
    let error = server
        .redact_screenshot_tool(Parameters(request))
        .await
        .unwrap_err();
    assert!(error.message.contains("does not support redaction"));
    assert_eq!(std::fs::read(image).unwrap(), b"must remain unchanged");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires GTK4/VTE and a display; run under xvfb-run"]
fn native_vte_viewport_colors_metadata_and_chrome() {
    let case = Case::new("native-render", "");
    let input = case.dir.join("capture.ansi");
    let mut data = String::from("OFFSCREEN_SECRET\r\n");
    for index in 0..20 {
        data.push_str(&format!("old line {index}\r\n"));
    }
    data.push_str("\x1b[31mVISIBLE RED\x1b[0m cafe\u{301}\x1b[H");
    std::fs::write(&input, data).unwrap();
    let output = case
        .command()
        .args([
            "--backend",
            "vte",
            "render",
            "--cols",
            "30",
            "--rows",
            "3",
            "--chrome",
            "gnome",
            "--title",
            "VTE capture",
        ])
        .arg(&input)
        .output()
        .unwrap();
    let path = Case::assert_success(&output);
    let description = read_png_description(&path).unwrap();
    assert!(description.contains("VISIBLE RED"));
    assert!(!description.contains("OFFSCREEN_SECRET"));
    assert!(!description.contains("old line 0"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("earlier output"));
    let image = image::open(path).unwrap().into_rgba8();
    assert!(
        image.pixels().any(|pixel| {
            let [red, green, blue, alpha] = pixel.0;
            alpha == 255
                && red > 100
                && red > green.saturating_mul(2)
                && red > blue.saturating_mul(2)
        }),
        "ANSI red was not drawn"
    );
    assert_eq!(
        image.get_pixel(0, 0).0[3],
        0,
        "window corners should be transparent"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires GTK4/VTE and a display; run under xvfb-run"]
async fn native_vte_exec_and_mcp_capture_use_real_renderer() {
    let case = Case::new("native-exec", "");
    let output = case
        .command()
        .args([
            "--backend",
            "vte",
            "exec",
            "--no-prompt",
            "printf '\\033[32mCLI native\\033[0m'",
        ])
        .output()
        .unwrap();
    let path = Case::assert_success(&output);
    assert_eq!(read_png_description(&path).unwrap(), "CLI native");

    let server = vte_server(&case.dir);
    let request: ExecuteAndScreenshotRequest = serde_json::from_value(serde_json::json!({
        "command": "printf '\\033[34mMCP native\\033[0m'",
        "show_prompt": false, "cols": 30, "rows": 4
    }))
    .unwrap();
    let result = server
        .execute_and_screenshot_tool(Parameters(request))
        .await
        .unwrap();
    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("MCP native"));
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("Screenshot saved to: "))
        .unwrap();
    assert_eq!(read_png_description(Path::new(path)).unwrap(), "MCP native");

    use std::os::unix::fs::PermissionsExt;
    let prompt = Case::new("native-prompt", "");
    let root = std::fs::canonicalize(&prompt.dir).unwrap();
    let rc = root.join("bashrc");
    let shell = root.join("shell.sh");
    std::fs::write(&rc, "PS1='vte-test$ '\n").unwrap();
    std::fs::write(
        &shell,
        format!(
            "#!/bin/sh\nexec /bin/bash --noprofile --rcfile '{}' -i\n",
            rc.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        &prompt.config,
        format!(
            "output_dir = {:?}\nshell = {:?}\n",
            prompt.dir.display().to_string(),
            shell.display().to_string(),
        ),
    )
    .unwrap();
    let output = prompt
        .command()
        .args(["--backend", "vte", "exec", "printf 'interactive native\\n'"])
        .output()
        .unwrap();
    let path = Case::assert_success(&output);
    let text = read_png_description(&path).unwrap();
    assert_eq!(text.matches("vte-test$ ").count(), 1, "{text}");
    assert_eq!(text.lines().last(), Some("interactive native"));
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires GTK4/VTE and a display; run under xvfb-run"]
fn native_vte_handles_empty_incomplete_and_plain_logs_with_explicit_limits() {
    let case = Case::new("native-edges", "");
    for (name, data, expected) in [
        ("empty", b"".as_slice(), ""),
        ("lines", b"one\nTWO\nthree", "one\nTWO\nthree"),
        ("incomplete", b"before\x1b]unfinished", "before"),
    ] {
        let input = case.dir.join(format!("{name}.ansi"));
        std::fs::write(&input, data).unwrap();
        let output = case
            .command()
            .args([
                "--backend",
                "vte",
                "render",
                "--cols",
                "12",
                "--rows",
                "4",
                "--auto-crop",
                "true",
            ])
            .arg(&input)
            .output()
            .unwrap();
        let path = Case::assert_success(&output);
        assert_eq!(
            read_png_description(&path).as_deref(),
            (!expected.is_empty()).then_some(expected)
        );
        let image = image::open(path).unwrap();
        assert!(
            image.width() < 12 * 20 + 64,
            "auto-crop left the full viewport"
        );
    }

    let input = case.dir.join("large.ansi");
    std::fs::write(&input, vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap();
    let output = case
        .command()
        .args(["--backend", "vte", "render"])
        .arg(&input)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("4 MiB"));

    let input = case.dir.join("empty.ansi");
    let output = case
        .command()
        .args([
            "--backend",
            "vte",
            "render",
            "--cols",
            "500",
            "--rows",
            "500",
        ])
        .arg(&input)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("16 megapixels"));
}
