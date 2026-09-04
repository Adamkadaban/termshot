//! Integration tests that drive the MCP tool handlers programmatically.
//!
//! These exercise the same async functions the MCP server exposes to a client
//! (`execute_and_screenshot`, `render_ansi`, `redact_screenshot`) without a
//! real transport or LLM: the tool methods are called directly with their
//! `Parameters` wrappers, mirroring what an MCP client request would decode to.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;

use termshot::config::{ChromeConfig, Config, LoadedConfig, ThemeConfig};
use termshot::redaction::ManualRedactionSpec;
use termshot::redaction::RedactionConfig;
use termshot::renderer::{FontSelection, Renderer, RendererOptions};
use termshot::server::{
    ComposeScreenshotsParams, ExecuteAndScreenshotParams, ExecuteAndScreenshotRequest,
    RedactScreenshotRequest, RenderAnsiParams, ScreenshotServer,
};
use termshot::shaping::ShapingOptions;

/// Base configuration for an isolated server whose screenshots land in
/// `out_dir`. The scrollback capacity rides in `LoadedConfig`, beside the
/// 1.0.0 `Config`, exactly as `Config::load_with_options` returns it.
fn base_config(out_dir: &Path) -> LoadedConfig {
    std::fs::create_dir_all(out_dir).unwrap();
    LoadedConfig {
        config: Config {
            output_dir: out_dir.to_path_buf(),
            font_path: None,
            font_size: 16.0,
            default_cols: 80,
            default_rows: 24,
            default_timeout_secs: 30,
            shell: "/bin/bash".to_string(),
            embed_description: true,
            default_theme: "dark".to_string(),
            chrome: ChromeConfig::default(),
            themes: HashMap::new(),
            user_theme_names: BTreeSet::new(),
            redaction: RedactionConfig::default(),
        },
        max_scrollback_lines: termshot::capture::DEFAULT_MAX_SCROLLBACK_LINES,
    }
}

/// Build a server from a config, exactly the way `termshot mcp` does: one
/// renderer that owns a font chain per configured theme.
///
/// The one deviation is [`ShapingOptions::deterministic`], which turns off
/// automatic system font discovery. `termshot mcp` leaves it on, but a test
/// that let it run would be asserting on whichever fonts the machine running
/// it happens to have installed - the CJK assertions below in particular.
fn server_from_config(loaded: LoadedConfig) -> ScreenshotServer {
    let LoadedConfig {
        config,
        max_scrollback_lines,
    } = loaded;
    let renderer = Renderer::new_with_shaping(
        &FontSelection {
            global_font: config.font_path.clone(),
            ..FontSelection::default()
        },
        config.font_size,
        &config.themes,
        &config.default_theme,
        &config.chrome,
        RendererOptions::default().with_max_scrollback_lines(max_scrollback_lines),
        ShapingOptions::deterministic(),
    )
    .expect("renderer");
    ScreenshotServer::new(config, renderer)
}

/// Build an isolated server whose screenshots land in `out_dir`.
fn make_server(out_dir: &Path) -> ScreenshotServer {
    server_from_config(base_config(out_dir))
}

/// Build a server whose redaction master switch is off.
fn make_server_with_redaction_disabled(out_dir: &Path) -> ScreenshotServer {
    let mut config = base_config(out_dir);
    config.redaction = RedactionConfig {
        enabled: false,
        ..RedactionConfig::default()
    };
    server_from_config(config)
}

/// Concatenate all text content blocks from a tool result.
fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the screenshot path from a `Screenshot saved to: <path>` line.
fn screenshot_path(text: &str) -> PathBuf {
    for line in text.lines() {
        for prefix in ["Screenshot saved to: ", "Redacted screenshot saved to: "] {
            if let Some(rest) = line.strip_prefix(prefix) {
                return PathBuf::from(rest.trim());
            }
        }
    }
    panic!("no screenshot path in result:\n{}", text);
}

#[tokio::test]
async fn execute_and_screenshot_returns_png_and_text() {
    let dir = Path::new("target/mcp-int/exec");
    let server = make_server(dir);

    let params = ExecuteAndScreenshotParams {
        command: "echo hello-from-mcp".to_string(),
        cols: Some(80),
        rows: Some(10),
        timeout_secs: Some(20),
        show_prompt: Some(false),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: None,
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: Some(true),
        output_name: None,
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };

    let result = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("tool call");
    let text = result_text(&result);

    assert!(text.contains("hello-from-mcp"), "output missing:\n{}", text);
    let png = screenshot_path(&text);
    assert!(png.exists(), "PNG not written: {}", png.display());
    // Fallback name is `{cwd_basename}-{first_command_word}`, and no sidecar
    // files are written.
    let stem = png.file_stem().unwrap().to_string_lossy().into_owned();
    let cwd_base = std::env::current_dir()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    let expected_prefix = format!("{}-echo", cwd_base);
    assert!(
        stem.starts_with(&expected_prefix),
        "unexpected fallback filename: {} (expected prefix {})",
        stem,
        expected_prefix
    );
    assert!(
        !png.with_extension("raw").exists(),
        "unexpected .raw sidecar"
    );
    assert!(
        !png.with_extension("meta.json").exists(),
        "unexpected .meta.json sidecar"
    );
}

#[tokio::test]
async fn execute_request_sets_cwd_before_the_real_prompt_and_strips_the_last_prompt() {
    let dir = Path::new("target/mcp-int/exec-cwd");
    let working_dir = dir.join("short-context");
    std::fs::create_dir_all(&working_dir).unwrap();
    let canonical = working_dir.canonicalize().unwrap();
    let server = make_server(dir);

    let request = ExecuteAndScreenshotRequest {
        command: None,
        commands: Some(vec!["pwd".to_string()]),
        cwd: Some(working_dir.display().to_string()),
        cols: Some(100),
        rows: Some(10),
        timeout_secs: Some(20),
        show_prompt: Some(true),
        theme: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: Some(false),
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: Some(true),
        output_name: Some("exec-cwd".to_string()),
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };

    let result = server
        .execute_and_screenshot_tool(Parameters(request))
        .await
        .expect("cwd request");
    let text = result_text(&result);
    let terminal = text
        .split("--- Terminal Output ---")
        .nth(1)
        .expect("terminal output")
        .trim();

    assert!(
        terminal.contains(canonical.to_string_lossy().as_ref()),
        "pwd did not run in requested directory:\n{terminal}"
    );
    assert!(
        terminal
            .lines()
            .last()
            .unwrap()
            .ends_with(canonical.to_string_lossy().as_ref()),
        "the final real prompt was not stripped:\n{terminal}"
    );
}

#[tokio::test]
async fn execute_request_rejects_bad_cwd_and_command_shapes_before_execution() {
    let dir = Path::new("target/mcp-int/exec-cwd-invalid");
    std::fs::create_dir_all(dir).unwrap();
    let marker = dir.join("must-not-exist");
    let server = make_server(dir);

    let request = |command, commands, cwd| ExecuteAndScreenshotRequest {
        command,
        commands,
        cwd,
        cols: Some(80),
        rows: Some(10),
        timeout_secs: Some(20),
        show_prompt: Some(false),
        theme: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: Some(false),
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: Some(true),
        output_name: Some("invalid-request".to_string()),
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };

    let missing = dir.join("does-not-exist");
    let error = server
        .execute_and_screenshot_tool(Parameters(request(
            Some(format!("touch {}", marker.display())),
            None,
            Some(missing.display().to_string()),
        )))
        .await
        .expect_err("missing cwd must fail");
    assert!(error.message.contains("Working directory"), "{error:?}");
    assert!(!marker.exists(), "command ran despite invalid cwd");

    let result = server
        .execute_and_screenshot_tool(Parameters(request(
            Some(format!("touch {}", marker.display())),
            Some(vec!["echo commands-wins".to_string()]),
            None,
        )))
        .await
        .expect("legacy command plus commands shape remains accepted");
    assert!(
        result_text(&result).contains("commands-wins"),
        "commands did not take precedence"
    );
    assert!(!marker.exists(), "ignored singular command was executed");

    for invalid in [
        request(None, None, None),
        request(None, Some(Vec::new()), None),
        request(None, Some(vec![" ".to_string()]), None),
    ] {
        let error = server
            .execute_and_screenshot_tool(Parameters(invalid))
            .await
            .expect_err("invalid command shape must fail");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }
}

#[tokio::test]
async fn execute_and_screenshot_redacts_image_only_by_default() {
    let dir = Path::new("target/mcp-int/exec-redact-default");
    let server = make_server(dir);

    // redact = true masks the PNG, but with redact_text unset the returned
    // text keeps the ORIGINAL content so the agent can still see it.
    let params = ExecuteAndScreenshotParams {
        command: "echo host 10.11.12.13 online".to_string(),
        cols: Some(80),
        rows: Some(10),
        timeout_secs: Some(20),
        show_prompt: Some(false),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: Some(true),
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };

    let result = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("tool call");
    let text = result_text(&result);

    // Text still contains the original IP (only the image was redacted)...
    assert!(
        text.contains("10.11.12.13"),
        "default text should keep original IP:\n{}",
        text
    );
    // ...and the audit still reports what was masked in the image.
    assert!(
        text.contains("Redacted:"),
        "expected audit summary:\n{}",
        text
    );
}

#[tokio::test]
async fn execute_and_screenshot_redacts_text_when_requested() {
    let dir = Path::new("target/mcp-int/exec-redact-text");
    let server = make_server(dir);

    let params = ExecuteAndScreenshotParams {
        command: "echo host 10.11.12.13 online".to_string(),
        cols: Some(80),
        rows: Some(10),
        timeout_secs: Some(20),
        show_prompt: Some(false),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: Some(true),
        redaction_rules: None,
        redactions: None,
        redact_text: Some(true),
        show_labels: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };

    let result = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("tool call");
    let text = result_text(&result);

    assert!(
        !text.contains("10.11.12.13"),
        "redacted text still leaks IP:\n{}",
        text
    );
    assert!(
        text.contains('\u{2588}'),
        "expected redaction blocks:\n{}",
        text
    );
}

#[tokio::test]
async fn execute_and_screenshot_preserves_ansi_by_default() {
    let dir = Path::new("target/mcp-int/exec-ansi");
    let server = make_server(dir);

    // `ls --color` style output; force a color code via printf.
    let params = ExecuteAndScreenshotParams {
        command: "printf '\\033[31mRED\\033[0m normal\\n'".to_string(),
        cols: Some(80),
        rows: Some(6),
        timeout_secs: Some(20),
        show_prompt: Some(false),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: None,
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };

    let result = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("tool call");
    let text = result_text(&result);

    // Default output preserves ANSI escape codes (ESC = 0x1b).
    assert!(
        text.contains('\u{1b}'),
        "default output should keep ANSI codes:\n{:?}",
        text
    );
    assert!(text.contains("RED"));
}

#[tokio::test]
async fn redact_screenshot_pattern_and_coordinate_flow() {
    let dir = Path::new("target/mcp-int/redact-flow");
    let server = make_server(dir);

    // Step 1: capture WITHOUT redaction so the agent can see the plain text.
    let params = ExecuteAndScreenshotParams {
        command: "echo secret 10.20.30.40 token".to_string(),
        cols: Some(80),
        rows: Some(10),
        timeout_secs: Some(20),
        show_prompt: Some(false),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: None,
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };
    let first = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("exec");
    let first_text = result_text(&first);
    assert!(first_text.contains("10.20.30.40"), "plain text expected");
    let png = screenshot_path(&first_text);

    // Step 2: agent decides to redact the IP by pattern and the word "secret"
    // by coordinates (cols 0..6 on the output row). Request redacted text back.
    let redact_params = RedactScreenshotRequest {
        screenshot_path: png.display().to_string(),
        redactions: vec![
            ManualRedactionSpec::Pattern {
                pattern: r"10\.20\.30\.40".to_string(),
                replacement: Some("[REDACTED-IP]".to_string()),
                keep_prefix: None,
                keep_suffix: None,
                color: None,
            },
            ManualRedactionSpec::Coordinate {
                row: 0,
                col_start: 0,
                col_end: 6,
                label: Some("SECRET".to_string()),
                color: None,
            },
        ],
        redact_text: Some(true),
        show_labels: None,
        strip_ansi: None,
    };
    let redacted = server
        .redact_screenshot_tool(Parameters(redact_params))
        .await
        .expect("redact");
    let redacted_text = result_text(&redacted);
    // Inspect only the terminal-output section: the descriptive filename can
    // legitimately echo the command (redaction masks the image/text, not the
    // path derived from the command line).
    let terminal_out = redacted_text
        .split("--- Terminal Output ---")
        .nth(1)
        .expect("terminal output section");

    assert!(
        !terminal_out.contains("10.20.30.40"),
        "IP still visible after redaction:\n{}",
        terminal_out
    );
    assert!(
        !terminal_out.contains("secret"),
        "coordinate redaction failed:\n{}",
        terminal_out
    );
    assert!(png.exists(), "redacted PNG should still exist");
}

