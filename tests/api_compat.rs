//! Source compatibility with the public Rust API published in 1.0.0.
//!
//! Everything here is written the way a 1.0.0 dependant would have written it,
//! with no knowledge of scrollback capture or line selection. It is really a
//! compile-time test - if these calls stop building, the crate has broken a
//! published signature - but the assertions also check that the old calls still
//! *behave* the way they used to: whole-capture rendering, default scrollback,
//! no surprises.
//!
//! The rule the crate follows: a published struct or enum keeps exactly the
//! shape it shipped with, so an exhaustive literal written without `..` still
//! compiles, and anything gained since lives in a *new* type beside it:
//!
//! | Published in 1.0.0        | Extended by                                  |
//! | ------------------------- | -------------------------------------------- |
//! | `Config`                  | `LoadedConfig` (derefs to it)                |
//! | `RenderMeta`              | `RenderContext` (flattens it)                |
//! | `RenderOptions`           | `ExtendedRenderOptions` (`base` + `manual`)  |
//! | `ExecuteAndScreenshotParams` | `ExecuteAndScreenshotRequest` (`cwd`)     |
//! | `server::RedactionSpec`   | `redaction::ManualRedactionSpec` (`From`)    |
//! | `RedactScreenshotParams`  | `RedactScreenshotRequest` (`From`)           |
//!
//! The MCP surface follows the same rule. `ScreenshotServer::redact_screenshot`
//! keeps the exact signature it published, taking
//! `Parameters<RedactScreenshotParams>`. The tool the router publishes under the
//! unchanged wire name `redact_screenshot` is a second handler,
//! `redact_screenshot_tool`, taking the `RedactScreenshotRequest` whose schema
//! describes the specification the CLI and the tool share. Both delegate to one
//! implementation, so they cannot disagree.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use termshot::capture::LineSelection;
use termshot::config::{ChromeConfig, Config, ThemeConfig};
use termshot::config::{ConfigFile, LoadedConfig};
use termshot::redaction::{
    ManualRedactionSpec, ManualRedactions, RedactionConfig, RedactionEngine,
};
use termshot::renderer::{
    ChromeOptions, ExtendedRenderOptions, FontSelection, RedactionRequest, RenderContext,
    RenderMeta, RenderOptions, Renderer, RendererOptions, TextOptions,
};
use termshot::server::{
    RedactScreenshotParams, RedactScreenshotRequest, RedactionSpec, ScreenshotServer,
};

