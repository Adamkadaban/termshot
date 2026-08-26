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

use termshot::config::{ChromeConfig, Config, ThemeConfig};
use termshot::redaction::RedactionConfig;
use termshot::renderer::{FontSelection, Renderer};
use termshot::server::{
    ComposeScreenshotsParams, ExecuteAndScreenshotParams, RedactScreenshotParams, RedactionSpec,
    RenderAnsiParams, ScreenshotServer,
};

/// Base configuration for an isolated server whose screenshots land in
/// `out_dir`.
fn base_config(out_dir: &Path) -> Config {
    std::fs::create_dir_all(out_dir).unwrap();
    Config {
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
    }
}

/// Build a server from a config, exactly the way `termshot mcp` does: one
/// renderer that owns a font chain per configured theme.
fn server_from_config(config: Config) -> ScreenshotServer {
    let renderer = Renderer::new(
        &FontSelection {
            global_font: config.font_path.clone(),
            ..FontSelection::default()
        },
        config.font_size,
        &config.themes,
        &config.default_theme,
        &config.chrome,
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
        redact_text: None,
        show_labels: None,
        strip_ansi: Some(true),
        output_name: None,
        auto_crop: None,
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
        redact_text: None,
        show_labels: None,
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
        serde_json::from_value::<RedactScreenshotParams>(serde_json::json!({
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
        redact_text: None,
        show_labels: None,
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