#[tokio::test]
async fn render_ansi_renders_sample_data() {
    let dir = Path::new("target/mcp-int/render-ansi");
    let server = make_server(dir);

    // Sample ANSI: bold red "ERROR" then normal text.
    let ansi = "\x1b[1;31mERROR\x1b[0m something happened\n";
    let input_path = dir.join("sample.ansi");
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&input_path, ansi).unwrap();

    let params = RenderAnsiParams {
        input_path: input_path.display().to_string(),
        cols: Some(80),
        rows: Some(6),
        theme: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        redact: None,
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        head_lines: None,
        tail_lines: None,
    };

    let result = server
        .render_ansi(Parameters(params))
        .await
        .expect("render_ansi");
    let text = result_text(&result);

    assert!(
        text.contains("ERROR"),
        "rendered text missing ERROR:\n{}",
        text
    );
    assert!(
        text.contains("something happened"),
        "rendered text missing body:\n{}",
        text
    );
    let png = screenshot_path(&text);
    assert!(
        png.exists(),
        "render_ansi PNG not written: {}",
        png.display()
    );
}

/// `render_ansi` must honor redaction: rendering a captured log is exactly the
/// case where the caller has not eyeballed the content first.
#[tokio::test]
async fn render_ansi_redacts_when_requested() {
    let dir = Path::new("target/mcp-int/render-ansi-redact");
    let server = make_server(dir);

    let input_path = dir.join("secret.ansi");
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&input_path, "key AKIAIOSFODNN7EXAMPLE end\n").unwrap();

    let params = RenderAnsiParams {
        input_path: input_path.display().to_string(),
        cols: Some(60),
        rows: Some(4),
        theme: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        strip_ansi: Some(true),
        output_name: Some("render-ansi-redacted".to_string()),
        auto_crop: None,
        redact: Some(true),
        redaction_rules: None,
        redactions: None,
        redact_text: Some(true),
        show_labels: None,
        head_lines: None,
        tail_lines: None,
    };

    let result = server
        .render_ansi(Parameters(params))
        .await
        .expect("render_ansi");
    let text = result_text(&result);

    assert!(text.contains("aws_key"), "no redaction audit:\n{}", text);
    assert!(
        !text.contains("AKIAIOSFODNN7EXAMPLE"),
        "redacted text leaked the key:\n{}",
        text
    );
    let png = screenshot_path(&text);
    assert!(png.exists(), "PNG not written: {}", png.display());
}

/// Extract the composed screenshot path from the tool result.
fn composed_path(text: &str) -> PathBuf {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Composed screenshot saved to: ") {
            return PathBuf::from(rest.trim());
        }
    }
    panic!("no composed screenshot path in result:\n{}", text);
}

#[tokio::test]
async fn compose_screenshots_places_images_side_by_side() {
    use image::GenericImageView;

    let dir = Path::new("target/mcp-int/compose");
    let server = make_server(dir);
    std::fs::create_dir_all(dir).unwrap();

    // Two solid-color source PNGs of different sizes.
    let a = dir.join("a.png");
    let b = dir.join("b.png");
    image::RgbaImage::from_pixel(40, 30, image::Rgba([200, 0, 0, 255]))
        .save(&a)
        .unwrap();
    image::RgbaImage::from_pixel(20, 50, image::Rgba([0, 0, 200, 255]))
        .save(&b)
        .unwrap();

    let out = dir.join("combined.png");
    let params = ComposeScreenshotsParams {
        paths: vec![a.display().to_string(), b.display().to_string()],
        layout: Some("horizontal".to_string()),
        divider: Some(16),
        theme: None,
        chrome: None,
        title: None,
        output: Some(out.display().to_string()),
    };

    let result = server
        .compose_screenshots(Parameters(params))
        .await
        .expect("compose_screenshots");
    let text = result_text(&result);
    let png = composed_path(&text);
    assert_eq!(png, out);
    assert!(png.exists(), "composed PNG not written: {}", png.display());

    // Horizontal: width = 40 + 16 + 20 = 76, height = max(30, 50) = 50.
    let composed = image::open(&png).unwrap();
    assert_eq!(composed.dimensions(), (76, 50));
}

#[tokio::test]
async fn compose_screenshots_vertical_auto_output() {
    use image::GenericImageView;

    let dir = Path::new("target/mcp-int/compose-vertical");
    let server = make_server(dir);
    std::fs::create_dir_all(dir).unwrap();

    let a = dir.join("a.png");
    let b = dir.join("b.png");
    image::RgbaImage::from_pixel(40, 30, image::Rgba([200, 0, 0, 255]))
        .save(&a)
        .unwrap();
    image::RgbaImage::from_pixel(20, 50, image::Rgba([0, 0, 200, 255]))
        .save(&b)
        .unwrap();

    let params = ComposeScreenshotsParams {
        paths: vec![a.display().to_string(), b.display().to_string()],
        layout: Some("vertical".to_string()),
        divider: Some(10),
        theme: None,
        chrome: None,
        title: None,
        output: None,
    };

    let result = server
        .compose_screenshots(Parameters(params))
        .await
        .expect("compose_screenshots");
    let text = result_text(&result);
    let png = composed_path(&text);
    assert!(png.exists(), "composed PNG not written: {}", png.display());

    // Vertical: width = max(40, 20) = 40, height = 30 + 10 + 50 = 90.
    let composed = image::open(&png).unwrap();
    assert_eq!(composed.dimensions(), (40, 90));
}

#[tokio::test]
async fn compose_screenshots_requires_two_paths() {
    let dir = Path::new("target/mcp-int/compose-invalid");
    let server = make_server(dir);
    std::fs::create_dir_all(dir).unwrap();

    let a = dir.join("only.png");
    image::RgbaImage::from_pixel(10, 10, image::Rgba([0, 0, 0, 255]))
        .save(&a)
        .unwrap();

    let params = ComposeScreenshotsParams {
        paths: vec![a.display().to_string()],
        layout: None,
        divider: None,
        theme: None,
        chrome: None,
        title: None,
        output: None,
    };

    let err = server.compose_screenshots(Parameters(params)).await;
    assert!(err.is_err(), "single-path compose should fail");
}

#[tokio::test]
async fn execute_and_screenshot_respects_output_name_override() {
    let dir = Path::new("target/mcp-int/exec-name");
    let server = make_server(dir);
    // Ensure a deterministic filename across repeated runs.
    std::fs::remove_file(dir.join("my-custom-shot.png")).ok();

    let params = ExecuteAndScreenshotParams {
        command: "echo hi".to_string(),
        cols: Some(80),
        rows: Some(6),
        timeout_secs: Some(20),
        show_prompt: Some(false),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: None,
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: Some(true),
        output_name: Some("My Custom Shot!".to_string()),
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };

    let result = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("tool call");
    let png = screenshot_path(&result_text(&result));
    assert_eq!(
        png.file_name().unwrap().to_string_lossy(),
        "my-custom-shot.png"
    );
    assert!(png.exists());
}

#[tokio::test]
async fn redact_screenshot_uses_in_memory_record_not_sidecars() {
    let dir = Path::new("target/mcp-int/redact-mem");
    let server = make_server(dir);

    let params = ExecuteAndScreenshotParams {
        command: "echo token 10.20.30.40 end".to_string(),
        cols: Some(80),
        rows: Some(6),
        timeout_secs: Some(20),
        show_prompt: Some(false),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: None,
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };
    let first = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("exec");
    let png = screenshot_path(&result_text(&first));

    // No sidecar files exist on disk.
    assert!(!png.with_extension("raw").exists());
    assert!(!png.with_extension("meta.json").exists());

    // Redaction works purely from the in-memory record.
    let redact_params = RedactScreenshotRequest {
        screenshot_path: png.display().to_string(),
        redactions: vec![ManualRedactionSpec::Pattern {
            pattern: r"10\.20\.30\.40".to_string(),
            replacement: Some("[REDACTED-IP]".to_string()),
            keep_prefix: None,
            keep_suffix: None,
            color: None,
        }],
        redact_text: Some(true),
        show_labels: None,
        strip_ansi: None,
    };
    let redacted = server
        .redact_screenshot_tool(Parameters(redact_params))
        .await
        .expect("redact");
    assert!(!result_text(&redacted).contains("10.20.30.40"));

    // A path the server never produced has no record and must error.
    let missing = RedactScreenshotRequest {
        screenshot_path: dir.join("nonexistent.png").display().to_string(),
        redactions: vec![ManualRedactionSpec::Pattern {
            pattern: "x".to_string(),
            replacement: None,
            keep_prefix: None,
            keep_suffix: None,
            color: None,
        }],
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
    };
    assert!(
        server
            .redact_screenshot_tool(Parameters(missing))
            .await
            .is_err()
    );
}

/// With the master switch off, an explicit `redact: true` must fail loudly
/// rather than quietly return an unredacted screenshot.
#[tokio::test]
async fn explicit_redaction_fails_when_master_switch_is_off() {
    let dir = Path::new("target/mcp-int/redaction-disabled");
    let server = make_server_with_redaction_disabled(dir);

    let input_path = dir.join("secret.ansi");
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&input_path, "key AKIAIOSFODNN7EXAMPLE end\n").unwrap();

    let params = RenderAnsiParams {
        input_path: input_path.display().to_string(),
        cols: Some(60),
        rows: Some(4),
        theme: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        strip_ansi: Some(true),
        output_name: Some("blocked".to_string()),
        auto_crop: None,
        redact: Some(true),
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        head_lines: None,
        tail_lines: None,
    };

    let err = server
        .render_ansi(Parameters(params))
        .await
        .expect_err("explicit redaction must not silently succeed");
    let message = format!("{err}");
    assert!(
        message.contains("disabled in config"),
        "unexpected error: {message}"
    );
}

// -------------------------------------------------------------------------
// Per-theme fonts
// -------------------------------------------------------------------------

/// Stand-in for a real primary font that only covers printable ASCII (no box
/// drawing, no CJK) and uses a wider 0.64 em cell than JetBrains Mono.
const LIMITED_PRIMARY: &str = "tests/fixtures/limited-ascii.ttf";
/// Stand-in for a user-configured fallback font: one CJK character and
/// nothing else.
const CJK_FALLBACK: &str = "tests/fixtures/cjk-fallback.ttf";
/// The one character `cjk-fallback.ttf` covers and no other test font does.
const CJK_CHAR: &str = "\u{4e2d}";

fn theme_config(font: Option<&str>, fallbacks: &[&str]) -> ThemeConfig {
    ThemeConfig {
        foreground: "#ffffff".to_string(),
        background: "#000000".to_string(),
        font: font.map(str::to_string),
        font_bold: None,
        fallback_fonts: fallbacks.iter().map(|f| f.to_string()).collect(),
        palette: std::array::from_fn(|_| "#808080".to_string()),
        base_dir: None,
    }
}

/// Render `ansi` through the MCP `render_ansi` tool with an explicit theme and
/// return the written PNG.
async fn render_with_theme(
    server: &ScreenshotServer,
    dir: &Path,
    name: &str,
    ansi: &str,
    theme: &str,
) -> image::RgbaImage {
    let input_path = dir.join(format!("{}.ansi", name));
    std::fs::write(&input_path, ansi).unwrap();
    std::fs::remove_file(dir.join(format!("{}.png", name))).ok();

    let params = RenderAnsiParams {
        input_path: input_path.display().to_string(),
        cols: Some(10),
        rows: Some(2),
        theme: Some(theme.to_string()),
        chrome: Some("none".to_string()),
        title: None,
        timestamp: None,
        // Square corners and full width keep the pixel comparison about the
        // fonts and nothing else.
        rounded: Some(false),
        strip_ansi: None,
        output_name: Some(name.to_string()),
        auto_crop: Some(false),
        redact: Some(false),
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        head_lines: None,
        tail_lines: None,
    };
    let result = server
        .render_ansi(Parameters(params))
        .await
        .expect("render_ansi");
    let png = screenshot_path(&result_text(&result));
    image::open(&png).expect("composed png opens").to_rgba8()
}

/// Pixels that are not the (black) theme background, i.e. how much ink a glyph
/// actually put on the canvas.
fn ink(img: &image::RgbaImage) -> usize {
    img.pixels().filter(|p| p.0 != [0, 0, 0, 255]).count()
}

