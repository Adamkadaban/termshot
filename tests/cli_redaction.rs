//! CLI / MCP redaction parity.
//!
//! `termshot exec` and `termshot render` take the same manual redaction
//! specifications as the MCP `redact_screenshot` tool, through a repeatable
//! `--redaction '<JSON>'` option. These tests drive the real binary (so the
//! argument parsing, the JSON decoding, and the exit codes are the ones a user
//! gets) and check the results in the pixels and in the PNG's `Description`
//! metadata, then compare them against the equivalent MCP call.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::{fs::Permissions, os::unix::fs::PermissionsExt};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;

use termshot::config::{ChromeConfig, Config, LoadedConfig, ThemeConfig};
use termshot::redaction::{ManualRedactionSpec, RedactionConfig};
use termshot::renderer::{FontSelection, Renderer, RendererOptions, read_png_description};
use termshot::server::{RedactScreenshotRequest, RenderAnsiParams, ScreenshotServer};

/// The `termshot` binary this test run built.
const BIN: &str = env!("CARGO_BIN_EXE_termshot");

/// A test's isolated working area: an output directory plus a config file that
/// points termshot at it, so nothing depends on the user's `~/.config`.
struct Case {
    dir: PathBuf,
    config: PathBuf,
}

impl Case {
    fn new(name: &str) -> Self {
        let dir = Path::new("target/cli-redaction").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create case dir");
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!("output_dir = {:?}\n", dir.display().to_string()),
        )
        .expect("write config");
        Self { dir, config }
    }

    /// Run the CLI with this case's config, returning the raw process output.
    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(BIN);
        command.arg("--config").arg(&self.config);
        command.args(args);
        command.output().expect("run termshot")
    }

    /// Run the CLI and require success, returning `(screenshot path, stderr)`.
    fn run_ok(&self, args: &[&str]) -> (PathBuf, String) {
        let output = self.run(args);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "termshot {:?} failed:\n{}",
            args,
            stderr
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let path = PathBuf::from(stdout.trim());
        assert!(
            path.exists(),
            "no screenshot at {}:\n{}",
            path.display(),
            stderr
        );
        (path, stderr)
    }

    /// Run the CLI and require failure, returning the combined message.
    fn run_err(&self, args: &[&str]) -> String {
        let output = self.run(args);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            !output.status.success(),
            "termshot {:?} unexpectedly succeeded:\n{}",
            args,
            stderr
        );
        stderr
    }

    /// Write an ANSI capture file for `termshot render`.
    fn ansi_file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, contents).expect("write ansi file");
        path
    }

    /// Install an isolated interactive Bash with a deterministic prompt and
    /// point this case's config at it. This keeps prompt/echo tests independent
    /// of the developer or CI runner's shell configuration.
    fn use_deterministic_shell(&self) {
        let root = std::env::current_dir()
            .expect("current dir")
            .join(&self.dir);
        let rc = root.join("bashrc");
        let wrapper = root.join("shell.sh");
        std::fs::write(&rc, "PS1='termshot-test$ '\n").expect("write bashrc");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec /bin/bash --noprofile --rcfile '{}' -i\n",
                rc.display()
            ),
        )
        .expect("write shell wrapper");
        std::fs::set_permissions(&wrapper, Permissions::from_mode(0o755))
            .expect("make shell wrapper executable");
        std::fs::write(
            &self.config,
            format!(
                "output_dir = {:?}\nshell = {:?}\n",
                self.dir.display().to_string(),
                wrapper.display().to_string()
            ),
        )
        .expect("write deterministic config");
    }
}

/// Every pixel painted in the default redaction red.
fn redaction_pixels(png: &Path) -> Vec<(u32, u32)> {
    color_pixels(png, [212, 25, 25])
}

/// Every pixel painted in `color`.
fn color_pixels(png: &Path, color: [u8; 3]) -> Vec<(u32, u32)> {
    let img = image::open(png)
        .unwrap_or_else(|e| panic!("open {}: {e}", png.display()))
        .to_rgba8();
    img.enumerate_pixels()
        .filter(|(_, _, px)| px[0] == color[0] && px[1] == color[1] && px[2] == color[2])
        .map(|(x, y, _)| (x, y))
        .collect()
}

