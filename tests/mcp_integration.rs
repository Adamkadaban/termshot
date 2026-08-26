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

use termshot::config::{ChromeConfig, Config};
use termshot::redaction::RedactionConfig;
use termshot::renderer::Renderer;
use termshot::server::{
    ComposeScreenshotsParams, ExecuteAndScreenshotParams, RedactScreenshotParams, RedactionSpec,
    RenderAnsiParams, ScreenshotServer,
};

/// Build an isolated server whose screenshots land in `out_dir`.
fn make_server(out_dir: &Path) -> ScreenshotServer {
    std::fs::create_dir_all(out_dir).unwrap();
    let config = Config {
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
    };
    let renderer = Renderer::new(
        None,
        None,
        config.font_size,
        &config.themes,
        &config.default_theme,
        &config.chrome,
    )
    .expect("renderer");
    ScreenshotServer::new(config, renderer)
}

/// Build a server whose redaction master switch is off.
fn make_server_with_redaction_disabled(out_dir: &Path) -> ScreenshotServer {
    std::fs::create_dir_all(out_dir).unwrap();
    let config = Config {
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
        redaction: RedactionConfig {
            enabled: false,
            ..RedactionConfig::default()
        },
    };
    let renderer = Renderer::new(
        None,
        None,
        config.font_size,
        &config.themes,
        &config.default_theme,
        &config.chrome,
    )
    .expect("renderer");
    ScreenshotServer::new(config, renderer)
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
        redact_text: None,
        show_labels: None,
        strip_ansi: Some(true),
        output_name: None,
        auto_crop: None,
        embed_description: None,
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
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        embed_description: None,
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
        redact_text: Some(true),
        show_labels: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        embed_description: None,
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
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        embed_description: None,
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
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        embed_description: None,
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
    let redact_params = RedactScreenshotParams {
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
        .redact_screenshot(Parameters(redact_params))
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
        redact_text: None,
        show_labels: None,
        embed_description: None,
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
        redact_text: Some(true),
        show_labels: None,
        embed_description: None,
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
        redact_text: None,
        show_labels: None,
        strip_ansi: Some(true),
        output_name: Some("My Custom Shot!".to_string()),
        auto_crop: None,
        embed_description: None,
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
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
        output_name: None,
        auto_crop: None,
        embed_description: None,
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
    let redact_params = RedactScreenshotParams {
        screenshot_path: png.display().to_string(),
        redactions: vec![RedactionSpec::Pattern {
            pattern: r"10\.20\.30\.40".to_string(),
            replacement: Some("[REDACTED-IP]".to_string()),
            keep_prefix: None,
            keep_suffix: None,
        }],
        redact_text: Some(true),
        show_labels: None,
        strip_ansi: None,
    };
    let redacted = server
        .redact_screenshot(Parameters(redact_params))
        .await
        .expect("redact");
    assert!(!result_text(&redacted).contains("10.20.30.40"));

    // A path the server never produced has no record and must error.
    let missing = RedactScreenshotParams {
        screenshot_path: dir.join("nonexistent.png").display().to_string(),
        redactions: vec![RedactionSpec::Pattern {
            pattern: "x".to_string(),
            replacement: None,
            keep_prefix: None,
            keep_suffix: None,
        }],
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
    };
    assert!(server.redact_screenshot(Parameters(missing)).await.is_err());
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
        redact_text: None,
        show_labels: None,
        embed_description: None,
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