/// A theme requested per MCP call must render with THAT theme's fonts: its own
/// primary face (which sets the cell size) and its own fallback chain.
#[tokio::test]
async fn mcp_theme_selects_that_themes_font_and_fallback_chain() {
    let dir = Path::new("target/mcp-int/theme-fonts");
    let mut config = base_config(dir);
    config.themes.insert(
        "ascii-only".to_string(),
        theme_config(Some(LIMITED_PRIMARY), &[]),
    );
    config.themes.insert(
        "cjk".to_string(),
        theme_config(Some(LIMITED_PRIMARY), &[CJK_FALLBACK]),
    );
    // Default theme uses the embedded font, so it must NOT leak into the two
    // themes above (and vice versa).
    config
        .themes
        .insert("embedded".to_string(), theme_config(None, &[]));
    config.default_theme = "embedded".to_string();
    let server = server_from_config(config);

    // 1. Fallback chain: only the `cjk` theme configures a font covering the
    //    CJK character, so only it draws anything for it.
    let with_fallback = render_with_theme(&server, dir, "cjk-theme", CJK_CHAR, "cjk").await;
    let without_fallback =
        render_with_theme(&server, dir, "ascii-theme", CJK_CHAR, "ascii-only").await;
    assert!(
        ink(&with_fallback) > 0,
        "the requested theme's fallback font was not used"
    );
    assert_eq!(
        ink(&without_fallback),
        0,
        "a theme without that fallback must not borrow another theme's font"
    );

    // 2. Primary face: the fixture font has a wider cell than the embedded
    //    JetBrains Mono, so the same content renders wider under it.
    let wide = render_with_theme(&server, dir, "wide-theme", "abc", "ascii-only").await;
    let narrow = render_with_theme(&server, dir, "narrow-theme", "abc", "embedded").await;
    assert!(
        wide.width() > narrow.width(),
        "per-theme primary font ignored: {} vs {} px",
        wide.width(),
        narrow.width()
    );
}

// -------------------------------------------------------------------------
// Unknown MCP parameters
// -------------------------------------------------------------------------

/// `embed_description` is deliberately global config only, and no MCP tool
/// exposes it. A caller that still sends it (or any other unknown field) must
/// get an error rather than a screenshot that silently ignored the request.
#[test]
fn mcp_params_reject_unknown_fields() {
    let err = serde_json::from_value::<ExecuteAndScreenshotParams>(serde_json::json!({
        "command": "echo hi",
        "embed_description": false,
    }))
    .expect_err("unknown field must be rejected");
    assert!(
        err.to_string()
            .contains("unknown field `embed_description`"),
        "unexpected error: {err}"
    );

    for params in [
        serde_json::json!({"input_path": "x.ansi", "embed_description": false}),
        serde_json::json!({"input_path": "x.ansi", "no_such_option": 1}),
    ] {
        assert!(
            serde_json::from_value::<RenderAnsiParams>(params.clone()).is_err(),
            "render_ansi accepted unknown field: {params}"
        );
    }
    assert!(
        serde_json::from_value::<RedactScreenshotRequest>(serde_json::json!({
            "screenshot_path": "x.png",
            "redactions": [],
            "embed_description": true,
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ComposeScreenshotsParams>(serde_json::json!({
            "paths": ["a.png", "b.png"],
            "embed_description": true,
        }))
        .is_err()
    );

    // Known fields still parse.
    serde_json::from_value::<ExecuteAndScreenshotParams>(serde_json::json!({
        "command": "echo hi",
        "cols": 80,
    }))
    .expect("known fields parse");
    serde_json::from_value::<ExecuteAndScreenshotRequest>(serde_json::json!({
        "commands": ["pwd", "git status --short"],
        "cwd": "~/project",
        "cols": 100,
    }))
    .expect("current request fields parse");
    assert!(
        serde_json::from_value::<ExecuteAndScreenshotRequest>(serde_json::json!({
            "command": "echo hi",
            "fake_prompt": true,
        }))
        .is_err(),
        "current request accepted an unknown field"
    );
}

/// The published tool schemas tell clients that extra properties are refused,
/// so the rejection above is discoverable rather than a surprise.
#[test]
fn published_tool_schemas_forbid_additional_properties() {
    let tools = ScreenshotServer::tool_definitions();
    assert!(!tools.is_empty(), "no tools published");
    for tool in tools {
        let schema = serde_json::to_value(&tool.input_schema).expect("schema serializes");
        assert_eq!(
            schema.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false)),
            "tool '{}' allows unknown parameters: {schema}",
            tool.name
        );
    }
}

/// The tool the router publishes is unchanged by the handler split: same four
/// names, and `redact_screenshot` still carries the description its doc comment
/// gives it. Only the Rust entry point moved.
#[test]
fn the_published_tool_set_is_unchanged() {
    let tools = ScreenshotServer::tool_definitions();
    let mut names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "compose_screenshots",
            "execute_and_screenshot",
            "redact_screenshot",
            "render_ansi",
        ]
    );

    let description = tools
        .iter()
        .find(|tool| tool.name == "redact_screenshot")
        .and_then(|tool| tool.description.clone())
        .expect("redact_screenshot is described");
    assert!(
        description.starts_with("Apply selective redactions to a screenshot"),
        "unexpected description: {description}"
    );
    for mentioned in ["keep_prefix", "show_labels", "col_start"] {
        assert!(
            description.contains(mentioned),
            "the description stopped documenting {mentioned}: {description}"
        );
    }

    let execute = tools
        .iter()
        .find(|tool| tool.name == "execute_and_screenshot")
        .expect("execute_and_screenshot is published");
    let execute_description = execute.description.as_deref().unwrap_or_default();
    for guidance in ["real shell prompt", "`cwd`", "Never synthesize"] {
        assert!(
            execute_description.contains(guidance),
            "execute description stopped documenting {guidance}: {execute_description}"
        );
    }
    let execute_schema =
        serde_json::to_value(&execute.input_schema).expect("execute schema serializes");
    let properties = execute_schema["properties"]
        .as_object()
        .expect("execute properties");
    assert!(
        properties.contains_key("cwd"),
        "cwd missing: {execute_schema}"
    );
    assert!(
        properties.contains_key("command") && properties.contains_key("commands"),
        "command forms missing: {execute_schema}"
    );
    assert!(
        !execute_schema["required"]
            .as_array()
            .is_some_and(|required| required.iter().any(|field| field == "command")),
        "schema still requires command even when commands is used: {execute_schema}"
    );
}

/// The `redact_screenshot` schema a live `tools/list` publishes must describe
/// exactly what the parser accepts: every field, both variants, nothing extra.
/// A client builds its call from this schema, so a field the schema hides is a
/// feature nobody can reach and a field it invents is a call that fails.
#[test]
fn published_redaction_schema_matches_the_parser() {
    let (pattern, coordinate) = published_redaction_variants();

    for (variant, expected_fields, expected_required) in [
        (
            &pattern,
            vec![
                "color",
                "keep_prefix",
                "keep_suffix",
                "label",
                "pattern",
                "replacement",
            ],
            vec!["pattern"],
        ),
        (
            &coordinate,
            vec!["col_end", "col_start", "color", "label", "row"],
            vec!["row", "col_start", "col_end"],
        ),
    ] {
        assert_eq!(
            variant.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false)),
            "a redaction variant accepts unknown fields: {variant}"
        );
        assert_eq!(variant["type"], "object", "{variant}");

        let fields: Vec<&str> = variant["properties"]
            .as_object()
            .expect("properties is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(fields, expected_fields, "wrong fields in {variant}");

        let required: Vec<&str> = variant["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .map(|value| value.as_str().expect("a field name"))
            .collect();
        assert_eq!(required, expected_required, "wrong required in {variant}");
    }
}

/// Every field the published schema names is a field the parser accepts, and
/// every one it marks required really is - checked by feeding the schema's own
/// vocabulary back through the decoder the tool uses.
#[test]
fn the_published_schema_and_the_parser_agree_field_by_field() {
    let (pattern, coordinate) = published_redaction_variants();

    // A pattern object carrying every published field but `label` (which is an
    // alias for `replacement`, so the two are mutually exclusive) parses.
    serde_json::from_value::<ManualRedactionSpec>(serde_json::json!({
        "pattern": "x",
        "replacement": "TAG",
        "keep_prefix": 1,
        "keep_suffix": 2,
        "color": "#d41919",
    }))
    .expect("the parser accepts every published pattern field");
    serde_json::from_value::<ManualRedactionSpec>(serde_json::json!({
        "pattern": "x", "label": "TAG",
    }))
    .expect("the parser accepts the published `label` alias");

    serde_json::from_value::<ManualRedactionSpec>(serde_json::json!({
        "row": 0, "col_start": 0, "col_end": 4, "label": "L", "color": "#00ff00",
    }))
    .expect("the parser accepts every published coordinate field");

    // A field the schema does not publish is refused, matching
    // `additionalProperties: false`.
    for variant in [&pattern, &coordinate] {
        let mut object = serde_json::Map::new();
        for field in variant["properties"].as_object().unwrap().keys() {
            // `label` and `replacement` are the same field under two names.
            if field == "label" {
                continue;
            }
            let value = match field.as_str() {
                "pattern" => serde_json::json!("x"),
                "replacement" => serde_json::json!("TAG"),
                "color" => serde_json::json!("#d41919"),
                "keep_prefix" | "keep_suffix" => serde_json::json!(1),
                "col_end" => serde_json::json!(4),
                _ => serde_json::json!(0),
            };
            object.insert(field.clone(), value);
        }
        object.insert("not_a_field".to_string(), serde_json::json!(1));
        let err = serde_json::from_value::<ManualRedactionSpec>(serde_json::Value::Object(object))
            .expect_err("an unpublished field must be refused");
        assert!(
            err.to_string().contains("not_a_field"),
            "the error must name the offending field: {err}"
        );
    }

    // A required field left out is refused too.
    assert!(
        serde_json::from_value::<ManualRedactionSpec>(serde_json::json!({
            "row": 0, "col_start": 0,
        }))
        .is_err(),
        "a coordinate redaction without `col_end` must be refused"
    );
}

/// The two `redact_screenshot` redaction variants, as a live `tools/list`
/// publishes them.
fn published_redaction_variants() -> (serde_json::Value, serde_json::Value) {
    let tool = ScreenshotServer::tool_definitions()
        .into_iter()
        .find(|tool| tool.name == "redact_screenshot")
        .expect("redact_screenshot is published");
    let schema = serde_json::to_value(&tool.input_schema).expect("schema serializes");

    // The array items reference the shared definition the schema carries.
    let reference = schema["properties"]["redactions"]["items"]["$ref"]
        .as_str()
        .expect("redactions items are a $ref")
        .to_string();
    let name = reference
        .strip_prefix("#/$defs/")
        .expect("a local definition");
    let spec = schema["$defs"][name].clone();

    let variants = spec["oneOf"]
        .as_array()
        .expect("the redaction spec is a oneOf of its two variants")
        .clone();
    assert_eq!(variants.len(), 2, "expected exactly two variants: {spec}");
    (variants[0].clone(), variants[1].clone())
}

// -------------------------------------------------------------------------
// Composed image metadata
// -------------------------------------------------------------------------

/// Capture a screenshot with `render_ansi` and return its PNG path.
async fn render_pane(server: &ScreenshotServer, dir: &Path, name: &str, text: &str) -> PathBuf {
    let input_path = dir.join(format!("{}.ansi", name));
    std::fs::write(&input_path, text).unwrap();
    std::fs::remove_file(dir.join(format!("{}.png", name))).ok();
    let params = RenderAnsiParams {
        input_path: input_path.display().to_string(),
        cols: Some(40),
        rows: Some(3),
        theme: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        strip_ansi: None,
        output_name: Some(name.to_string()),
        auto_crop: None,
        redact: Some(false),
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        head_lines: None,
        tail_lines: None,
    };
    let result = server
        .render_ansi(Parameters(params))
        .await
        .expect("render_ansi");
    screenshot_path(&result_text(&result))
}

async fn compose(server: &ScreenshotServer, panes: &[PathBuf], out: &Path) -> PathBuf {
    let params = ComposeScreenshotsParams {
        paths: panes.iter().map(|p| p.display().to_string()).collect(),
        layout: Some("vertical".to_string()),
        divider: Some(2),
        theme: None,
        chrome: None,
        title: None,
        output: Some(out.display().to_string()),
    };
    let result = server
        .compose_screenshots(Parameters(params))
        .await
        .expect("compose_screenshots");
    composed_path(&result_text(&result))
}