/// The text embedded in the PNG's `Description` chunk.
fn description(png: &Path) -> String {
    read_png_description(png)
        .unwrap_or_else(|| panic!("no Description metadata in {}", png.display()))
}

/// The `Redacted: ...` audit line the CLI prints to stderr.
fn audit_line(stderr: &str) -> String {
    stderr
        .lines()
        .find(|line| line.starts_with("Redacted: "))
        .unwrap_or_else(|| panic!("no audit summary in:\n{}", stderr))
        .to_string()
}

// -------------------------------------------------------------------------
// 1. exec: pattern + keep_prefix
// -------------------------------------------------------------------------

/// A hash masked from the command line keeps its first four characters, in the
/// image and in the text the screenshot carries for screen readers.
#[test]
fn exec_pattern_with_keep_prefix_masks_pixels_and_metadata() {
    let case = Case::new("exec-keep-prefix");
    let (png, stderr) = case.run_ok(&[
        "exec",
        "--no-prompt",
        "--cols",
        "70",
        "--rows",
        "8",
        "--redact-text",
        "--redaction",
        r#"{"pattern":"[a-f0-9]{32}","replacement":"HASH","keep_prefix":4}"#,
        "echo hash 8846f7eaee8fb117ad06bdd830b7586c",
    ]);

    assert_eq!(audit_line(&stderr), "Redacted: 1x custom_0");
    assert!(
        !stderr.contains("8846f7eaee8fb117ad06bdd830b7586c"),
        "the printed text still leaks the hash:\n{}",
        stderr
    );

    let description = description(&png);
    assert!(
        !description.contains("8846f7eaee8fb117ad06bdd830b7586c"),
        "the PNG description still leaks the hash: {description}"
    );
    assert!(
        description.contains("8846\u{2588}"),
        "the kept prefix should survive the mask: {description}"
    );
    assert_eq!(
        description.matches('\u{2588}').count(),
        28,
        "28 of the 32 hash characters should be masked: {description}"
    );
    assert!(
        !redaction_pixels(&png).is_empty(),
        "no redaction block was painted"
    );
}

// -------------------------------------------------------------------------
// 2. render: pattern + keep_suffix
// -------------------------------------------------------------------------

/// `render` takes the same specifications, including `keep_suffix`.
#[test]
fn render_pattern_with_keep_suffix_keeps_the_tail_visible() {
    let case = Case::new("render-keep-suffix");
    let input = case.ansi_file("key.ansi", "user AKIAIOSFODNN7EXAMPLE end\n");

    let (png, stderr) = case.run_ok(&[
        "render",
        "--cols",
        "60",
        "--rows",
        "6",
        "--redact-text",
        "--redaction",
        r#"{"pattern":"AKIA[0-9A-Z]{16}","keep_suffix":4}"#,
        input.to_str().unwrap(),
    ]);

    assert_eq!(audit_line(&stderr), "Redacted: 1x custom_0");
    let description = description(&png);
    assert!(
        !description.contains("AKIAIOSFODNN7EXAMPLE"),
        "the key survived: {description}"
    );
    assert!(
        description.contains(&format!("{}MPLE", "\u{2588}".repeat(16))),
        "the last four characters should stay visible: {description}"
    );
    assert!(!redaction_pixels(&png).is_empty());
}

// -------------------------------------------------------------------------
// 3. Repeated specifications
// -------------------------------------------------------------------------