fn out_dir(name: &str) -> std::path::PathBuf {
    let dir = Path::new("target/api-compat").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The 1.0.0 constructor: fonts, size, themes, default theme, chrome.
fn legacy_renderer() -> Renderer {
    let themes: HashMap<String, ThemeConfig> = HashMap::new();
    Renderer::new(
        &FontSelection::default(),
        16.0,
        &themes,
        "dark",
        &ChromeConfig::default(),
    )
    .expect("Renderer::new keeps its 1.0.0 signature")
}

/// `Renderer::new` still takes five arguments and renders a whole capture.
#[test]
fn renderer_new_keeps_its_signature() {
    let renderer = legacy_renderer();
    assert!(renderer.theme_names().contains(&"dark".to_string()));
}

/// `render_bytes` still takes ten arguments and returns the same four-tuple.
#[test]
fn render_bytes_keeps_its_signature() {
    let renderer = legacy_renderer();
    let dir = out_dir("render-bytes");

    let (path, text, audit, meta): (std::path::PathBuf, String, Vec<(String, usize)>, RenderMeta) =
        renderer
            .render_bytes(
                b"hello legacy world\r\n",
                80,
                24,
                &dir,
                Some("legacy"),
                Some("dark"),
                None,
                None,
                TextOptions::default(),
                true,
            )
            .expect("render_bytes keeps its 1.0.0 signature");

    assert!(path.exists());
    assert!(text.contains("hello legacy world"));
    assert!(audit.is_empty());
    assert_eq!(meta.cols, 80);
    assert_eq!(meta.rows, 24);
    assert!(meta.auto_crop);
    std::fs::remove_file(&path).ok();
}

/// The optional parameters a 1.0.0 caller could pass - a theme, chrome options,
/// a redaction request, text options - all still take the same types.
#[test]
fn render_bytes_optional_arguments_keep_their_types() {
    let renderer = legacy_renderer();
    let dir = out_dir("render-bytes-options");
    let engine = RedactionEngine::from_config(&RedactionConfig::default()).unwrap();
    let chrome = ChromeOptions::from_config(&ChromeConfig::default());
    let request = RedactionRequest {
        engine: &engine,
        rules: None,
    };

    let (path, text, audit, _meta) = renderer
        .render_bytes(
            b"ip 10.20.30.40 up\r\n",
            80,
            24,
            &dir,
            None,
            None,
            Some(&chrome),
            Some(&request),
            TextOptions {
                strip_ansi: true,
                redact_text: true,
                embed_description: true,
                from_screen: true,
            },
            false,
        )
        .expect("render_bytes keeps its 1.0.0 signature");

    assert!(path.exists());
    assert!(!text.contains("10.20.30.40"));
    assert!(!audit.is_empty());
    std::fs::remove_file(&path).ok();
}

/// New behaviour is opt-in through the options types, and the option-taking
/// forms default to exactly what the legacy calls do.
#[test]
fn options_default_to_the_legacy_behaviour() {
    let themes: HashMap<String, ThemeConfig> = HashMap::new();
    let renderer = Renderer::new_with_options(
        &FontSelection::default(),
        16.0,
        &themes,
        "dark",
        &ChromeConfig::default(),
        RendererOptions::default(),
    )
    .unwrap();
    let dir = out_dir("options");
    let data: String = (1..=60).map(|i| format!("line {}\r\n", i)).collect();

    let render = |options: RenderOptions| {
        renderer
            .render_bytes_with_options(
                data.as_bytes(),
                40,
                10,
                &dir,
                Some("options"),
                Some("dark"),
                None,
                None,
                TextOptions {
                    strip_ansi: true,
                    ..TextOptions::default()
                },
                true,
                options,
            )
            .unwrap()
    };

    let (default_path, default_text, _, _) = render(RenderOptions::default());
    let (legacy_path, legacy_text, _, _) = renderer
        .render_bytes(
            data.as_bytes(),
            40,
            10,
            &dir,
            Some("options"),
            Some("dark"),
            None,
            None,
            TextOptions {
                strip_ansi: true,
                ..TextOptions::default()
            },
            true,
        )
        .unwrap();
    assert_eq!(default_text, legacy_text);
    assert!(default_text.contains("line 1\n"));
    assert!(default_text.contains("line 60"));

    for path in [&default_path, &legacy_path] {
        std::fs::remove_file(path).ok();
    }
}

/// `RendererOptions` is defaulted and has a builder, so setting one field never
/// requires naming the others.
#[test]
fn renderer_options_can_be_built_field_by_field() {
    assert_eq!(
        RendererOptions::default().with_max_scrollback_lines(500),
        RendererOptions {
            max_scrollback_lines: 500,
        }
    );
    // `..RendererOptions::default()` is how a caller stays source-compatible
    // as fields are added; today there is only one, which clippy would rather
    // see spelled out.
    #[allow(clippy::needless_update)]
    let spread = RendererOptions {
        max_scrollback_lines: 42,
        ..RendererOptions::default()
    };
    assert_eq!(spread.max_scrollback_lines, 42);
}

/// `RenderOptions` still has exactly the one field it was published with, so
/// the exhaustive literal a 1.0.0 caller wrote - no spread, no lifetime - still
/// compiles, and it is still `Copy`, `Eq`, and `Default`.
#[test]
fn render_options_still_has_exactly_its_1_0_0_field() {
    let options = RenderOptions {
        lines: LineSelection::Tail(5),
    };
    assert_eq!(options.lines, LineSelection::Tail(5));
    assert_eq!(RenderOptions::default().lines, LineSelection::All);
    assert_eq!(
        RenderOptions::default().with_lines(LineSelection::Head(3)),
        RenderOptions {
            lines: LineSelection::Head(3),
        }
    );
    // It is still a plain `Copy` value type.
    let copied = options;
    assert_eq!(copied, options);
}

/// Manual redactions live in the extension type, so a 1.0.0 `RenderOptions`
/// never has to name them, and the extended options default to exactly the
/// 1.0.0 behaviour.
#[test]
fn manual_redactions_are_opt_in_through_the_extended_options() {
    assert!(ExtendedRenderOptions::default().manual.is_none());
    assert_eq!(
        ExtendedRenderOptions::from(RenderOptions::default()),
        ExtendedRenderOptions::default()
    );
    assert_eq!(
        ExtendedRenderOptions::default()
            .with_lines(LineSelection::Head(2))
            .base,
        RenderOptions {
            lines: LineSelection::Head(2),
        }
    );

    let renderer = legacy_renderer();
    let dir = out_dir("manual-redactions");
    let specs = vec![
        ManualRedactionSpec::from_json(r#"{"pattern":"AKIA[0-9A-Z]{16}","keep_prefix":4}"#)
            .unwrap(),
    ];
    let manual = ManualRedactions::new(&specs, true).unwrap();

    let (path, text, audit, _context) = renderer
        .render_bytes_with_extended_options(
            b"key AKIAIOSFODNN7EXAMPLE\r\n",
            80,
            24,
            &dir,
            Some("manual"),
            Some("dark"),
            None,
            None,
            TextOptions {
                strip_ansi: true,
                redact_text: true,
                ..TextOptions::default()
            },
            true,
            RenderOptions::default().with_manual(&manual),
        )
        .unwrap();

    assert_eq!(audit, vec![("custom_0".to_string(), 1)]);
    assert!(text.contains("AKIA\u{2588}"), "{text}");
    assert!(!text.contains("AKIAIOSFODNN7EXAMPLE"), "{text}");
    std::fs::remove_file(&path).ok();
}

/// `server::RedactionSpec` still has exactly the variants and fields 1.0.0
/// published, so the literals an old caller wrote - with no `color` and no
/// spread - still compile, and each converts into the shared specification the
/// tool and the CLI use today.
#[test]
fn redaction_spec_still_has_exactly_its_1_0_0_variants() {
    let pattern = RedactionSpec::Pattern {
        pattern: r"10\.20\.30\.40".to_string(),
        replacement: Some("[REDACTED-IP]".to_string()),
        keep_prefix: None,
        keep_suffix: Some(2),
    };
    let coordinate = RedactionSpec::Coordinate {
        row: 3,
        col_start: 12,
        col_end: 44,
        label: Some("SECRET".to_string()),
    };

    match ManualRedactionSpec::from(pattern) {
        ManualRedactionSpec::Pattern {
            pattern,
            replacement,
            keep_prefix,
            keep_suffix,
            color,
        } => {
            assert_eq!(pattern, r"10\.20\.30\.40");
            assert_eq!(replacement.as_deref(), Some("[REDACTED-IP]"));
            assert_eq!(keep_prefix, None);
            assert_eq!(keep_suffix, Some(2));
            assert!(color.is_none(), "a 1.0.0 spec never set a color");
        }
        other => panic!("expected a pattern spec, got {other:?}"),
    }

    match ManualRedactionSpec::from(coordinate) {
        ManualRedactionSpec::Coordinate {
            row,
            col_start,
            col_end,
            label,
            color,
        } => {
            assert_eq!((row, col_start, col_end), (3, 12, 44));
            assert_eq!(label.as_deref(), Some("SECRET"));
            assert!(color.is_none(), "a 1.0.0 spec never set a color");
        }
        other => panic!("expected a coordinate spec, got {other:?}"),
    }

    // It still deserializes the JSON 1.0.0 accepted.
    let parsed: RedactionSpec =
        serde_json::from_str(r#"{"row":1,"col_start":0,"col_end":4,"label":"X"}"#).unwrap();
    assert!(matches!(parsed, RedactionSpec::Coordinate { row: 1, .. }));
}

/// `RedactScreenshotParams` still has exactly the fields it was published
/// with, including `redactions: Vec<RedactionSpec>`, and converts into the
/// request the tool takes today.
#[test]
fn redact_screenshot_params_still_have_their_1_0_0_shape() {
    let params = RedactScreenshotParams {
        screenshot_path: "shot.png".to_string(),
        redactions: vec![
            RedactionSpec::Pattern {
                pattern: "secret".to_string(),
                replacement: None,
                keep_prefix: None,
                keep_suffix: None,
            },
            RedactionSpec::Coordinate {
                row: 0,
                col_start: 0,
                col_end: 6,
                label: None,
            },
        ],
        redact_text: Some(true),
        show_labels: None,
        strip_ansi: None,
    };

    let request: RedactScreenshotRequest = params.into_request();
    assert_eq!(request.screenshot_path, "shot.png");
    assert_eq!(request.redactions.len(), 2);
    assert_eq!(request.redact_text, Some(true));
    assert!(request.show_labels.is_none());
    assert!(request.strip_ansi.is_none());

    // `From` is the same conversion, so `.into()` works at a call site too.
    let request: RedactScreenshotRequest = RedactScreenshotParams {
        screenshot_path: "shot.png".to_string(),
        redactions: Vec::new(),
        redact_text: None,
        show_labels: Some(false),
        strip_ansi: Some(true),
    }
    .into();
    assert!(request.redactions.is_empty());
    assert_eq!(request.show_labels, Some(false));

    // And it still deserializes the request body 1.0.0 documented.
    let parsed: RedactScreenshotParams = serde_json::from_str(
        r#"{"screenshot_path":"shot.png","redactions":[{"pattern":"x","replacement":"Y"}]}"#,
    )
    .unwrap();
    assert_eq!(parsed.redactions.len(), 1);
}

/// `ScreenshotServer::redact_screenshot` still takes `Parameters` of the 1.0.0
/// `RedactScreenshotParams` and returns the same result, so a Rust caller that
/// drives the server directly still compiles - and still gets the same error
/// for a screenshot this server never rendered.
#[test]
fn redact_screenshot_keeps_its_1_0_0_method_signature() {
    // Written exactly as a 1.0.0 caller would have written the call: no
    // conversion, no `.into()`. This function only compiles while the method
    // keeps its published signature.
    async fn call_the_1_0_0_way(
        server: &ScreenshotServer,
        params: RedactScreenshotParams,
    ) -> Result<CallToolResult, McpError> {
        server.redact_screenshot(Parameters(params)).await
    }

    let server = ScreenshotServer::new(Config::default(), legacy_renderer());
    let params = RedactScreenshotParams {
        screenshot_path: "target/api-compat/never-rendered.png".to_string(),
        redactions: vec![RedactionSpec::Pattern {
            pattern: "secret".to_string(),
            replacement: None,
            keep_prefix: None,
            keep_suffix: None,
        }],
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
    };

    let err = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(call_the_1_0_0_way(&server, params))
        .expect_err("this server never rendered that screenshot");
    assert!(
        err.to_string().contains("No in-memory record"),
        "the 1.0.0 call must still reach the handler: {err}"
    );

    // The tool handler behind the published schema takes the shared request,
    // and the two parameter types convert into one another's shape.
    let request: RedactScreenshotRequest = RedactScreenshotParams {
        screenshot_path: "target/api-compat/never-rendered.png".to_string(),
        redactions: Vec::new(),
        redact_text: None,
        show_labels: None,
        strip_ansi: None,
    }
    .into_request();
    let err = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(server.redact_screenshot_tool(Parameters(request)))
        .expect_err("this server never rendered that screenshot");
    assert!(err.to_string().contains("No in-memory record"), "{err}");
}

/// `Config` still has exactly the fields it was published with, so an
/// exhaustive literal - which is how a 1.0.0 dependant with no `..Default`
/// would have written it - still compiles. Adding a field here would break that
/// code, so any setting termshot has gained since lives in `LoadedConfig`.
#[test]
fn config_still_has_exactly_its_1_0_0_fields() {
    let config = Config {
        output_dir: PathBuf::from("target/api-compat/config"),
        font_path: None,
        font_size: 16.0,
        default_cols: 100,
        default_rows: 30,
        default_timeout_secs: 30,
        shell: "/bin/bash".to_string(),
        embed_description: true,
        default_theme: "dark".to_string(),
        chrome: ChromeConfig::default(),
        themes: HashMap::new(),
        user_theme_names: BTreeSet::new(),
        redaction: RedactionConfig::default(),
    };
    assert_eq!(config.default_cols, 100);
    assert_eq!(config.default_rows, 30);
    assert!(config.themes.is_empty());
}

/// `RenderMeta` likewise: exactly the six fields 1.0.0 published, named
/// exhaustively, with no spread.
#[test]
fn render_meta_still_has_exactly_its_1_0_0_fields() {
    let meta = RenderMeta {
        cols: 100,
        rows: 30,
        theme: Some("dark".to_string()),
        chrome: None,
        auto_crop: true,
        from_screen: false,
    };
    assert_eq!(meta.cols, 100);
    assert_eq!(meta.rows, 30);
    assert!(meta.auto_crop, "auto_crop defaults to on, as it always has");
    assert!(!meta.from_screen);
}

/// `ConfigFile` is the third exhaustive public struct: it deserializes the TOML
/// a 1.0.0 user wrote, and keys added since are read alongside it rather than
/// as new fields.
#[test]
fn config_file_still_has_exactly_its_1_0_0_fields() {
    let file = ConfigFile {
        output_dir: "target/api-compat/config-file".to_string(),
        font_path: None,
        font_size: 16.0,
        cols: 120,
        rows: 40,
        timeout_secs: 30,
        shell: None,
        embed_description: true,
        default_theme: "dark".to_string(),
        chrome: ChromeConfig::default(),
        themes: HashMap::new(),
        redaction: RedactionConfig::default(),
    };
    assert_eq!(file.cols, 120);
    assert_eq!(file.rows, 40);

    // A config file carrying the newer key still parses into the 1.0.0 struct:
    // unknown keys are ignored, so old code reading new config keeps working.
    let parsed: ConfigFile = toml::from_str(
        "output_dir = \"target/api-compat/config-file\"\nmax_scrollback_lines = 4242\n",
    )
    .expect("a 1.0.0 ConfigFile still parses a config file with newer keys");
    assert_eq!(parsed.output_dir, "target/api-compat/config-file");
}

/// The settings added since 1.0.0 are reachable through the extension type,
/// which is what termshot's own CLI and MCP server use.
#[test]
fn loaded_config_carries_the_settings_added_after_1_0_0() {
    let loaded = LoadedConfig::default();
    assert!(loaded.max_scrollback_lines >= 1);
    // It derefs to `Config`, so the published fields read exactly as before.
    assert_eq!(loaded.default_cols, Config::default().default_cols);
    assert!(loaded.config().themes.contains_key("dark"));
    assert!(loaded.into_config().themes.contains_key("dark"));
}

/// `RenderContext` is where the renderer records what it learned after 1.0.0,
/// and it wraps an untouched `RenderMeta`.
#[test]
fn render_context_extends_meta_without_changing_it() {
    let context = RenderContext::from_meta(RenderMeta {
        cols: 80,
        rows: 24,
        theme: None,
        chrome: None,
        auto_crop: true,
        from_screen: false,
    });
    assert_eq!(context.lines, LineSelection::All);
    assert!(!context.truncated);
    // It derefs to the metadata it carries.
    assert_eq!(context.cols, 80);
    assert_eq!(context.meta.rows, 24);
}

/// The option-taking render API returns the extended context, so new callers
/// can see the line selection and truncation the 1.0.0 tuple cannot carry.
#[test]
fn the_option_taking_api_returns_the_extended_context() {
    let renderer = legacy_renderer();
    let dir = out_dir("render-context");
    let data: String = (1..=200).map(|i| format!("line {}\r\n", i)).collect();

    let (path, text, _, context) = renderer
        .render_bytes_with_options(
            data.as_bytes(),
            40,
            10,
            &dir,
            Some("context"),
            Some("dark"),
            None,
            None,
            TextOptions {
                strip_ansi: true,
                ..TextOptions::default()
            },
            true,
            RenderOptions::default().with_lines(LineSelection::Head(5)),
        )
        .unwrap();

    assert_eq!(context.lines, LineSelection::Head(5));
    assert!(!context.truncated);
    assert_eq!(context.meta.cols, 40);
    assert_eq!(text.lines().count(), 5);
    std::fs::remove_file(&path).ok();
}

/// The redaction engine still takes a plain `vt100::Screen`, the way a 1.0.0
/// caller with its own parser would hand one over.
#[test]
fn redaction_still_accepts_a_plain_vt100_screen() {
    let engine = RedactionEngine::from_config(&RedactionConfig::default()).unwrap();
    let mut parser = vt100::Parser::new(6, 60, 0);
    parser.process(b"ip 10.20.30.40 up\r\n");

    let map = engine.redact_screen(parser.screen(), None);
    assert!(!map.is_empty(), "the IPv4 address should be redacted");
    assert!(!map.audit_summary().is_empty());

    let mut into = termshot::redaction::RedactionMap::default();
    engine.redact_screen_into(parser.screen(), None, &mut into);
    assert_eq!(into.cell_count(), map.cell_count());

    let text = map.redacted_plain_text(parser.screen());
    assert!(text.contains("ip "));
    assert!(!text.contains("10.20.30.40"));
}

/// `RenderMeta` still serializes as the flat record 1.0.0 wrote, and the
/// extended `RenderContext` flattens it, so a stored 1.0.0 record deserializes
/// into either.
#[test]
fn render_meta_serialization_is_unchanged_and_context_flattens_it() {
    let meta = RenderMeta {
        cols: 100,
        rows: 30,
        theme: Some("dark".to_string()),
        chrome: None,
        auto_crop: false,
        from_screen: true,
    };
    let json = serde_json::to_value(&meta).unwrap();
    assert_eq!(json["cols"], 100);
    assert_eq!(json["rows"], 30);
    assert_eq!(json["auto_crop"], false);
    assert_eq!(json["from_screen"], true);
    assert!(
        json.get("lines").is_none() && json.get("truncated").is_none(),
        "RenderMeta must not carry the newer fields: {json}"
    );

    // A 1.0.0 record still reads back as a context, with the newer fields at
    // their defaults.
    let context: RenderContext = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(context.meta.cols, 100);
    assert!(!context.meta.auto_crop);
    assert_eq!(context.lines, LineSelection::All);
    assert!(!context.truncated);

    // And a context round-trips through the flattened form.
    let extended = RenderContext {
        meta,
        lines: LineSelection::Tail(7),
        truncated: true,
    };
    let round: RenderContext =
        serde_json::from_value(serde_json::to_value(&extended).unwrap()).unwrap();
    assert_eq!(round.lines, LineSelection::Tail(7));
    assert!(round.truncated);
    assert_eq!(round.meta.rows, 30);

    // The flattened record still deserializes into the 1.0.0 struct, which
    // ignores the fields it does not know.
    let back: RenderMeta =
        serde_json::from_value(serde_json::to_value(&extended).unwrap()).unwrap();
    assert_eq!(back.cols, 100);
    assert!(back.from_screen);
}