/// A composed PNG must carry the panes' `Description` metadata, joined with a
/// clear separator, with Unicode intact.
#[tokio::test]
async fn composed_png_carries_pane_descriptions() {
    let dir = Path::new("target/mcp-int/compose-description");
    let server = make_server(dir);

    let first = render_pane(&server, dir, "pane-one", "héllo wörld ✓\n").await;
    let second = render_pane(&server, dir, "pane-two", "\u{4e2d}\u{6587} λ ─┐\n").await;

    let out = dir.join("described.png");
    let composed = compose(&server, &[first, second], &out).await;

    let description = termshot::renderer::read_png_description(&composed)
        .expect("composed PNG must carry a description");
    assert!(
        description.contains("héllo wörld ✓"),
        "first pane text missing:\n{description}"
    );
    assert!(
        description.contains("\u{4e2d}\u{6587} λ ─┐"),
        "second pane text (Unicode) missing:\n{description}"
    );
    assert!(
        description.contains("--- Pane 2 ---"),
        "panes are not separated:\n{description}"
    );
    // The panes stay in order.
    let first_at = description.find("héllo").unwrap();
    let second_at = description.find('\u{4e2d}').unwrap();
    assert!(first_at < second_at, "panes out of order:\n{description}");
}

/// With `embed_description` off in the server config, a composed image carries
/// no description either - the global setting governs compose too, and there is
/// no per-call MCP toggle.
#[tokio::test]
async fn composed_png_omits_description_when_disabled() {
    let dir = Path::new("target/mcp-int/compose-no-description");
    let mut config = base_config(dir);
    config.embed_description = false;
    let server = server_from_config(config);

    let first = render_pane(&server, dir, "quiet-one", "alpha\n").await;
    let second = render_pane(&server, dir, "quiet-two", "beta\n").await;
    let out = dir.join("quiet.png");
    let composed = compose(&server, &[first, second], &out).await;

    assert!(
        termshot::renderer::read_png_description(&composed).is_none(),
        "composed PNG embedded a description while embed_description is off"
    );
}

/// A 32-hex-character hash: long enough that an 80-column terminal wraps it in
/// the middle when the shell echoes the command that prints it.
const WRAPPED_SECRET: &str = "8846f7eaee8fb117ad06bdd830b7586c";

/// Write a shell whose PS1 is exactly `$ `, and return its path.
///
/// The interactive capture below depends on *where* the echoed command line
/// crosses the right margin, so the prompt has to be the same width on every
/// machine - a developer's own PS1 (git branch, virtualenv, hostname) would
/// otherwise move the wrap point and quietly stop testing the wrap.
fn fixed_prompt_shell(dir: &Path) -> String {
    let path = dir.join("fixed-prompt-shell.sh");
    std::fs::write(
        &path,
        "#!/bin/sh\nexport PS1='$ '\nexec /bin/bash --norc --noprofile \"$@\"\n",
    )
    .expect("write shell wrapper");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod shell wrapper");
    }
    std::fs::canonicalize(&path)
        .expect("canonicalize shell wrapper")
        .display()
        .to_string()
}

/// A server that captures through [`fixed_prompt_shell`].
fn make_server_with_fixed_prompt(out_dir: &Path) -> ScreenshotServer {
    let mut config = base_config(out_dir);
    config.shell = fixed_prompt_shell(out_dir);
    server_from_config(config)
}

/// Coordinates of every pixel painted in the redaction block color.
fn redaction_pixels(png: &Path) -> Vec<(u32, u32)> {
    let img = image::open(png).expect("open png").to_rgba8();
    let mut hits = Vec::new();
    for (x, y, px) in img.enumerate_pixels() {
        if px[0] == 212 && px[1] == 25 && px[2] == 25 && px[3] == 255 {
            hits.push((x, y));
        }
    }
    hits
}

/// Number of contiguous horizontal bands of redaction pixels, i.e. how many
/// separate terminal rows carry a mask.
fn redaction_bands(pixels: &[(u32, u32)]) -> usize {
    let mut ys: Vec<u32> = pixels.iter().map(|&(_, y)| y).collect();
    ys.sort_unstable();
    ys.dedup();
    let mut bands = 0usize;
    let mut prev: Option<u32> = None;
    for y in ys {
        if prev.map(|p| y > p + 1).unwrap_or(true) {
            bands += 1;
        }
        prev = Some(y);
    }
    bands
}

/// End-to-end regression for a secret split by a terminal soft wrap.
///
/// The interactive shell echoes the command it is about to run, and at 80
/// columns that echo wraps the hash across two physical rows while the
/// command's *output* prints it contiguously. Rebuilding the capture as screen
/// rows joined by hard newlines destroyed the terminal's wrap flags, so the
/// redaction pass saw the echoed hash as two unrelated lines: only the output
/// occurrence was masked, and the PNG's `Description` leaked the other one
/// split by a newline.
#[tokio::test]
async fn redact_screenshot_masks_a_secret_split_by_a_soft_wrap() {
    let dir = Path::new("target/mcp-int/redact-soft-wrap");
    let server = make_server_with_fixed_prompt(dir);

    // Step 1: capture in an interactive shell, exactly as the MCP tool does.
    let params = ExecuteAndScreenshotParams {
        command: format!(
            "printf '\\033[1;36mMCP execution test\\033[0m\\nSecret hash: {}\\nStatus: ok\\n'",
            WRAPPED_SECRET
        ),
        cols: Some(80),
        rows: Some(12),
        timeout_secs: Some(30),
        show_prompt: Some(true),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: Some(false),
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
        output_name: Some("soft-wrap-capture".to_string()),
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };
    let captured = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("exec");
    let png = screenshot_path(&result_text(&captured));

    // Test setup: the echoed command really did wrap the hash across two rows,
    // and the output line really does carry it whole.
    let before = termshot::renderer::read_png_description(&png).expect("description");
    assert!(
        before.contains("8846f7eaee8fb117ad06\nbdd830b7586c"),
        "test setup: the echoed hash should be split by a soft wrap:\n{}",
        before
    );
    assert!(
        before.contains(&format!("Secret hash: {}", WRAPPED_SECRET)),
        "test setup: the output line should carry the hash whole:\n{}",
        before
    );

    // Step 2: the agent masks the hash, keeping its first 4 characters.
    let redact_params = RedactScreenshotRequest {
        screenshot_path: png.display().to_string(),
        redactions: vec![ManualRedactionSpec::Pattern {
            pattern: "[a-f0-9]{32}".to_string(),
            replacement: None,
            keep_prefix: Some(4),
            keep_suffix: None,
            color: None,
        }],
        redact_text: Some(true),
        show_labels: None,
        strip_ansi: None,
    };
    let redacted = server
        .redact_screenshot_tool(Parameters(redact_params))
        .await
        .expect("redact");
    let redacted_text = result_text(&redacted);

    // Step 3: both occurrences are reported, masked in the text, and masked in
    // the PNG's embedded description.
    assert!(
        redacted_text.contains("Redacted: 2x custom_0"),
        "expected both occurrences to match:\n{}",
        redacted_text
    );
    let terminal_out = redacted_text
        .split("--- Terminal Output ---")
        .nth(1)
        .expect("terminal output section");
    for leak in [WRAPPED_SECRET, "8846f7eaee8fb117ad06", "bdd830b7586c"] {
        assert!(
            !terminal_out.contains(leak),
            "redacted text still leaks {:?}:\n{}",
            leak,
            terminal_out
        );
    }

    let description = termshot::renderer::read_png_description(&png).expect("description");
    for leak in [WRAPPED_SECRET, "8846f7eaee8fb117ad06", "bdd830b7586c"] {
        assert!(
            !description.contains(leak),
            "PNG description still leaks {:?}:\n{}",
            leak,
            description
        );
    }
    // The kept prefix is still there for both occurrences, so the mask is
    // partial rather than the whole line having been dropped.
    assert_eq!(
        description.matches("8846\u{2588}").count(),
        2,
        "both occurrences should keep their 4-character prefix:\n{}",
        description
    );

    // Step 4: the pixels are masked on both wrapped rows. Only the echoed
    // command reaches the right margin, so redaction ink out there is proof the
    // wrapped occurrence was matched - and the separate band below it is the
    // output occurrence.
    let pixels = redaction_pixels(&png);
    assert!(!pixels.is_empty(), "no redaction blocks were painted");
    let img = image::open(&png).expect("open png");
    let width = image::GenericImageView::dimensions(&img).0;
    assert!(
        pixels.iter().any(|&(x, _)| x as f32 >= 0.75 * width as f32),
        "no redaction ink near the right margin: the wrapped occurrence was missed"
    );
    assert!(
        redaction_bands(&pixels) >= 2,
        "expected masks on both the echoed command and the output line"
    );

    // Step 5: composing the redacted screenshot must not resurrect the secret
    // through the panes' inherited metadata.
    let other = render_pane(&server, dir, "soft-wrap-other", "unrelated pane\n").await;
    let out = dir.join("soft-wrap-composed.png");
    let composed = compose(&server, &[png.clone(), other], &out).await;
    let composed_description =
        termshot::renderer::read_png_description(&composed).expect("composed description");
    for leak in [WRAPPED_SECRET, "8846f7eaee8fb117ad06", "bdd830b7586c"] {
        assert!(
            !composed_description.contains(leak),
            "composed description leaks {:?}:\n{}",
            leak,
            composed_description
        );
    }
    assert!(
        composed_description.contains("unrelated pane"),
        "composed description lost the second pane:\n{}",
        composed_description
    );
}

/// Built-in auto-redaction must recognize a wrapped secret too, not just
/// caller-supplied `redact_screenshot` patterns: the AWS key below is split by
/// the same soft wrap in the echoed command line.
#[tokio::test]
async fn auto_redaction_masks_a_secret_split_by_a_soft_wrap() {
    let dir = Path::new("target/mcp-int/auto-redact-soft-wrap");
    let server = make_server_with_fixed_prompt(dir);

    let params = ExecuteAndScreenshotParams {
        command:
            "printf 'padding padding padding padding padding padding key AKIAIOSFODNN7EXAMPLE\\n'"
                .to_string(),
        cols: Some(80),
        rows: Some(10),
        timeout_secs: Some(30),
        show_prompt: Some(true),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: Some(true),
        redaction_rules: Some(vec!["aws_key".to_string()]),
        redactions: None,
        redact_text: Some(true),
        show_labels: None,
        strip_ansi: None,
        output_name: Some("auto-soft-wrap".to_string()),
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };
    let result = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("exec");
    let text = result_text(&result);
    let png = screenshot_path(&text);

    assert!(
        text.contains("Redacted: 2x aws_key"),
        "auto-redaction should match the wrapped echo and the output:\n{}",
        text
    );
    let description = termshot::renderer::read_png_description(&png).expect("description");
    for leak in ["AKIAIOSFODNN7EXAMPLE", "AKIAIOSFODNN", "N7EXAMPLE"] {
        assert!(
            !description.contains(leak),
            "PNG description leaks {:?}:\n{}",
            leak,
            description
        );
    }
}

/// The wrap-aware join must not run *hard* lines together: two consecutive
/// output lines that would form a match only when concatenated must stay
/// unmatched, or every screenshot would sprout phantom redactions.
#[tokio::test]
async fn hard_newlines_are_not_joined_into_a_match() {
    let dir = Path::new("target/mcp-int/hard-newline-join");
    let server = make_server(dir);

    // Two halves of a 32-character hash, printed on their own lines and far
    // from the right margin, so nothing wrapped.
    let params = RenderAnsiParams {
        input_path: {
            let path = dir.join("halves.ansi");
            std::fs::write(&path, "8846f7eaee8fb117ad06\nbdd830b7586c\n").expect("write");
            path.display().to_string()
        },
        cols: Some(80),
        rows: Some(6),
        theme: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: None,
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: Some(true),
        output_name: Some("halves".to_string()),
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };
    let rendered = server
        .render_ansi(Parameters(params))
        .await
        .expect("render");
    let png = screenshot_path(&result_text(&rendered));

    let redact_params = RedactScreenshotRequest {
        screenshot_path: png.display().to_string(),
        redactions: vec![ManualRedactionSpec::Pattern {
            pattern: "[a-f0-9]{32}".to_string(),
            replacement: None,
            keep_prefix: Some(4),
            keep_suffix: None,
            color: None,
        }],
        redact_text: Some(true),
        show_labels: None,
        strip_ansi: None,
    };
    let redacted = server
        .redact_screenshot_tool(Parameters(redact_params))
        .await
        .expect("redact");
    let text = result_text(&redacted);

    assert!(
        !text.contains("Redacted:"),
        "hard-separated lines were wrongly joined into a match:\n{}",
        text
    );
    let terminal_out = text
        .split("--- Terminal Output ---")
        .nth(1)
        .expect("terminal output section");
    assert!(
        terminal_out.contains("8846f7eaee8fb117ad06"),
        "unmatched text should be untouched:\n{}",
        terminal_out
    );
    assert!(
        !terminal_out.contains('\u{2588}'),
        "nothing should have been masked:\n{}",
        terminal_out
    );
}