/// Several `--redaction` options apply together, in order, alongside the
/// built-in rules when `--redact` is also given.
#[test]
fn multiple_redactions_apply_in_order_and_alongside_builtin_rules() {
    let case = Case::new("render-multiple");
    let input = case.ansi_file(
        "multi.ansi",
        "host 10.20.30.40 key AKIAIOSFODNN7EXAMPLE ticket TCK-99\n",
    );

    let (png, stderr) = case.run_ok(&[
        "render",
        "--cols",
        "80",
        "--rows",
        "6",
        "--redact",
        "--redact-text",
        "--redaction",
        r#"{"pattern":"AKIA[0-9A-Z]{16}","replacement":"KEY"}"#,
        "--redaction",
        r#"{"pattern":"TCK-[0-9]+","replacement":"TICKET"}"#,
        input.to_str().unwrap(),
    ]);

    let audit = audit_line(&stderr);
    for rule in ["custom_0", "custom_1", "ipv4"] {
        assert!(audit.contains(rule), "{rule} missing from audit: {audit}");
    }

    let description = description(&png);
    for secret in ["10.20.30.40", "AKIAIOSFODNN7EXAMPLE", "TCK-99"] {
        assert!(
            !description.contains(secret),
            "{secret} survived: {description}"
        );
    }
    // Each specification masked exactly its own match, in the order given.
    assert_eq!(
        description.trim_end(),
        format!(
            "host {} key {} ticket {}",
            "\u{2588}".repeat(11),
            "\u{2588}".repeat(20),
            "\u{2588}".repeat(6)
        ),
        "the three redactions should cover exactly their matches"
    );
}

// -------------------------------------------------------------------------
// 4. Coordinate specification
// -------------------------------------------------------------------------

/// A coordinate range covers exactly the requested cells, in the color it asks
/// for, and is counted in the audit under its label.
#[test]
fn coordinate_redaction_is_validated_and_painted() {
    let case = Case::new("render-coordinate");
    let input = case.ansi_file("coord.ansi", "secret value here\nsecond line\n");

    let (png, stderr) = case.run_ok(&[
        "render",
        "--cols",
        "40",
        "--rows",
        "6",
        "--redact-text",
        "--redaction",
        r##"{"row":0,"col_start":0,"col_end":6,"label":"SECRET","color":"#00ff00"}"##,
        input.to_str().unwrap(),
    ]);

    assert_eq!(audit_line(&stderr), "Redacted: 1x manual:SECRET");
    let description = description(&png);
    assert!(
        description.starts_with(&"\u{2588}".repeat(6)),
        "the first six cells should be masked: {description}"
    );
    assert!(
        description.contains("value here"),
        "the rest of the row must survive: {description}"
    );
    assert!(
        !color_pixels(&png, [0, 255, 0]).is_empty(),
        "the per-redaction color was not used"
    );
    assert!(
        redaction_pixels(&png).is_empty(),
        "nothing should have been painted in the default red"
    );
}

// -------------------------------------------------------------------------
// 5. Out-of-bounds coordinates
// -------------------------------------------------------------------------

/// A cell the image never paints is refused, with the rendered dimensions in
/// the message - the same rule the MCP tool applies.
#[test]
fn out_of_bounds_coordinates_are_rejected() {
    let case = Case::new("render-coordinate-oob");
    let input = case.ansi_file("oob.ansi", "abc\n");

    let message = case.run_err(&[
        "render",
        "--cols",
        "40",
        "--rows",
        "6",
        "--redaction",
        r#"{"row":9,"col_start":0,"col_end":3}"#,
        input.to_str().unwrap(),
    ]);
    assert!(
        message.contains("row 9 is past the last rendered row"),
        "unhelpful error: {message}"
    );
    assert!(
        message.contains("renders 2 row(s) x 3 column(s)"),
        "the error must name the rendered bounds: {message}"
    );

    // A column past the auto-cropped width is refused the same way.
    let message = case.run_err(&[
        "render",
        "--cols",
        "40",
        "--rows",
        "6",
        "--redaction",
        r#"{"row":0,"col_start":0,"col_end":30}"#,
        input.to_str().unwrap(),
    ]);
    assert!(
        message.contains("col_end 30 is past the last rendered column boundary"),
        "unhelpful error: {message}"
    );

    // An empty range covers nothing, so it is an error rather than a no-op.
    let message = case.run_err(&[
        "render",
        "--redaction",
        r#"{"row":0,"col_start":2,"col_end":2}"#,
        input.to_str().unwrap(),
    ]);
    assert!(
        message.contains("the range is empty"),
        "unhelpful error: {message}"
    );
}