// -------------------------------------------------------------------------
// Full-output capture: `rows` is the viewport, not a limit on what is shown
// -------------------------------------------------------------------------

/// How one `render_ansi` call in the tests below is set up. Defaults to a
/// 10x80 viewport, no line selection (so every retained line is rendered), and
/// redaction off.
struct RenderCase<'a> {
    name: &'a str,
    text: &'a str,
    rows: u16,
    cols: u16,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    redact: Option<bool>,
    auto_crop: Option<bool>,
}

impl Default for RenderCase<'_> {
    fn default() -> Self {
        Self {
            name: "case",
            text: "",
            rows: 10,
            cols: 80,
            head_lines: None,
            tail_lines: None,
            redact: Some(false),
            auto_crop: None,
        }
    }
}

/// Write `case.text` to a file and render it with `render_ansi`.
async fn render_ansi_lines(
    server: &ScreenshotServer,
    dir: &Path,
    case: RenderCase<'_>,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let input_path = dir.join(format!("{}.ansi", case.name));
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&input_path, case.text).unwrap();
    let params = RenderAnsiParams {
        input_path: input_path.display().to_string(),
        cols: Some(case.cols),
        rows: Some(case.rows),
        theme: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        strip_ansi: None,
        output_name: Some(case.name.to_string()),
        auto_crop: case.auto_crop,
        redact: case.redact,
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        head_lines: case.head_lines,
        tail_lines: case.tail_lines,
    };
    server.render_ansi(Parameters(params)).await
}

/// A command whose output is far taller than the viewport must still be
/// captured whole: the screenshot - and the `Description` metadata that
/// mirrors it - has to hold the first line as well as the last.
#[tokio::test]
async fn execute_and_screenshot_captures_output_taller_than_the_viewport() {
    let dir = Path::new("target/mcp-int/full-output");
    let server = make_server(dir);

    let params = ExecuteAndScreenshotParams {
        command: "seq 1 200".to_string(),
        cols: Some(80),
        // Ten rows of viewport for two hundred lines of output.
        rows: Some(10),
        timeout_secs: Some(30),
        show_prompt: Some(false),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: Some(false),
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: Some(true),
        output_name: Some("full-output".to_string()),
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    };
    let result = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("execute_and_screenshot");
    let text = result_text(&result);
    let png = screenshot_path(&text);

    let description =
        termshot::renderer::read_png_description(&png).expect("screenshot carries a description");
    let lines: Vec<&str> = description.lines().map(str::trim).collect();
    assert!(
        lines.contains(&"1"),
        "the first line scrolled out of the capture:\n{}",
        &description[..description.len().min(200)]
    );
    assert!(
        lines.contains(&"200"),
        "the last line is missing from the capture"
    );
    assert!(
        text.contains("\n1\n") && text.contains("\n200"),
        "returned text is missing the scrolled-off output"
    );
}

/// `head_lines` shows the first N lines and nothing else.
#[tokio::test]
async fn render_ansi_head_lines_keeps_only_the_first_lines() {
    let dir = Path::new("target/mcp-int/head-lines");
    let server = make_server(dir);
    let input: String = (1..=200).map(|i| format!("line {}\n", i)).collect();

    let result = render_ansi_lines(
        &server,
        dir,
        RenderCase {
            name: "head",
            text: &input,
            head_lines: Some(10),
            ..RenderCase::default()
        },
    )
    .await
    .expect("render_ansi");
    let png = screenshot_path(&result_text(&result));
    let description = termshot::renderer::read_png_description(&png).expect("description");

    let lines: Vec<&str> = description.lines().collect();
    assert_eq!(lines.len(), 10, "expected ten lines, got:\n{description}");
    assert_eq!(lines[0], "line 1");
    assert_eq!(lines[9], "line 10");
    assert!(
        !description.contains("line 11"),
        "head selection leaked later lines:\n{description}"
    );
}

/// `tail_lines` shows the last N lines and nothing else.
#[tokio::test]
async fn render_ansi_tail_lines_keeps_only_the_last_lines() {
    let dir = Path::new("target/mcp-int/tail-lines");
    let server = make_server(dir);
    let input: String = (1..=200).map(|i| format!("line {}\n", i)).collect();

    let result = render_ansi_lines(
        &server,
        dir,
        RenderCase {
            name: "tail",
            text: &input,
            tail_lines: Some(10),
            ..RenderCase::default()
        },
    )
    .await
    .expect("render_ansi");
    let png = screenshot_path(&result_text(&result));
    let description = termshot::renderer::read_png_description(&png).expect("description");

    let lines: Vec<&str> = description.lines().collect();
    assert_eq!(lines.len(), 10, "expected ten lines, got:\n{description}");
    assert_eq!(lines[0], "line 191");
    assert_eq!(lines[9], "line 200");
    assert!(
        !description.contains("line 190"),
        "tail selection leaked earlier lines:\n{description}"
    );
}

/// Asking for both ends at once is a caller error, not a silent preference for
/// one of them.
#[tokio::test]
async fn head_and_tail_lines_are_rejected_together() {
    let dir = Path::new("target/mcp-int/head-tail-conflict");
    let server = make_server(dir);

    let err = render_ansi_lines(
        &server,
        dir,
        RenderCase {
            name: "both",
            text: "one\ntwo\n",
            head_lines: Some(5),
            tail_lines: Some(5),
            ..RenderCase::default()
        },
    )
    .await
    .expect_err("head_lines + tail_lines must be rejected");
    assert!(
        err.message.contains("mutually exclusive"),
        "unhelpful error: {}",
        err.message
    );

    let params = ExecuteAndScreenshotParams {
        command: "echo hi".to_string(),
        cols: Some(80),
        rows: Some(10),
        timeout_secs: Some(30),
        show_prompt: Some(false),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: Some(false),
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        head_lines: Some(5),
        tail_lines: Some(5),
    };
    let err = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect_err("head_lines + tail_lines must be rejected");
    assert!(
        err.message.contains("mutually exclusive"),
        "unhelpful error: {}",
        err.message
    );
}

/// Redaction runs over the whole capture, so a secret that scrolled out of the
/// viewport long ago is masked just like one still on screen - in the image, in
/// the `Description` metadata, and in the audit counts.
#[tokio::test]
async fn redaction_covers_scrollback_and_the_current_viewport() {
    let dir = Path::new("target/mcp-int/scrollback-redaction");
    let server = make_server(dir);

    let mut input = String::from("early key AKIAIOSFODNN7EXAMPLE\n");
    for i in 1..=150 {
        input.push_str(&format!("filler {}\n", i));
    }
    input.push_str("late key AKIAI44QH8DHBEXAMPLE\n");

    let result = render_ansi_lines(
        &server,
        dir,
        RenderCase {
            name: "scrollback-secrets",
            text: &input,
            redact: Some(true),
            ..RenderCase::default()
        },
    )
    .await
    .expect("render_ansi");
    let text = result_text(&result);
    let png = screenshot_path(&text);

    // Two matches: one from the scrollback, one from the current screen.
    assert!(
        text.contains("2x aws_key"),
        "both keys should be redacted, got:\n{}",
        text.lines()
            .find(|l| l.starts_with("Redacted:"))
            .unwrap_or("(no audit line)")
    );

    let description = termshot::renderer::read_png_description(&png).expect("description");
    assert!(
        !description.contains("AKIAIOSFODNN7EXAMPLE"),
        "the scrolled-off key leaked into the PNG metadata"
    );
    assert!(
        !description.contains("AKIAI44QH8DHBEXAMPLE"),
        "the on-screen key leaked into the PNG metadata"
    );
    assert!(
        description.contains('\u{2588}'),
        "nothing was masked in the metadata:\n{description}"
    );
}

/// A composed image carries the descriptions of the panes as they were
/// rendered, so head/tail selections survive into the composite's metadata.
#[tokio::test]
async fn composed_description_preserves_each_panes_selection() {
    let dir = Path::new("target/mcp-int/compose-selection");
    let server = make_server(dir);
    let input: String = (1..=100).map(|i| format!("line {}\n", i)).collect();

    let head = render_ansi_lines(
        &server,
        dir,
        RenderCase {
            name: "sel-head",
            text: &input,
            rows: 5,
            cols: 40,
            head_lines: Some(3),
            ..RenderCase::default()
        },
    )
    .await
    .expect("render_ansi");
    let head_png = screenshot_path(&result_text(&head));
    let tail = render_ansi_lines(
        &server,
        dir,
        RenderCase {
            name: "sel-tail",
            text: &input,
            rows: 5,
            cols: 40,
            tail_lines: Some(3),
            ..RenderCase::default()
        },
    )
    .await
    .expect("render_ansi");
    let tail_png = screenshot_path(&result_text(&tail));

    let out = dir.join("selection.png");
    let composed = compose(&server, &[head_png, tail_png], &out).await;
    let description =
        termshot::renderer::read_png_description(&composed).expect("composed description");

    let (first, second) = description
        .split_once("--- Pane 2 ---")
        .expect("panes are separated");
    assert_eq!(
        first.trim().lines().collect::<Vec<_>>(),
        vec!["line 1", "line 2", "line 3"],
        "first pane lost its head selection:\n{description}"
    );
    assert_eq!(
        second.trim().lines().collect::<Vec<_>>(),
        vec!["line 98", "line 99", "line 100"],
        "second pane lost its tail selection:\n{description}"
    );
}

/// Output that never overflows the viewport must render exactly as before:
/// the capture is the screen, and the image is the same size it always was.
#[tokio::test]
async fn short_output_renders_the_same_as_before() {
    let dir = Path::new("target/mcp-int/short-output");
    let server = make_server(dir);

    let result = render_ansi_lines(
        &server,
        dir,
        RenderCase {
            name: "short",
            text: "alpha\nbeta\n",
            rows: 24,
            ..RenderCase::default()
        },
    )
    .await
    .expect("render_ansi");
    let png = screenshot_path(&result_text(&result));
    let description = termshot::renderer::read_png_description(&png).expect("description");
    assert_eq!(description, "alpha\nbeta");
}

/// `redact_screenshot` re-renders from the same capture the screenshot was made
/// from, so the line selection is preserved and cell coordinates still address
/// the rows the image actually shows.
#[tokio::test]
async fn redact_screenshot_preserves_the_original_line_selection() {
    let dir = Path::new("target/mcp-int/redact-selection");
    let server = make_server(dir);
    let mut input: String = (1..=100).map(|i| format!("line {}\n", i)).collect();
    input.push_str("token AKIAIOSFODNN7EXAMPLE\n");

    let result = render_ansi_lines(
        &server,
        dir,
        RenderCase {
            name: "sel-redact",
            text: &input,
            tail_lines: Some(3),
            ..RenderCase::default()
        },
    )
    .await
    .expect("render_ansi");
    let png = screenshot_path(&result_text(&result));
    assert_eq!(
        termshot::renderer::read_png_description(&png).expect("description"),
        "line 99\nline 100\ntoken AKIAIOSFODNN7EXAMPLE"
    );

    let params = RedactScreenshotRequest {
        screenshot_path: png.display().to_string(),
        redactions: vec![
            ManualRedactionSpec::Pattern {
                pattern: r"AKIA[0-9A-Z]{16}".to_string(),
                replacement: Some("[KEY]".to_string()),
                keep_prefix: None,
                keep_suffix: None,
                color: None,
            },
            // Row 0 of the *selection*, i.e. the first line the image shows.
            ManualRedactionSpec::Coordinate {
                row: 0,
                col_start: 0,
                col_end: 7,
                label: Some("L".to_string()),
                color: None,
            },
        ],
        redact_text: Some(true),
        show_labels: Some(true),
        strip_ansi: Some(true),
    };
    let result = server
        .redact_screenshot_tool(Parameters(params))
        .await
        .expect("redact_screenshot");
    let text = result_text(&result);

    let description = termshot::renderer::read_png_description(&png).expect("description");
    let lines: Vec<&str> = description.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "the tail selection was lost on re-render:\n{description}"
    );
    assert_eq!(
        lines[0],
        "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}"
    );
    assert!(
        !description.contains("AKIAIOSFODNN7EXAMPLE"),
        "the key survived re-rendering:\n{description}"
    );
    assert!(!text.contains("AKIAIOSFODNN7EXAMPLE"));
}

// -------------------------------------------------------------------------
// Coordinate redactions are validated against the cells the render paints
// -------------------------------------------------------------------------