// -------------------------------------------------------------------------
// 6. Malformed specifications
// -------------------------------------------------------------------------

/// Invalid JSON, an unknown field, an invalid regex, and a bad color are all
/// refused before anything is rendered.
#[test]
fn malformed_specifications_are_rejected() {
    let case = Case::new("render-malformed");
    let input = case.ansi_file("plain.ansi", "nothing sensitive\n");
    let path = input.to_str().unwrap().to_string();

    let cases: [(&str, &str); 6] = [
        (r#"{"pattern":"#, "invalid redaction JSON"),
        (
            r#"{"pattern":"x","replacment":"y"}"#,
            "unknown field(s) replacment",
        ),
        (
            r#"{"row":0,"col_start":0,"col_end":2,"keep_prefix":1}"#,
            "unknown field(s) keep_prefix",
        ),
        (r#"{"replacement":"x"}"#, "needs either a \"pattern\""),
        (r#"{"pattern":"([a-"}"#, "invalid regex"),
        (r#"{"pattern":"x","color":"nope"}"#, "invalid color"),
    ];

    for (spec, expected) in cases {
        let message = case.run_err(&["render", "--redaction", spec, &path]);
        assert!(
            message.contains(expected),
            "{spec} should be refused with {expected:?}, got:\n{message}"
        );
    }

    // A coordinate missing one of its three required fields is refused too.
    let message = case.run_err(&["render", "--redaction", r#"{"row":0,"col_start":0}"#, &path]);
    assert!(
        message.contains("\"col_end\" is missing"),
        "unhelpful error: {message}"
    );

    // No screenshot was written for any of the refused runs.
    let pngs = std::fs::read_dir(&case.dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "png"))
        .count();
    assert_eq!(pngs, 0, "a refused redaction must not leave a screenshot");
}

// -------------------------------------------------------------------------
// 7. --no-redact conflicts with a manual specification
// -------------------------------------------------------------------------

/// Silently ignoring a manual redaction because `--no-redact` was also given
/// would hand back an unredacted screenshot, so the pair is refused outright -
/// on both subcommands.
#[test]
fn no_redact_conflicts_with_a_manual_specification() {
    let case = Case::new("no-redact-conflict");
    let input = case.ansi_file("conflict.ansi", "secret\n");

    let message = case.run_err(&[
        "render",
        "--no-redact",
        "--redaction",
        r#"{"pattern":"secret"}"#,
        input.to_str().unwrap(),
    ]);
    assert!(
        message.contains("cannot be used with"),
        "unhelpful conflict error: {message}"
    );

    let message = case.run_err(&[
        "exec",
        "--no-prompt",
        "--no-redact",
        "--redaction",
        r#"{"pattern":"secret"}"#,
        "echo secret",
    ]);
    assert!(
        message.contains("cannot be used with"),
        "unhelpful conflict error: {message}"
    );
}

// -------------------------------------------------------------------------
// 8. Parity with the MCP tool
// -------------------------------------------------------------------------

/// Build an MCP server whose screenshots land in `out_dir`, exactly as
/// `termshot mcp` does.
fn mcp_server(out_dir: &Path) -> ScreenshotServer {
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
    let loaded = LoadedConfig {
        config,
        max_scrollback_lines: termshot::capture::DEFAULT_MAX_SCROLLBACK_LINES,
    };
    let themes: HashMap<String, ThemeConfig> = HashMap::new();
    let renderer = Renderer::new_with_options(
        &FontSelection::default(),
        loaded.font_size,
        &themes,
        &loaded.default_theme,
        &loaded.chrome,
        RendererOptions::default().with_max_scrollback_lines(loaded.max_scrollback_lines),
    )
    .expect("renderer");
    ScreenshotServer::new(loaded.into_config(), renderer)
}

/// All text blocks of a tool result, concatenated.
fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The same specification, applied through the CLI and through the MCP
/// `redact_screenshot` tool, produces the same mask, the same audit, and the
/// same text.
#[tokio::test]
async fn cli_matches_the_equivalent_mcp_redaction() {
    let case = Case::new("parity");
    let content =
        "host 10.20.30.40 key AKIAIOSFODNN7EXAMPLE\nhash 8846f7eaee8fb117ad06bdd830b7586c\n";
    let input = case.ansi_file("parity.ansi", content);

    let specs = [
        r#"{"pattern":"[a-f0-9]{32}","replacement":"HASH","keep_prefix":4}"#,
        r#"{"row":0,"col_start":0,"col_end":4,"label":"SECRET"}"#,
    ];

    // CLI: one render with both specifications.
    let (cli_png, cli_stderr) = case.run_ok(&[
        "render",
        "--cols",
        "60",
        "--rows",
        "8",
        "--redact-text",
        "--redaction",
        specs[0],
        "--redaction",
        specs[1],
        input.to_str().unwrap(),
    ]);

    // MCP: render_ansi, then redact_screenshot with the same specifications
    // decoded from the same JSON.
    let mcp_dir = case.dir.join("mcp");
    let server = mcp_server(&mcp_dir);
    let rendered = server
        .render_ansi(Parameters(RenderAnsiParams {
            input_path: input.display().to_string(),
            cols: Some(60),
            rows: Some(8),
            theme: None,
            chrome: None,
            title: None,
            timestamp: None,
            rounded: None,
            strip_ansi: None,
            output_name: Some("parity".to_string()),
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
    let mcp_png = PathBuf::from(
        result_text(&rendered)
            .lines()
            .find_map(|l| l.strip_prefix("Screenshot saved to: "))
            .expect("screenshot path")
            .trim()
            .to_string(),
    );

    let redactions: Vec<ManualRedactionSpec> = specs
        .iter()
        .map(|spec| ManualRedactionSpec::from_json(spec).expect("shared parser"))
        .collect();
    let redacted = server
        .redact_screenshot_tool(Parameters(RedactScreenshotRequest {
            screenshot_path: mcp_png.display().to_string(),
            redactions,
            redact_text: Some(true),
            show_labels: None,
            strip_ansi: None,
        }))
        .await
        .expect("redact_screenshot");
    let mcp_text = result_text(&redacted);

    // Same audit, in the same order and under the same names.
    let cli_audit = audit_line(&cli_stderr);
    let mcp_audit = mcp_text
        .lines()
        .find(|l| l.starts_with("Redacted: "))
        .expect("mcp audit")
        .to_string();
    assert_eq!(cli_audit, mcp_audit, "audits differ");

    // Same masked text, and the images carry the same description.
    assert_eq!(
        description(&cli_png),
        description(&mcp_png),
        "the descriptions differ"
    );
    let mcp_terminal = mcp_text
        .split("--- Terminal Output ---")
        .nth(1)
        .expect("terminal output")
        .trim()
        .to_string();
    let cli_terminal = cli_stderr
        .split("--- Terminal Output ---")
        .nth(1)
        .expect("terminal output")
        .trim()
        .to_string();
    assert_eq!(cli_terminal, mcp_terminal, "the returned texts differ");

    // Same pixels: the two PNGs are byte-for-byte identical renders.
    let cli_pixels = image::open(&cli_png).unwrap().to_rgba8();
    let mcp_pixels = image::open(&mcp_png).unwrap().to_rgba8();
    assert_eq!(cli_pixels.dimensions(), mcp_pixels.dimensions());
    assert_eq!(
        cli_pixels.into_raw(),
        mcp_pixels.into_raw(),
        "the rendered images differ"
    );
}

// -------------------------------------------------------------------------
// 9. Soft wraps through exec
// -------------------------------------------------------------------------

/// A secret wrapped across the right margin is masked in both the echoed
/// command and the output line, exactly as the MCP flow masks it.
#[test]
fn exec_manual_pattern_catches_the_command_echo_and_the_output() {
    let case = Case::new("exec-wrapped");
    case.use_deterministic_shell();
    // The interactive prompt echoes the command, and at this width the hash in
    // the echo crosses the right margin onto the next row.
    let (png, stderr) = case.run_ok(&[
        "exec",
        "--cols",
        "40",
        "--rows",
        "10",
        "--redact-text",
        "--redaction",
        r#"{"pattern":"[a-f0-9]{32}","keep_prefix":4}"#,
        "echo Secret hash: 8846f7eaee8fb117ad06bdd830b7586c",
    ]);

    let audit = audit_line(&stderr);
    assert!(
        audit.contains("2x custom_0"),
        "both the echo and the output should match: {audit}"
    );

    let description = description(&png);
    for leak in [
        "8846f7eaee8fb117ad06bdd830b7586c",
        "8846f7eaee8fb117ad06",
        "bdd830b7586c",
    ] {
        assert!(
            !description.contains(leak),
            "the image description still leaks {leak:?}: {description}"
        );
    }
    assert_eq!(
        description.matches("8846\u{2588}").count(),
        2,
        "both occurrences should keep their prefix: {description}"
    );

    // The mask reaches the right margin (the wrapped occurrence) and covers
    // more than one row.
    let pixels = redaction_pixels(&png);
    assert!(!pixels.is_empty(), "no redaction block was painted");
    let width = image::open(&png).unwrap().to_rgba8().width();
    assert!(
        pixels.iter().any(|&(x, _)| x as f32 >= 0.75 * width as f32),
        "no redaction ink near the right margin: the wrapped occurrence was missed"
    );
}

// -------------------------------------------------------------------------
// 10. Head / tail selection
// -------------------------------------------------------------------------

/// Coordinates address the rendered screenshot, so they follow a head or tail
/// selection instead of the underlying capture.
#[test]
fn coordinates_follow_a_head_or_tail_selection() {
    let case = Case::new("selection");
    let content: String = (1..=300).map(|i| format!("line {}\n", i)).collect();
    let input = case.ansi_file("many.ansi", &content);
    let path = input.to_str().unwrap().to_string();

    // Tail: row 0 is `line 298`, and there are only three rows to address.
    let (png, stderr) = case.run_ok(&[
        "render",
        "--cols",
        "40",
        "--rows",
        "10",
        "--tail-lines",
        "3",
        "--redact-text",
        "--redaction",
        r#"{"row":0,"col_start":0,"col_end":8}"#,
        &path,
    ]);
    assert_eq!(audit_line(&stderr), "Redacted: 1x manual:REDACTED");
    let tail_description = description(&png);
    let lines: Vec<&str> = tail_description.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "the tail selection renders 3 rows: {lines:?}"
    );
    assert_eq!(
        lines[0],
        "\u{2588}".repeat(8),
        "row 0 is the first tail row"
    );
    assert_eq!(lines[1], "line 299");

    let message = case.run_err(&[
        "render",
        "--cols",
        "40",
        "--rows",
        "10",
        "--tail-lines",
        "3",
        "--redaction",
        r#"{"row":3,"col_start":0,"col_end":4}"#,
        &path,
    ]);
    assert!(
        message.contains("renders 3 row(s) x 8 column(s)"),
        "the bounds must describe the selection, not the capture: {message}"
    );

    // Head: row 0 is `line 1`, and the same three-row bound applies.
    let (head_png, _) = case.run_ok(&[
        "render",
        "--cols",
        "40",
        "--rows",
        "10",
        "--head-lines",
        "3",
        "--redact-text",
        "--redaction",
        r#"{"row":2,"col_start":0,"col_end":6}"#,
        &path,
    ]);
    let head_description = description(&head_png);
    let lines: Vec<&str> = head_description.lines().collect();
    assert_eq!(lines[0], "line 1");
    assert_eq!(lines[2], "\u{2588}".repeat(6));
}