/// Render three short lines and return the screenshot path.
///
/// The capture is 6 rows x 20 columns, but the image is not: the renderer stops
/// one row past the last row with content and (by default) one column past the
/// last column with content, so the pixels are 4 rows x 5 columns. Everything
/// below tests coordinates against *that*.
async fn coordinate_fixture(server: &ScreenshotServer, dir: &Path, name: &str) -> PathBuf {
    coordinate_fixture_cropped(server, dir, name, None).await
}

/// [`coordinate_fixture`] with an explicit `auto_crop` setting.
async fn coordinate_fixture_cropped(
    server: &ScreenshotServer,
    dir: &Path,
    name: &str,
    auto_crop: Option<bool>,
) -> PathBuf {
    let result = render_ansi_lines(
        server,
        dir,
        RenderCase {
            name,
            text: "alpha\nbeta\ngamma\n",
            rows: 6,
            cols: 20,
            auto_crop,
            ..RenderCase::default()
        },
    )
    .await
    .expect("render_ansi");
    screenshot_path(&result_text(&result))
}

/// Apply a single coordinate redaction to `png`.
async fn redact_coordinate(
    server: &ScreenshotServer,
    png: &Path,
    row: u16,
    col_start: u16,
    col_end: u16,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let params = RedactScreenshotRequest {
        screenshot_path: png.display().to_string(),
        redactions: vec![ManualRedactionSpec::Coordinate {
            row,
            col_start,
            col_end,
            label: Some("X".to_string()),
            color: None,
        }],
        redact_text: Some(true),
        show_labels: Some(false),
        strip_ansi: Some(true),
    };
    server.redact_screenshot_tool(Parameters(params)).await
}

/// Decode a rendered PNG into `(width, height, rgba bytes)`.
fn png_pixels(path: &Path) -> (u32, u32, Vec<u8>) {
    let img = image::open(path)
        .unwrap_or_else(|e| panic!("decode {}: {e}", path.display()))
        .to_rgba8();
    (img.width(), img.height(), img.into_raw())
}

/// A row the capture retains but the render never paints is refused, not
/// silently ignored: the caller asked for cells to be covered, and a blank
/// trailing row is trimmed off the image before a single pixel is drawn.
#[tokio::test]
async fn redact_screenshot_rejects_a_trailing_blank_row() {
    let dir = Path::new("target/mcp-int/coord-row-oob");
    let server = make_server(dir);
    let png = coordinate_fixture(&server, dir, "coord-row").await;

    // Rows 3, 4 and 5 are inside the 6-row capture and blank, so the render
    // stops before them: accepting them would report a redaction that never
    // appeared.
    for row in [3u16, 4, 5] {
        let message = redact_coordinate(&server, &png, row, 0, 3)
            .await
            .err()
            .unwrap_or_else(|| panic!("blank row {row} must be refused"))
            .to_string();
        assert!(
            message.contains(&format!("row {row}"))
                && message.contains("past the last rendered row"),
            "unhelpful error: {message}"
        );
        assert!(
            message.contains("renders 3 row(s) x 5 column(s)"),
            "the error must report the rendered dimensions: {message}"
        );
    }

    // A row past the capture entirely is refused the same way.
    assert!(redact_coordinate(&server, &png, 6, 0, 3).await.is_err());
    assert!(redact_coordinate(&server, &png, 5_000, 0, 3).await.is_err());
}

/// Columns are checked against the auto-cropped width, at both ends of the
/// range: the image is trimmed to the rightmost column with content, so the
/// empty ones the capture still holds cannot be redacted.
#[tokio::test]
async fn redact_screenshot_rejects_columns_past_the_rendered_width() {
    let dir = Path::new("target/mcp-int/coord-col-oob");
    let server = make_server(dir);
    let png = coordinate_fixture(&server, dir, "coord-col").await;

    // Inside the 20-column capture, past the 5 columns auto_crop leaves.
    let err = redact_coordinate(&server, &png, 0, 10, 15)
        .await
        .expect_err("a column past the cropped width must be refused");
    let message = err.to_string();
    assert!(
        message.contains("col_start 10") && message.contains("past the last rendered column"),
        "unhelpful error: {message}"
    );
    assert!(
        message.contains("renders 3 row(s) x 5 column(s)"),
        "the error must report the rendered dimensions: {message}"
    );

    // Past the capture altogether.
    let err = redact_coordinate(&server, &png, 0, 20, 25)
        .await
        .expect_err("col_start past the grid must be refused");
    assert!(
        err.to_string().contains("col_start 20"),
        "unhelpful error: {err}"
    );

    // col_start inside the rendered width but col_end past its right edge: the
    // range would only be partly covered, which is exactly the silent failure
    // the check exists to prevent.
    let err = redact_coordinate(&server, &png, 0, 3, 8)
        .await
        .expect_err("col_end past the rendered width must be refused");
    let message = err.to_string();
    assert!(
        message.contains("col_end 8") && message.contains("past the last rendered column boundary"),
        "unhelpful error: {message}"
    );
}

/// An empty or reversed column interval covers nothing, so accepting it would
/// report a redaction that never happened.
#[tokio::test]
async fn redact_screenshot_rejects_an_empty_or_reversed_column_range() {
    let dir = Path::new("target/mcp-int/coord-empty");
    let server = make_server(dir);
    let png = coordinate_fixture(&server, dir, "coord-empty").await;

    for (col_start, col_end) in [(3u16, 3u16), (5, 2)] {
        let err = redact_coordinate(&server, &png, 0, col_start, col_end)
            .await
            .err()
            .unwrap_or_else(|| panic!("an empty range ({col_start}..{col_end}) must be refused"));
        let message = err.to_string();
        assert!(
            message.contains(&format!("col_start {}", col_start))
                && message.contains("the range is empty"),
            "unhelpful error for {col_start}..{col_end}: {message}"
        );
    }
}

/// The same column that auto_crop puts out of reach is redactable when the
/// screenshot was rendered without it: the image really is the full grid then,
/// so the cell really is painted.
#[tokio::test]
async fn a_column_past_the_crop_is_accepted_without_auto_crop() {
    let dir = Path::new("target/mcp-int/coord-no-crop");
    let server = make_server(dir);
    let png = coordinate_fixture_cropped(&server, dir, "coord-no-crop", Some(false)).await;

    let before = png_pixels(&png);
    let result = redact_coordinate(&server, &png, 0, 10, 15)
        .await
        .expect("without auto_crop the full grid width is rendered");
    let text = result_text(&result);
    assert!(
        text.contains("Redacted: 1x manual"),
        "the audit must count the redaction: {text}"
    );

    let after = png_pixels(&png);
    assert_eq!(
        (before.0, before.1),
        (after.0, after.1),
        "the re-render must keep the same geometry"
    );
    assert_ne!(
        before.2, after.2,
        "no pixels were painted for the redaction"
    );

    // Trailing blank rows are trimmed whatever auto_crop says, so the row bound
    // is unchanged.
    let err = redact_coordinate(&server, &png, 3, 0, 3)
        .await
        .expect_err("a blank trailing row is trimmed even without auto_crop");
    assert!(
        err.to_string().contains("renders 3 row(s) x 20 column(s)"),
        "unhelpful error: {err}"
    );
}

/// The boundary itself is valid: the last rendered row and a range ending
/// exactly at the last rendered column are accepted, counted in the audit, and
/// actually painted.
#[tokio::test]
async fn redact_screenshot_accepts_the_last_rendered_cell() {
    let dir = Path::new("target/mcp-int/coord-boundary");
    let server = make_server(dir);
    let png = coordinate_fixture(&server, dir, "coord-boundary").await;

    // Last rendered row (2 of 3) and last rendered column (4 of 5) - the very
    // last cell the image draws.
    let before = png_pixels(&png);
    let result = redact_coordinate(&server, &png, 2, 4, 5)
        .await
        .expect("the last rendered row and column are inside the image");
    let text = result_text(&result);
    assert!(text.contains("Redacted screenshot saved to"));
    assert!(
        text.contains("Redacted: 1x manual"),
        "the audit must count the redaction: {text}"
    );

    let after = png_pixels(&png);
    assert_eq!((before.0, before.1), (after.0, after.1));
    assert_ne!(
        before.2, after.2,
        "the last visible cell was accepted but never painted"
    );

    // Row 0, columns 0..5, covers "alpha" and really removes it.
    let result = redact_coordinate(&server, &png, 0, 0, 5)
        .await
        .expect("a range inside the rendered image is accepted");
    let text = result_text(&result);
    let terminal_out = text
        .split("--- Terminal Output ---")
        .nth(1)
        .expect("terminal output section");
    assert!(
        !terminal_out.contains("alpha"),
        "the covered cells were not masked: {terminal_out}"
    );
    assert!(terminal_out.contains("beta"));

    let description = termshot::renderer::read_png_description(&png).expect("description");
    assert!(
        description.starts_with("\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}"),
        "the image no longer shows the mask: {description}"
    );
}

/// A pattern redaction is matched against the capture, not against the manual
/// coordinate bounds, so it still reaches the rightmost content cell - and the
/// continuation cell of a double-width character sitting there.
#[tokio::test]
async fn pattern_redaction_still_reaches_the_rightmost_content_cell() {
    let dir = Path::new("target/mcp-int/coord-pattern-edge");
    let server = make_server(dir);
    let result = render_ansi_lines(
        &server,
        dir,
        RenderCase {
            name: "pattern-edge",
            // The secret ends the longest line, so its last cell is the
            // rightmost column auto_crop keeps; the wide characters below it
            // push the crop out to a continuation cell.
            text: "user AKIAIOSFODNN7EXAMPLE\n\u{5e83}\u{5e83}\u{5e83}\n",
            rows: 6,
            cols: 40,
            ..RenderCase::default()
        },
    )
    .await
    .expect("render_ansi");
    let png = screenshot_path(&result_text(&result));

    let params = RedactScreenshotRequest {
        screenshot_path: png.display().to_string(),
        redactions: vec![ManualRedactionSpec::Pattern {
            pattern: "AKIA[0-9A-Z]{16}".to_string(),
            replacement: None,
            keep_prefix: None,
            keep_suffix: None,
            color: None,
        }],
        redact_text: Some(true),
        show_labels: Some(false),
        strip_ansi: Some(true),
    };
    let result = server
        .redact_screenshot_tool(Parameters(params))
        .await
        .expect("pattern redaction");
    let text = result_text(&result);
    assert!(
        text.contains("Redacted: 1x custom_0"),
        "the pattern must still match: {text}"
    );
    assert!(
        !text.contains("AKIAIOSFODNN7EXAMPLE"),
        "the key survived: {text}"
    );

    let description = termshot::renderer::read_png_description(&png).expect("description");
    assert!(
        description.contains(&"\u{2588}".repeat(20)),
        "the whole key must be masked in the image: {description}"
    );
    assert!(
        description.contains('\u{5e83}'),
        "the wide characters must survive: {description}"
    );
}

/// Coordinates are validated against the *selected* capture as it is rendered,
/// not the PTY viewport: a `tail_lines` screenshot is three rows tall no matter
/// how much output scrolled past, and auto_crop narrows it to the width of the
/// lines it shows.
#[tokio::test]
async fn coordinate_bounds_follow_the_rendered_line_selection() {
    let dir = Path::new("target/mcp-int/coord-selection");
    let server = make_server(dir);
    let input: String = (1..=300).map(|i| format!("line {}\n", i)).collect();

    let result = render_ansi_lines(
        &server,
        dir,
        RenderCase {
            name: "coord-sel",
            text: &input,
            rows: 10,
            cols: 40,
            tail_lines: Some(3),
            ..RenderCase::default()
        },
    )
    .await
    .expect("render_ansi");
    let png = screenshot_path(&result_text(&result));

    let err = redact_coordinate(&server, &png, 3, 0, 4)
        .await
        .expect_err("row 3 is past a three-row selection");
    let message = err.to_string();
    assert!(
        message.contains("renders 3 row(s) x 8 column(s)"),
        "the bounds must describe the rendered selection, not the viewport: {message}"
    );

    // The last row of the selection is in bounds.
    redact_coordinate(&server, &png, 2, 0, 4)
        .await
        .expect("row 2 is the last row of a three-row selection");
}

/// The parameters published in 1.0.0 still drive the tool: an old caller
/// converts them with one `.into()` and gets exactly the redaction it always
/// got.
#[tokio::test]
async fn the_1_0_0_redact_parameters_still_drive_the_tool() {
    use termshot::server::{RedactScreenshotParams, RedactionSpec};

    let dir = Path::new("target/mcp-int/redact-1-0-0-params");
    let server = make_server(dir);

    let input_path = dir.join("legacy-params.ansi");
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&input_path, "secret 10.20.30.40 end\n").unwrap();
    let rendered = server
        .render_ansi(Parameters(RenderAnsiParams {
            input_path: input_path.display().to_string(),
            cols: Some(40),
            rows: Some(3),
            theme: None,
            chrome: None,
            title: None,
            timestamp: None,
            rounded: None,
            strip_ansi: None,
            output_name: Some("legacy-params".to_string()),
            auto_crop: None,
            redact: Some(false),
            redaction_rules: None,
            redactions: None,
            redact_text: None,
            show_labels: None,
            head_lines: None,
            tail_lines: None,
        }))
        .await
        .expect("render_ansi");
    let png = screenshot_path(&result_text(&rendered));

    // Written exactly as a 1.0.0 caller would have written it: no `color`, no
    // spread, `Vec<RedactionSpec>`.
    let legacy = RedactScreenshotParams {
        screenshot_path: png.display().to_string(),
        redactions: vec![
            RedactionSpec::Pattern {
                pattern: r"10\.20\.30\.40".to_string(),
                replacement: Some("[REDACTED-IP]".to_string()),
                keep_prefix: None,
                keep_suffix: None,
            },
            RedactionSpec::Coordinate {
                row: 0,
                col_start: 0,
                col_end: 6,
                label: Some("SECRET".to_string()),
            },
        ],
        redact_text: Some(true),
        show_labels: None,
        strip_ansi: None,
    };

    let redacted = server
        .redact_screenshot(Parameters(legacy))
        .await
        .expect("the 1.0.0 method still takes the 1.0.0 parameters");
    let text = result_text(&redacted);
    let terminal = text
        .split("--- Terminal Output ---")
        .nth(1)
        .expect("terminal output");
    assert!(!terminal.contains("10.20.30.40"), "{terminal}");
    assert!(!terminal.contains("secret"), "{terminal}");
}

// -------------------------------------------------------------------------
// Inline manual redactions on execute_and_screenshot / render_ansi
// -------------------------------------------------------------------------

/// `execute_and_screenshot` parameters with everything at its default, so a
/// test only spells out what it is about.
fn exec_params(command: &str, output_name: &str) -> ExecuteAndScreenshotParams {
    ExecuteAndScreenshotParams {
        command: command.to_string(),
        cols: Some(80),
        rows: Some(10),
        timeout_secs: Some(30),
        show_prompt: Some(false),
        theme: None,
        commands: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        redact: None,
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
        output_name: Some(output_name.to_string()),
        auto_crop: None,
        head_lines: None,
        tail_lines: None,
    }
}

/// `render_ansi` parameters with everything at its default.
fn render_params(input_path: &Path, output_name: &str) -> RenderAnsiParams {
    RenderAnsiParams {
        input_path: input_path.display().to_string(),
        cols: Some(80),
        rows: Some(10),
        theme: None,
        chrome: None,
        title: None,
        timestamp: None,
        rounded: None,
        strip_ansi: None,
        output_name: Some(output_name.to_string()),
        auto_crop: None,
        redact: None,
        redaction_rules: None,
        redactions: None,
        redact_text: None,
        show_labels: None,
        head_lines: None,
        tail_lines: None,
    }
}

/// Write an ANSI capture file for `render_ansi`.
fn ansi_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("create dir");
    let path = dir.join(name);
    std::fs::write(&path, contents).expect("write ansi file");
    path
}

/// Decode manual redaction specifications from the JSON an MCP client sends,
/// through the same parser the tool schema describes.
fn specs(json: &[&str]) -> Vec<ManualRedactionSpec> {
    json.iter()
        .map(|spec| ManualRedactionSpec::from_json(spec).expect("shared parser"))
        .collect()
}

/// Every pixel painted in `color`.
fn pixels_of_color(png: &Path, color: [u8; 3]) -> usize {
    image::open(png)
        .unwrap_or_else(|e| panic!("open {}: {e}", png.display()))
        .to_rgba8()
        .pixels()
        .filter(|px| px[0] == color[0] && px[1] == color[1] && px[2] == color[2])
        .count()
}

/// The `Redacted: ...` audit line of a tool result.
fn audit_line(text: &str) -> String {
    text.lines()
        .find(|line| line.starts_with("Redacted: "))
        .unwrap_or_else(|| panic!("no audit summary in:\n{}", text))
        .to_string()
}

/// Test 1: an inline pattern on `execute_and_screenshot` masks the image
/// without any `redact` flag, keeps its prefix, and paints its own color.
#[tokio::test]
async fn execute_applies_an_inline_pattern_with_keep_prefix_and_color() {
    let dir = Path::new("target/mcp-int/inline-exec-pattern");
    let server = make_server(dir);

    let mut params = exec_params(
        "printf 'hash 8846f7eaee8fb117ad06bdd830b7586c\\n'",
        "inline-exec-pattern",
    );
    params.redactions = Some(specs(&[
        r##"{"pattern":"[a-f0-9]{32}","replacement":"HASH","keep_prefix":4,"color":"#ff6600"}"##,
    ]));
    params.redact_text = Some(true);

    let result = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("inline redactions need no `redact` flag");
    let text = result_text(&result);
    let png = screenshot_path(&text);

    assert_eq!(audit_line(&text), "Redacted: 1x custom_0");
    assert!(
        !text.contains("8846f7eaee8fb117ad06bdd830b7586c"),
        "the returned text still leaks the hash:\n{text}"
    );

    let description = termshot::renderer::read_png_description(&png).expect("description");
    assert!(
        description.contains(&format!("8846{}", "\u{2588}".repeat(28))),
        "the prefix should survive and the rest be masked: {description}"
    );

    assert!(
        pixels_of_color(&png, [0xff, 0x66, 0x00]) > 0,
        "the requested block color was never painted"
    );
    assert_eq!(
        pixels_of_color(&png, [212, 25, 25]),
        0,
        "the default red was painted instead of the requested color"
    );
}

/// Test 2: `render_ansi` takes the same specifications inline, including
/// `keep_suffix`.
#[tokio::test]
async fn render_ansi_applies_an_inline_pattern_with_keep_suffix() {
    let dir = Path::new("target/mcp-int/inline-render-pattern");
    let input = ansi_file(
        dir,
        "suffix.ansi",
        "hash 8846f7eaee8fb117ad06bdd830b7586c\n",
    );
    let server = make_server(dir);

    let mut params = render_params(&input, "inline-render-pattern");
    params.redactions = Some(specs(&[
        r#"{"pattern":"[a-f0-9]{32}","replacement":"HASH","keep_suffix":4}"#,
    ]));
    params.redact_text = Some(true);

    let result = server
        .render_ansi(Parameters(params))
        .await
        .expect("render_ansi takes inline redactions");
    let text = result_text(&result);
    let png = screenshot_path(&text);

    assert_eq!(audit_line(&text), "Redacted: 1x custom_0");
    let description = termshot::renderer::read_png_description(&png).expect("description");
    assert!(
        description.contains(&format!("{}586c", "\u{2588}".repeat(28))),
        "the suffix should survive and the rest be masked: {description}"
    );
    assert!(
        !description.contains("8846f7eaee8fb117ad06bdd830b7586c"),
        "the description still leaks the hash: {description}"
    );
}

/// Test 3: several specifications apply in one call, in order - patterns first,
/// then cell ranges, each with its own label and color.
#[tokio::test]
async fn inline_redactions_apply_several_specs_in_one_call() {
    let dir = Path::new("target/mcp-int/inline-multiple");
    let input = ansi_file(
        dir,
        "multi.ansi",
        "host 10.20.30.40 key AKIAIOSFODNN7EXAMPLE\nhash 8846f7eaee8fb117ad06bdd830b7586c\n",
    );
    let server = make_server(dir);

    let mut params = render_params(&input, "inline-multiple");
    params.redactions = Some(specs(&[
        r#"{"pattern":"[a-f0-9]{32}","replacement":"HASH","keep_prefix":4}"#,
        r#"{"pattern":"AKIA[0-9A-Z]{16}","replacement":"AWS"}"#,
        r##"{"row":0,"col_start":0,"col_end":4,"label":"SECRET","color":"#00ff00"}"##,
    ]));
    params.redact_text = Some(true);

    let result = server
        .render_ansi(Parameters(params))
        .await
        .expect("render_ansi");
    let text = result_text(&result);
    let png = screenshot_path(&text);

    assert_eq!(
        audit_line(&text),
        "Redacted: 1x custom_0, 1x custom_1, 1x manual:SECRET"
    );
    let description = termshot::renderer::read_png_description(&png).expect("description");
    assert!(
        !description.contains("AKIAIOSFODNN7EXAMPLE")
            && !description.contains("8846f7eaee8fb117ad06bdd830b7586c"),
        "a specification was dropped: {description}"
    );
    assert_eq!(
        description.lines().next().unwrap(),
        format!(
            "{} 10.20.30.40 key {}",
            "\u{2588}".repeat(4),
            "\u{2588}".repeat(20)
        ),
        "the coordinate range should mask exactly the first four cells: {description}"
    );
    assert!(
        pixels_of_color(&png, [0x00, 0xff, 0x00]) > 0,
        "the coordinate redaction's own color was never painted"
    );
}

/// Inline specifications compose with the built-in rules rather than replacing
/// them: `redact: true` runs both.
#[tokio::test]
async fn inline_redactions_compose_with_the_builtin_rules() {
    let dir = Path::new("target/mcp-int/inline-plus-rules");
    let input = ansi_file(
        dir,
        "both.ansi",
        "host 10.20.30.40\nhash 8846f7eaee8fb117ad06bdd830b7586c\n",
    );
    let server = make_server(dir);

    let mut params = render_params(&input, "inline-plus-rules");
    params.redact = Some(true);
    params.redaction_rules = Some(vec!["ipv4".to_string()]);
    params.redactions = Some(specs(&[
        r#"{"pattern":"[a-f0-9]{32}","replacement":"HASH"}"#,
    ]));
    params.redact_text = Some(true);

    let result = server
        .render_ansi(Parameters(params))
        .await
        .expect("render_ansi");
    let text = result_text(&result);
    let png = screenshot_path(&text);

    let audit = audit_line(&text);
    assert!(
        audit.contains("1x ipv4") && audit.contains("1x custom_0"),
        "rule-based and manual redactions must both run: {audit}"
    );
    let description = termshot::renderer::read_png_description(&png).expect("description");
    assert!(
        !description.contains("10.20.30.40")
            && !description.contains("8846f7eaee8fb117ad06bdd830b7586c"),
        "something went unmasked: {description}"
    );
}

/// Test 4: a coordinate specification addresses the rendered image, and one
/// outside it is refused with the dimensions it should have used.
#[tokio::test]
async fn inline_coordinates_are_validated_against_the_rendered_image() {
    let dir = Path::new("target/mcp-int/inline-coordinates");
    let input = ansi_file(dir, "coords.ansi", "alpha bravo\ncharlie delta\n");
    let server = make_server(dir);

    let mut params = render_params(&input, "inline-coordinates");
    params.redactions = Some(specs(&[r#"{"row":1,"col_start":0,"col_end":7}"#]));
    params.redact_text = Some(true);
    let result = server
        .render_ansi(Parameters(params))
        .await
        .expect("an in-bounds range applies");
    let text = result_text(&result);
    let png = screenshot_path(&text);
    assert_eq!(audit_line(&text), "Redacted: 1x manual:REDACTED");
    let description = termshot::renderer::read_png_description(&png).expect("description");
    assert_eq!(
        description.lines().nth(1).unwrap(),
        format!("{} delta", "\u{2588}".repeat(7)),
        "the range should mask `charlie` only: {description}"
    );

    // The image renders exactly the two rows of content - the blank row the
    // cursor rests on is trimmed - so row 3 is not addressable.
    let mut params = render_params(&input, "inline-coordinates-oob");
    params.redactions = Some(specs(&[r#"{"row":3,"col_start":0,"col_end":4}"#]));
    let err = server
        .render_ansi(Parameters(params))
        .await
        .expect_err("a row past the rendered image must be refused");
    let message = format!("{err}");
    assert!(
        message.contains("renders 2 row(s) x 13 column(s)"),
        "the error must name the rendered bounds: {message}"
    );

    // A column past the auto-cropped width is refused the same way.
    let mut params = render_params(&input, "inline-coordinates-wide");
    params.redactions = Some(specs(&[r#"{"row":0,"col_start":0,"col_end":40}"#]));
    let err = server
        .render_ansi(Parameters(params))
        .await
        .expect_err("a column past the cropped width must be refused");
    assert!(
        format!("{err}").contains("past the last rendered column"),
        "unexpected error: {err}"
    );

    // An empty interval is refused the same way, before anything is drawn.
    let mut params = render_params(&input, "inline-coordinates-empty");
    params.redactions = Some(specs(&[r#"{"row":0,"col_start":4,"col_end":4}"#]));
    let err = server
        .render_ansi(Parameters(params))
        .await
        .expect_err("an empty range must be refused");
    assert!(
        format!("{err}").contains("the range is empty"),
        "unexpected error: {err}"
    );
}

/// Test 5: asking for manual masking and for redaction to be off at once is
/// refused, exactly as the CLI refuses `--redaction` with `--no-redact`.
#[tokio::test]
async fn inline_redactions_conflict_with_redact_false() {
    let dir = Path::new("target/mcp-int/inline-conflict");
    let input = ansi_file(
        dir,
        "conflict.ansi",
        "hash 8846f7eaee8fb117ad06bdd830b7586c\n",
    );
    let server = make_server(dir);

    let inline = specs(&[r#"{"pattern":"[a-f0-9]{32}"}"#]);

    let mut params = exec_params("printf 'hi\\n'", "inline-conflict-exec");
    params.redact = Some(false);
    params.redactions = Some(inline.clone());
    let err = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect_err("execute_and_screenshot must refuse the conflict");
    let message = format!("{err}");
    assert!(
        message.contains("conflicts with `redact: false`"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("--no-redact"),
        "the error should point at the CLI equivalent: {message}"
    );

    let mut params = render_params(&input, "inline-conflict-render");
    params.redact = Some(false);
    params.redactions = Some(inline);
    let err = server
        .render_ansi(Parameters(params))
        .await
        .expect_err("render_ansi must refuse the conflict too");
    assert!(
        format!("{err}").contains("conflicts with `redact: false`"),
        "unexpected error: {err}"
    );

    // Nothing to conflict with: an empty list leaves `redact: false` alone.
    let mut params = render_params(&input, "inline-conflict-empty");
    params.redact = Some(false);
    params.redactions = Some(Vec::new());
    let result = server
        .render_ansi(Parameters(params))
        .await
        .expect("an empty list is not a request to redact");
    assert!(
        !result_text(&result).contains("Redacted: "),
        "nothing should have been redacted"
    );
}

/// An invalid regex is refused before the command runs, not after.
#[tokio::test]
async fn an_invalid_inline_pattern_is_refused_before_execution() {
    let dir = Path::new("target/mcp-int/inline-bad-regex");
    let server = make_server(dir);
    let marker = dir.join("ran.txt");
    std::fs::create_dir_all(dir).expect("create dir");
    std::fs::remove_file(&marker).ok();

    let mut params = exec_params(&format!("touch {}", marker.display()), "inline-bad-regex");
    params.redactions = Some(specs(&[r#"{"pattern":"([a-"}"#]));
    let err = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect_err("an invalid regex must be refused");
    assert!(
        format!("{err}").contains("invalid regex"),
        "unexpected error: {err}"
    );
    assert!(
        !marker.exists(),
        "the command ran despite the invalid redaction"
    );
}

/// `show_labels: false` draws plain blocks for inline redactions too.
#[tokio::test]
async fn inline_redactions_honor_show_labels() {
    let dir = Path::new("target/mcp-int/inline-show-labels");
    let input = ansi_file(
        dir,
        "labels.ansi",
        "hash 8846f7eaee8fb117ad06bdd830b7586c\n",
    );
    let server = make_server(dir);

    let mut labelled = render_params(&input, "inline-labels-on");
    labelled.redactions = Some(specs(&[
        r#"{"pattern":"[a-f0-9]{32}","replacement":"HASH"}"#,
    ]));
    let with_labels = screenshot_path(&result_text(
        &server
            .render_ansi(Parameters(labelled))
            .await
            .expect("render_ansi"),
    ));

    let mut plain = render_params(&input, "inline-labels-off");
    plain.show_labels = Some(false);
    plain.redactions = Some(specs(&[
        r#"{"pattern":"[a-f0-9]{32}","replacement":"HASH"}"#,
    ]));
    let without_labels = screenshot_path(&result_text(
        &server
            .render_ansi(Parameters(plain))
            .await
            .expect("render_ansi"),
    ));

    assert!(
        pixels_of_color(&without_labels, [212, 25, 25])
            > pixels_of_color(&with_labels, [212, 25, 25]),
        "a plain block should carry more block color than a labelled one"
    );
}

/// Test 7: `tools/list` publishes `redactions` on the capture tools too,
/// described by exactly the shared `ManualRedactionSpec` the redact tool
/// publishes.
#[test]
fn the_capture_tools_publish_the_shared_redaction_schema() {
    let reference = redaction_spec_definition("redact_screenshot");
    for tool in ["execute_and_screenshot", "render_ansi"] {
        let schema = tool_schema(tool);
        let redactions = &schema["properties"]["redactions"];
        assert_eq!(
            redactions["type"],
            serde_json::json!(["array", "null"]),
            "'{tool}' does not publish `redactions` as an optional array: {redactions}"
        );
        assert_eq!(
            redactions["items"]["$ref"], "#/$defs/ManualRedactionSpec",
            "'{tool}' does not reference the shared specification: {redactions}"
        );
        assert_eq!(
            redaction_spec_definition(tool),
            reference,
            "'{tool}' publishes a different specification than redact_screenshot"
        );
    }

    // The shared definition itself: two mutually exclusive variants, each
    // closed to anything it does not name.
    let variants = reference["oneOf"]
        .as_array()
        .expect("the specification is a oneOf of its two variants");
    assert_eq!(variants.len(), 2, "expected two variants: {reference}");
    for (variant, fields, required) in [
        (
            &variants[0],
            vec![
                "color",
                "keep_prefix",
                "keep_suffix",
                "label",
                "pattern",
                "replacement",
            ],
            vec!["pattern"],
        ),
        (
            &variants[1],
            vec!["col_end", "col_start", "color", "label", "row"],
            vec!["row", "col_start", "col_end"],
        ),
    ] {
        assert_eq!(
            variant.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false)),
            "a published variant accepts unknown fields: {variant}"
        );
        let published: Vec<&str> = variant["properties"]
            .as_object()
            .expect("properties is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(published, fields, "wrong fields in {variant}");
        let published_required: Vec<&str> = variant["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .map(|value| value.as_str().expect("a field name"))
            .collect();
        assert_eq!(published_required, required, "wrong required in {variant}");
    }
}

/// Test 8: the capture tools still refuse anything they do not publish - a
/// stray top-level parameter, and an unknown field inside a redaction.
#[test]
fn inline_redactions_do_not_open_the_door_to_unknown_fields() {
    // Known: a `redactions` array decodes through the shared parser.
    serde_json::from_value::<ExecuteAndScreenshotParams>(serde_json::json!({
        "command": "echo hi",
        "redactions": [{"pattern": "[a-f0-9]{32}", "replacement": "HASH", "keep_prefix": 4,
                        "color": "#ff6600"}],
    }))
    .expect("the live MCP request from the report must decode");
    serde_json::from_value::<RenderAnsiParams>(serde_json::json!({
        "input_path": "x.ansi",
        "redactions": [{"row": 0, "col_start": 0, "col_end": 4, "label": "SECRET"}],
    }))
    .expect("render_ansi takes the same array");

    // Unknown, at the top level and inside a specification.
    for params in [
        serde_json::json!({"command": "echo hi", "redaction": []}),
        serde_json::json!({"command": "echo hi", "embed_description": false}),
    ] {
        assert!(
            serde_json::from_value::<ExecuteAndScreenshotParams>(params.clone()).is_err(),
            "execute_and_screenshot accepted unknown field: {params}"
        );
    }
    for bad in [
        serde_json::json!({"pattern": "x", "not_a_field": 1}),
        serde_json::json!({"row": 0, "col_start": 0}),
        serde_json::json!({"replacement": "TAG"}),
    ] {
        let err = serde_json::from_value::<ExecuteAndScreenshotParams>(serde_json::json!({
            "command": "echo hi",
            "redactions": [bad],
        }))
        .expect_err("a malformed redaction must be refused");
        assert!(
            !err.to_string().is_empty(),
            "the error must explain the refusal"
        );
    }
    assert!(
        serde_json::from_value::<RenderAnsiParams>(serde_json::json!({
            "input_path": "x.ansi",
            "redactions": [{"row": 0, "col_start": 0, "col_end": 4, "nope": true}],
        }))
        .is_err(),
        "render_ansi accepted an unknown redaction field"
    );
}

/// The published input schema of one tool.
fn tool_schema(name: &str) -> serde_json::Value {
    let tool = ScreenshotServer::tool_definitions()
        .into_iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("{name} is published"));
    serde_json::to_value(&tool.input_schema).expect("schema serializes")
}

/// The `ManualRedactionSpec` definition a tool's schema carries.
fn redaction_spec_definition(name: &str) -> serde_json::Value {
    tool_schema(name)["$defs"]["ManualRedactionSpec"].clone()
}

/// An inline pattern catches a secret split by a soft wrap, exactly as the
/// CLI's `--redaction` does: the match is found on the capture, not on the
/// drawn rows.
#[tokio::test]
async fn an_inline_pattern_catches_a_secret_split_by_a_soft_wrap() {
    let dir = Path::new("target/mcp-int/inline-soft-wrap");
    let server = make_server(dir);

    // At 40 columns the hash crosses the right margin onto the next row.
    let mut params = exec_params(
        "printf 'padding padding padding hash 8846f7eaee8fb117ad06bdd830b7586c\\n'",
        "inline-soft-wrap",
    );
    params.cols = Some(40);
    params.redactions = Some(specs(&[r#"{"pattern":"[a-f0-9]{32}","keep_prefix":4}"#]));
    params.redact_text = Some(true);

    let result = server
        .execute_and_screenshot(Parameters(params))
        .await
        .expect("exec");
    let text = result_text(&result);
    let png = screenshot_path(&text);
    assert_eq!(audit_line(&text), "Redacted: 1x custom_0");

    let description = termshot::renderer::read_png_description(&png).expect("description");
    for leak in [
        "8846f7eaee8fb117ad06bdd830b7586c",
        "8846f7eaee8fb117ad06",
        "bdd830b7586c",
    ] {
        assert!(
            !description.contains(leak),
            "the description still leaks {leak:?}: {description}"
        );
    }

    // The mask reaches the right margin, so the wrapped half was covered too.
    let width = image::open(&png).expect("open png").to_rgba8().width();
    let pixels = redaction_pixels(&png);
    assert!(!pixels.is_empty(), "no redaction block was painted");
    assert!(
        pixels.iter().any(|&(x, _)| x as f32 >= 0.75 * width as f32),
        "no redaction ink near the right margin: the wrapped half was missed"
    );
}

/// Inline coordinates follow a `head_lines` / `tail_lines` selection, the way
/// the CLI's do: row 0 is the first row the image shows, and the bounds are the
/// selection's.
#[tokio::test]
async fn inline_coordinates_follow_a_head_or_tail_selection() {
    let dir = Path::new("target/mcp-int/inline-selection");
    let content: String = (1..=300).map(|i| format!("line {}\n", i)).collect();
    let input = ansi_file(dir, "many.ansi", &content);
    let server = make_server(dir);

    let mut params = render_params(&input, "inline-tail");
    params.cols = Some(40);
    params.tail_lines = Some(3);
    params.redactions = Some(specs(&[r#"{"row":0,"col_start":0,"col_end":8}"#]));
    params.redact_text = Some(true);
    let result = server
        .render_ansi(Parameters(params))
        .await
        .expect("render_ansi");
    let text = result_text(&result);
    let png = screenshot_path(&text);
    assert_eq!(audit_line(&text), "Redacted: 1x manual:REDACTED");

    let description = termshot::renderer::read_png_description(&png).expect("description");
    let lines: Vec<&str> = description.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "the tail selection renders 3 rows: {lines:?}"
    );
    assert_eq!(
        lines[0],
        "\u{2588}".repeat(8),
        "row 0 must be the first tail row"
    );
    assert_eq!(lines[1], "line 299");

    let mut params = render_params(&input, "inline-tail-oob");
    params.cols = Some(40);
    params.tail_lines = Some(3);
    params.redactions = Some(specs(&[r#"{"row":3,"col_start":0,"col_end":4}"#]));
    let err = server
        .render_ansi(Parameters(params))
        .await
        .expect_err("row 3 is past a three-row selection");
    assert!(
        format!("{err}").contains("renders 3 row(s) x 8 column(s)"),
        "the bounds must describe the selection, not the capture: {err}"
    );
}
