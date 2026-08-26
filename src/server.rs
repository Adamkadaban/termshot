use crate::config::Config;
use crate::executor;
use crate::redaction::{
    explicit_request_is_blocked, resolve_should_redact, RedactionEngine, RedactionMap,
    RedactionRuleConfig, REDACTION_DISABLED_MSG,
};
use crate::renderer::{
    fallback_output_name, ChromeOptions, ComposeLayout, RedactionRequest, RenderMeta, Renderer,
    TextOptions,
};
use rmcp::{
    handler::server::router::tool::ToolRouter, handler::server::wrapper::Parameters, model::*,
    schemars, service::ServiceExt, tool, tool_handler, tool_router, transport::io::stdio,
    ErrorData as McpError, ServerHandler,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Every parameter struct below denies unknown fields, so a caller that sends
/// a parameter this server does not expose - a stale `embed_description`, a
/// typo, or a field from an older schema - gets a clear error instead of a
/// screenshot silently rendered with different settings than it asked for.
/// (`embed_description` in particular is deliberately global config only.)
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecuteAndScreenshotParams {
    /// Shell command to execute. The command is run in an interactive shell,
    /// so the PS1 prompt (username, directory, etc.) will be visible in the
    /// screenshot.
    pub command: String,

    /// Terminal width in columns. Defaults to server config value (120).
    #[serde(default)]
    pub cols: Option<u16>,

    /// Terminal height in rows. Defaults to server config value (40).
    #[serde(default)]
    pub rows: Option<u16>,

    /// Maximum time in seconds to wait for the command to finish. Defaults to 30.
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// If true, run the command in an interactive login shell so the PS1
    /// prompt is rendered. If false, run the command directly (no prompt).
    /// Defaults to true.
    #[serde(default)]
    pub show_prompt: Option<bool>,

    /// Theme name for rendering. Uses default theme from config if not specified.
    #[serde(default)]
    pub theme: Option<String>,

    /// Optional list of commands. If provided, each command is executed on its
    /// own line and receives its own PS1 prompt. If omitted, `command` is used.
    #[serde(default)]
    pub commands: Option<Vec<String>>,

    /// Chrome preset: none, minimal, gnome, macos, report.
    #[serde(default)]
    pub chrome: Option<String>,

    /// Optional chrome title.
    #[serde(default)]
    pub title: Option<String>,

    /// Whether to add a UTC timestamp watermark below the terminal content.
    #[serde(default)]
    pub timestamp: Option<bool>,

    /// Draw soft rounded corners (default: true). With chrome the window frame
    /// is rounded; without chrome the terminal content itself gets rounded
    /// corners on a transparent background. Set false for square corners.
    #[serde(default)]
    pub rounded: Option<bool>,

    /// Enable or disable redaction of sensitive data (IPs, keys, tokens, ...)
    /// for this screenshot. When omitted, the server's config decides (auto
    /// redaction). Set to false to force redaction off.
    #[serde(default)]
    pub redact: Option<bool>,

    /// Specific redaction rule names to apply (default: all enabled rules).
    /// Providing this implies redaction is enabled.
    #[serde(default)]
    pub redaction_rules: Option<Vec<String>>,

    /// Also redact the returned terminal text. By default only the PNG image is
    /// redacted and the returned text keeps the ORIGINAL (unredacted) content
    /// so you can see what was there and decide what to redact (e.g. via
    /// redact_screenshot). Defaults to false.
    #[serde(default)]
    pub redact_text: Option<bool>,

    /// Whether to draw a short `[LABEL]` tag (e.g. `[IP]`, `[KEY]`) over each
    /// redaction block. Defaults to true. Set to false to draw plain solid
    /// blocks with no text overlay.
    #[serde(default)]
    pub show_labels: Option<bool>,

    /// Strip ANSI color codes from the returned terminal text. By default the
    /// raw output is returned WITH colors preserved. Defaults to false.
    #[serde(default)]
    pub strip_ansi: Option<bool>,

    /// Descriptive filename for the screenshot (without extension). Should be
    /// specific enough to identify the content at a glance among other
    /// screenshots in the same project: when a report folder holds 20 images,
    /// each name should tell you what is in it without opening it. Consider the
    /// context (which project, which phase, which comparison) when naming.
    /// Examples: secretsdump-corp-local-before-hardening, nmap-initial-scan-dmz,
    /// git-log-after-refactor, finding-03-sqli-login-page. When omitted, a
    /// fallback name is derived as `{working-dir}-{first-command-word}`. The
    /// name is sanitized (lowercased, symbols to hyphens) and made unique
    /// (`-2`, `-3`, ...) within the output directory.
    #[serde(default)]
    pub output_name: Option<String>,

    /// Crop the rendered image width to fit the actual content instead of
    /// using the full terminal width. Defaults to true. Set to false to keep
    /// the full `cols` width.
    #[serde(default)]
    pub auto_crop: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderAnsiParams {
    /// Path to a file containing raw terminal output with ANSI escape sequences.
    pub input_path: String,

    /// Terminal width in columns for rendering. Defaults to 120.
    #[serde(default)]
    pub cols: Option<u16>,

    /// Terminal height in rows for rendering. Defaults to 40.
    #[serde(default)]
    pub rows: Option<u16>,

    /// Theme name for rendering. Uses default theme from config if not specified.
    #[serde(default)]
    pub theme: Option<String>,

    /// Chrome preset: none, minimal, gnome, macos, report.
    #[serde(default)]
    pub chrome: Option<String>,

    /// Optional chrome title.
    #[serde(default)]
    pub title: Option<String>,

    /// Whether to add a UTC timestamp watermark below the terminal content.
    #[serde(default)]
    pub timestamp: Option<bool>,

    /// Draw soft rounded corners (default: true). With chrome the window frame
    /// is rounded; without chrome the terminal content itself gets rounded
    /// corners on a transparent background. Set false for square corners.
    #[serde(default)]
    pub rounded: Option<bool>,

    /// Strip ANSI color codes from the returned terminal text. By default the
    /// raw output is returned WITH colors preserved. Defaults to false.
    #[serde(default)]
    pub strip_ansi: Option<bool>,

    /// Optional descriptive base name for the output PNG (without extension).
    /// When omitted, a name is derived from the input file name. The name is
    /// sanitized and made unique within the output directory.
    #[serde(default)]
    pub output_name: Option<String>,

    /// Crop the rendered image width to fit the actual content instead of
    /// using the full terminal width. Defaults to true. Set to false to keep
    /// the full `cols` width.
    #[serde(default)]
    pub auto_crop: Option<bool>,

    /// Enable or disable redaction of sensitive data for this render. When
    /// omitted, the server's config decides (auto redaction). Set to false to
    /// force redaction off.
    #[serde(default)]
    pub redact: Option<bool>,

    /// Specific redaction rule names to apply (default: all enabled rules).
    /// Providing this implies redaction is enabled.
    #[serde(default)]
    pub redaction_rules: Option<Vec<String>>,

    /// Also redact the returned terminal text. By default only the PNG image
    /// is redacted and the returned text keeps the ORIGINAL content.
    #[serde(default)]
    pub redact_text: Option<bool>,

    /// Whether to draw a short `[LABEL]` tag over each redaction block.
    /// Defaults to true.
    #[serde(default)]
    pub show_labels: Option<bool>,
}

/// A single selective redaction for `redact_screenshot`: either a regex
/// pattern (redacts every match) or an explicit cell range.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RedactionSpec {
    /// Redact every match of a regex pattern.
    Pattern {
        /// Regex pattern to match against the terminal text.
        pattern: String,
        /// Replacement marker used to derive the on-image `[LABEL]` tag.
        #[serde(default)]
        replacement: Option<String>,
        /// Number of leading matched characters to leave unmasked. When set,
        /// only the characters after the prefix are blocked out (e.g. show
        /// `AKIA****` for an AWS key). Defaults to 0 (mask everything).
        #[serde(default)]
        keep_prefix: Option<usize>,
        /// Number of trailing matched characters to leave unmasked. When set,
        /// only the characters before the suffix are blocked out. Defaults to
        /// 0 (mask everything).
        #[serde(default)]
        keep_suffix: Option<usize>,
    },
    /// Redact an explicit cell range on a single row (0-based).
    Coordinate {
        /// Row index (0-based).
        row: u16,
        /// First column to redact (0-based, inclusive).
        col_start: u16,
        /// One past the last column to redact (exclusive).
        col_end: u16,
        /// Short label drawn over the redaction block.
        #[serde(default)]
        label: Option<String>,
    },
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RedactScreenshotParams {
    /// Path to a screenshot PNG produced by THIS server instance in the current
    /// session (via execute_and_screenshot or render_ansi). The raw output and
    /// render metadata are kept in memory, so this only works for screenshots
    /// created since the server last started, and not for CLI-produced images.
    pub screenshot_path: String,

    /// Redactions to apply: regex patterns and/or explicit cell ranges.
    pub redactions: Vec<RedactionSpec>,

    /// Also redact the returned terminal text. By default the returned text is
    /// the ORIGINAL (unredacted) content; only the PNG is redacted. Defaults to
    /// false.
    #[serde(default)]
    pub redact_text: Option<bool>,

    /// Whether to draw a label over each redaction block. Defaults to true.
    /// For a regex `pattern`, the label is the `replacement` field (drawn only
    /// when provided); for a coordinate redaction it is the `label` field. Set
    /// to false to draw plain solid blocks with no text overlay.
    #[serde(default)]
    pub show_labels: Option<bool>,

    /// Strip ANSI color codes from the returned terminal text. By default the
    /// raw output is returned WITH colors preserved. Defaults to false.
    #[serde(default)]
    pub strip_ansi: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComposeScreenshotsParams {
    /// Paths to two or more PNG screenshots to combine, in order.
    pub paths: Vec<String>,

    /// Layout: "vertical" (stacked top-to-bottom, tmux-style) or "horizontal"
    /// (side by side). Defaults to "vertical".
    #[serde(default)]
    pub layout: Option<String>,

    /// Divider thickness in pixels drawn between adjacent panes (tmux-style
    /// split). Defaults to 2. Set to 0 for no divider line.
    #[serde(default)]
    pub divider: Option<u32>,

    /// Theme name whose background color fills the canvas. Uses the server's
    /// default theme when omitted.
    #[serde(default)]
    pub theme: Option<String>,

    /// Wrap the composed result in a single outer window frame. Chrome preset:
    /// "none", "minimal", "gnome", "macos", "report". Inputs should be raw
    /// (chrome-less) screenshots so only this outer frame is drawn. Omit for no
    /// outer chrome.
    #[serde(default)]
    pub chrome: Option<String>,

    /// Optional title for the outer chrome frame (only used with `chrome`).
    #[serde(default)]
    pub title: Option<String>,

    /// Output PNG path. When omitted, a file is auto-generated in the output
    /// directory.
    #[serde(default)]
    pub output: Option<String>,
}

/// A screenshot's source data retained in memory so it can be re-rendered
/// (e.g. by `redact_screenshot`) without any on-disk sidecar files.
#[derive(Clone)]
struct CachedRender {
    data: Vec<u8>,
    meta: RenderMeta,
    /// Monotonic insertion counter, used to evict the oldest entries first.
    serial: u64,
}

/// Maximum number of screenshots kept in the in-memory render cache.
const MAX_CACHE_ENTRIES: usize = 32;
/// Maximum total size of the raw terminal bytes held by the render cache.
const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Canonical cache key for a screenshot path, so `./out/x.png` and
/// `/abs/out/x.png` resolve to the same entry.
fn cache_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// Drop the oldest cache entries until the entry-count and byte budgets hold.
/// Bounding the cache keeps a long-lived server from growing without limit and
/// limits how long plaintext captures linger in memory.
fn evict_cache(cache: &mut HashMap<String, CachedRender>) {
    let over_budget = |cache: &HashMap<String, CachedRender>| {
        cache.len() > MAX_CACHE_ENTRIES
            || cache.values().map(|c| c.data.len()).sum::<usize>() > MAX_CACHE_BYTES
    };
    while over_budget(cache) && cache.len() > 1 {
        let oldest = cache
            .iter()
            .min_by_key(|(_, c)| c.serial)
            .map(|(k, _)| k.clone());
        match oldest {
            Some(key) => {
                cache.remove(&key);
            }
            None => break,
        }
    }
}

#[derive(Clone)]
pub struct ScreenshotServer {
    config: Arc<Config>,
    renderer: Arc<Renderer>,
    /// Raw terminal bytes + render metadata for screenshots produced this
    /// session, keyed by PNG path. Populated by execute_and_screenshot /
    /// render_ansi and consumed by redact_screenshot. Lost on restart.
    render_cache: Arc<Mutex<HashMap<String, CachedRender>>>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ScreenshotServer {
    pub fn new(config: Config, renderer: Renderer) -> Self {
        Self {
            config: Arc::new(config),
            renderer: Arc::new(renderer),
            render_cache: Arc::new(Mutex::new(HashMap::new())),
            tool_router: Self::tool_router(),
        }
    }

    /// Remember a rendered screenshot's source bytes + metadata in memory so it
    /// can later be re-rendered by `redact_screenshot`.
    ///
    /// The cache holds raw terminal bytes (plaintext, including anything the
    /// image redacts), so it is bounded: the oldest entries are evicted once
    /// [`MAX_CACHE_ENTRIES`] or [`MAX_CACHE_BYTES`] is exceeded, rather than
    /// keeping every capture for the lifetime of the process.
    fn remember_render(&self, path: &Path, data: &[u8], meta: RenderMeta) {
        let mut cache = match self.render_cache.lock() {
            Ok(cache) => cache,
            Err(_) => {
                tracing::warn!("Render cache lock poisoned; screenshot will not be re-renderable");
                return;
            }
        };
        let key = cache_key(path);
        let serial = cache.values().map(|c| c.serial).max().unwrap_or(0) + 1;
        cache.insert(
            key,
            CachedRender {
                data: data.to_vec(),
                meta,
                serial,
            },
        );
        evict_cache(&mut cache);
    }

    /// Resolve this request's redaction policy and build the matching engine.
    /// Shared by `execute_and_screenshot` and `render_ansi` so both honor the
    /// server's `[redaction] auto` setting and fail closed when redaction was
    /// explicitly requested but cannot be enabled.
    fn resolve_redaction(
        &self,
        redact: Option<bool>,
        rules: Option<&Vec<String>>,
        show_labels: bool,
    ) -> Result<Option<RedactionEngine>, McpError> {
        let redact_flag = redact == Some(true) || rules.is_some();
        let no_redact_flag = redact == Some(false);
        // The master switch wins over an explicit request, but the caller must
        // hear about it instead of receiving an unredacted screenshot.
        if explicit_request_is_blocked(&self.config.redaction, redact_flag, no_redact_flag) {
            return Err(McpError::invalid_params(
                REDACTION_DISABLED_MSG.to_string(),
                None,
            ));
        }
        if !resolve_should_redact(&self.config.redaction, redact_flag, no_redact_flag) {
            return Ok(None);
        }
        match RedactionEngine::from_config_with_labels(&self.config.redaction, show_labels) {
            Ok(engine) => {
                // An explicit rule list with an unknown name must fail loudly
                // instead of silently redacting nothing.
                if let Some(rules) = rules {
                    engine
                        .validate_rule_names(rules)
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                }
                Ok(Some(engine))
            }
            Err(e) if redact_flag => Err(McpError::internal_error(
                format!("redaction was requested but could not be enabled: {}", e),
                None,
            )),
            Err(e) => {
                // Auto-redaction is best-effort: warn and continue.
                tracing::warn!("Auto-redaction disabled: {}", e);
                Ok(None)
            }
        }
    }

    /// Execute a shell command in a real terminal and capture a PNG screenshot
    /// of the output.
    ///
    /// The command runs in a PTY (pseudo-terminal), so all ANSI escape
    /// sequences (colors, formatting, cursor movement) are rendered into an
    /// image that looks like a real terminal. When `show_prompt` is true
    /// (default), it runs in an interactive login shell so the screenshot
    /// includes the PS1 prompt (username, host, directory, ...).
    ///
    /// Redaction: by default the rendered PNG is automatically scanned and
    /// masked for known secret patterns (IPs, AWS keys, JWTs, private keys,
    /// emails, internal hostnames, API keys, ...), while the RETURNED TEXT
    /// keeps the ORIGINAL, unredacted content so you can see exactly what was
    /// on screen. Review that text and, if anything else is sensitive, call
    /// `redact_screenshot` with the returned screenshot path to mask it in the
    /// PNG. Set `redact_text: true` to also scrub the returned text, or
    /// `redact: false` to disable auto-redaction for this call.
    ///
    /// The PNG filename is best set with `output_name`: choose a name specific
    /// enough to identify the screenshot at a glance among others in the same
    /// project (e.g. `nmap-initial-scan-dmz` or `finding-03-sqli-login-page`).
    /// When omitted, a fallback name is derived as
    /// `{working-dir}-{first-command-word}` (e.g. `nrs-cargo`).
    ///
    /// Returns the saved PNG path, the exit status, an optional redaction audit
    /// summary, and the terminal text output.
    #[tool(name = "execute_and_screenshot")]
    pub async fn execute_and_screenshot(
        &self,
        Parameters(params): Parameters<ExecuteAndScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        let cols = params.cols.unwrap_or(self.config.default_cols);
        let rows = params.rows.unwrap_or(self.config.default_rows);
        validate_dimensions(cols, rows)?;
        let timeout = Duration::from_secs(
            params
                .timeout_secs
                .unwrap_or(self.config.default_timeout_secs),
        );
        let show_prompt = params.show_prompt.unwrap_or(true);
        let commands = params
            .commands
            .unwrap_or_else(|| vec![params.command.clone()]);
        let command_refs: Vec<&str> = commands.iter().map(|s| s.as_str()).collect();
        let chrome_options = chrome_options_from_params(
            &self.config,
            params.chrome,
            params.title,
            params.timestamp,
            params.rounded,
            commands.first().map(String::as_str),
        );

        let result = if show_prompt {
            executor::execute_command(&command_refs, &self.config.shell, rows, cols, timeout).await
        } else {
            // Each element of `commands` is an independent command line, so
            // join them with newlines (not spaces) when running without a
            // prompt, so `["pwd", "whoami"]` runs as two commands.
            executor::execute_command_simple(
                &commands.join("\n"),
                &self.config.shell,
                rows,
                cols,
                timeout,
            )
            .await
        };

        let exec_result = result.map_err(|e| {
            McpError::internal_error(format!("Command execution failed: {}", e), None)
        })?;

        let theme_name = params.theme.as_deref();

        // Resolve redaction: an explicit `redact` flag (or a rules list) wins;
        // otherwise fall back to the server's auto-redaction config.
        let redaction_engine = self.resolve_redaction(
            params.redact,
            params.redaction_rules.as_ref(),
            params.show_labels.unwrap_or(true),
        )?;
        let redaction_request = redaction_engine.as_ref().map(|engine| RedactionRequest {
            engine,
            rules: params.redaction_rules.clone(),
        });

        let text_options = TextOptions {
            strip_ansi: params.strip_ansi.unwrap_or(false),
            redact_text: params.redact_text.unwrap_or(false),
            embed_description: self.config.embed_description,
        };

        // Derive the output PNG base name. An explicit `output_name` is the
        // preferred, descriptive choice; otherwise fall back to
        // `{cwd_basename}-{first_word_of_command}`.
        let output_name = params.output_name.clone().unwrap_or_else(|| {
            let cwd = std::env::current_dir().ok();
            fallback_output_name(cwd.as_deref(), &commands.join(" "))
        });

        let (image_path, plain_text, redactions, meta) = self
            .renderer
            .render_bytes(
                &exec_result.raw_output,
                cols,
                rows,
                &self.config.output_dir,
                Some(output_name.as_str()),
                theme_name,
                chrome_options.as_ref(),
                redaction_request.as_ref(),
                text_options,
                params.auto_crop.unwrap_or(true),
            )
            .map_err(|e| McpError::internal_error(format!("Rendering failed: {}", e), None))?;

        // Keep the source bytes + metadata in memory for later re-rendering.
        self.remember_render(&image_path, &exec_result.raw_output, meta);

        let exit_info = if exec_result.timed_out {
            "TIMED OUT".to_string()
        } else {
            match exec_result.exit_code {
                Some(code) => format!("exit code: {}", code),
                None => "exit code: unknown".to_string(),
            }
        };

        let mut content = vec![
            ContentBlock::text(format!("Screenshot saved to: {}", image_path.display())),
            ContentBlock::text(format!("Status: {}", exit_info)),
        ];
        if !redactions.is_empty() {
            let summary = redactions
                .iter()
                .map(|(name, count)| format!("{}x {}", count, name))
                .collect::<Vec<_>>()
                .join(", ");
            content.push(ContentBlock::text(format!("Redacted: {}", summary)));
        }
        content.push(ContentBlock::text(format!(
            "--- Terminal Output ---\n{}",
            plain_text
        )));

        Ok(CallToolResult::success(content))
    }

    /// Render a file containing raw ANSI terminal output to a PNG screenshot.
    ///
    /// Takes a path to a file that contains terminal output with ANSI escape
    /// sequences and renders it to a PNG image. Useful for rendering
    /// previously captured output.
    #[tool(name = "render_ansi")]
    pub async fn render_ansi(
        &self,
        Parameters(params): Parameters<RenderAnsiParams>,
    ) -> Result<CallToolResult, McpError> {
        let cols = params.cols.unwrap_or(self.config.default_cols);
        let rows = params.rows.unwrap_or(self.config.default_rows);
        validate_dimensions(cols, rows)?;
        let theme_name = params.theme.as_deref();
        let chrome_options = chrome_options_from_params(
            &self.config,
            params.chrome,
            params.title,
            params.timestamp,
            params.rounded,
            None,
        );

        let data = std::fs::read(&params.input_path).map_err(|e| {
            McpError::internal_error(
                format!("Failed to read file '{}': {}", params.input_path, e),
                None,
            )
        })?;

        // Derive the output name from an explicit override or the input file.
        let output_name = params.output_name.clone().or_else(|| {
            std::path::Path::new(&params.input_path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        });

        // Rendering a captured log honors the same redaction policy as
        // execution: this is exactly the case where the caller has not read
        // the content first.
        let redaction_engine = self.resolve_redaction(
            params.redact,
            params.redaction_rules.as_ref(),
            params.show_labels.unwrap_or(true),
        )?;
        let redaction_request = redaction_engine.as_ref().map(|engine| RedactionRequest {
            engine,
            rules: params.redaction_rules.clone(),
        });

        let (image_path, plain_text, redactions, meta) = self
            .renderer
            .render_bytes(
                &data,
                cols,
                rows,
                &self.config.output_dir,
                output_name.as_deref(),
                theme_name,
                chrome_options.as_ref(),
                redaction_request.as_ref(),
                TextOptions {
                    strip_ansi: params.strip_ansi.unwrap_or(false),
                    redact_text: params.redact_text.unwrap_or(false),
                    embed_description: self.config.embed_description,
                },
                params.auto_crop.unwrap_or(true),
            )
            .map_err(|e| McpError::internal_error(format!("Rendering failed: {}", e), None))?;

        // Keep the source bytes + metadata in memory for later re-rendering.
        self.remember_render(&image_path, &data, meta);

        let mut content = vec![ContentBlock::text(format!(
            "Screenshot saved to: {}",
            image_path.display()
        ))];
        if !redactions.is_empty() {
            let summary = redactions
                .iter()
                .map(|(name, count)| format!("{}x {}", count, name))
                .collect::<Vec<_>>()
                .join(", ");
            content.push(ContentBlock::text(format!("Redacted: {}", summary)));
        }
        content.push(ContentBlock::text(format!(
            "--- Terminal Output ---\n{}",
            plain_text
        )));

        Ok(CallToolResult::success(content))
    }

    /// Apply selective redactions to a screenshot from this session and
    /// overwrite it in place.
    ///
    /// Agent-driven workflow: run `execute_and_screenshot` (or `render_ansi`),
    /// inspect the returned plain text, decide what is sensitive, then call
    /// this tool with the returned screenshot path plus regex patterns and/or
    /// cell coordinates. The original terminal output is re-parsed from the
    /// server's in-memory record (no sidecar files on disk), the redactions are
    /// applied, and the PNG is re-rendered. This only works for screenshots
    /// produced by this server instance since it last started.
    ///
    /// Each redaction is either a regex `pattern` (with optional `replacement`
    /// used as the on-image `[LABEL]` tag) or an explicit cell range (`row`,
    /// `col_start`, `col_end`, optional `label`). A pattern with no
    /// `replacement` draws a plain block with no label. Set `show_labels: false`
    /// to draw plain blocks for every redaction in the call.
    ///
    /// Partial redaction: a pattern may set `keep_prefix` and/or `keep_suffix`
    /// (character counts) to leave the first/last characters of each match
    /// visible and mask only the middle. For a hash like
    /// `8846f7eaee8fb117ad06bdd830b7586c`, `keep_prefix: 4` renders
    /// `8846████████████████████████████`, and for an AWS key `AKIA...`,
    /// `keep_prefix: 4` shows `AKIA████████████████`.
    ///
    /// By default the returned text is the ORIGINAL content; set `redact_text`
    /// to return the scrubbed text.
    #[tool(name = "redact_screenshot")]
    pub async fn redact_screenshot(
        &self,
        Parameters(params): Parameters<RedactScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        let png_path = std::path::PathBuf::from(&params.screenshot_path);
        let key = cache_key(&png_path);

        let cached = {
            let cache = self.render_cache.lock().map_err(|_| {
                McpError::internal_error("Render cache lock poisoned".to_string(), None)
            })?;
            cache.get(&key).cloned()
        };
        let CachedRender { data, meta, .. } = cached.ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "No in-memory record for screenshot '{}'. It must have been produced \
                     by this server instance in the current session (records are lost on \
                     restart and are not created by the CLI).",
                    key
                ),
                None,
            )
        })?;

        let show_labels = params.show_labels.unwrap_or(true);

        // Split the requested redactions into regex patterns and explicit
        // coordinate ranges.
        let mut pattern_rules: Vec<RedactionRuleConfig> = Vec::new();
        let mut coordinates: Vec<(u16, u16, u16, Option<String>)> = Vec::new();
        for (i, spec) in params.redactions.iter().enumerate() {
            match spec {
                RedactionSpec::Pattern {
                    pattern,
                    replacement,
                    keep_prefix,
                    keep_suffix,
                } => {
                    // Use the caller-supplied `replacement` as the on-image
                    // label. When it is omitted, leave it empty so the block
                    // renders with no label (never a misleading built-in tag).
                    let replacement = replacement.clone().unwrap_or_default();
                    let mut rule =
                        RedactionRuleConfig::new(&format!("custom_{}", i), pattern, &replacement);
                    rule.keep_prefix = *keep_prefix;
                    rule.keep_suffix = *keep_suffix;
                    pattern_rules.push(rule);
                }
                RedactionSpec::Coordinate {
                    row,
                    col_start,
                    col_end,
                    label,
                } => {
                    coordinates.push((*row, *col_start, *col_end, label.clone()));
                }
            }
        }

        // Build the combined redaction map: regex matches first, then manual
        // coordinate ranges.
        let mut map = RedactionMap::default();
        if !pattern_rules.is_empty() {
            let engine = RedactionEngine::from_rules_with_labels(&pattern_rules, show_labels)
                .map_err(|e| {
                    McpError::internal_error(format!("Invalid redaction pattern: {}", e), None)
                })?;
            let mut parser = vt100::Parser::new(meta.rows, meta.cols, 0);
            parser.process(&data);
            engine.redact_screen_into(parser.screen(), None, &mut map);
        }
        for (row, col_start, col_end, label) in &coordinates {
            let label = if show_labels {
                label.as_deref().or(Some("REDACTED"))
            } else {
                None
            };
            map.add_manual(*row, *col_start, *col_end, label);
        }

        let (plain_text, audit) = self
            .renderer
            .render_redaction_to(
                &data,
                &meta,
                &map,
                &png_path,
                TextOptions {
                    strip_ansi: params.strip_ansi.unwrap_or(false),
                    redact_text: params.redact_text.unwrap_or(false),
                    embed_description: self.config.embed_description,
                },
            )
            .map_err(|e| McpError::internal_error(format!("Rendering failed: {}", e), None))?;

        let mut content = vec![ContentBlock::text(format!(
            "Redacted screenshot saved to: {}",
            png_path.display()
        ))];
        if !audit.is_empty() {
            let summary = audit
                .iter()
                .map(|(name, count)| format!("{}x {}", count, name))
                .collect::<Vec<_>>()
                .join(", ");
            content.push(ContentBlock::text(format!("Redacted: {}", summary)));
        }
        content.push(ContentBlock::text(format!(
            "--- Terminal Output ---\n{}",
            plain_text
        )));

        Ok(CallToolResult::success(content))
    }

    /// Compose two or more existing screenshots into a single image, placed
    /// side by side (horizontal) or stacked (vertical) like tmux split panes:
    /// adjacent with no gap, separated by a thin solid divider line. Panes are
    /// stretched to a common height (horizontal) or width (vertical) so the
    /// seams line up. Useful for comparing before/after runs, different themes,
    /// or related outputs.
    #[tool(name = "compose_screenshots")]
    pub async fn compose_screenshots(
        &self,
        Parameters(params): Parameters<ComposeScreenshotsParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.paths.len() < 2 {
            return Err(McpError::invalid_params(
                "compose_screenshots requires at least two image paths".to_string(),
                None,
            ));
        }

        let layout = ComposeLayout::parse(params.layout.as_deref().unwrap_or("vertical"))
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let divider = params.divider.unwrap_or(2);
        let paths: Vec<std::path::PathBuf> =
            params.paths.iter().map(std::path::PathBuf::from).collect();
        let output = params.output.as_ref().map(std::path::PathBuf::from);
        let chrome_options =
            chrome_options_from_params(&self.config, params.chrome, params.title, None, None, None);

        let path = self
            .renderer
            .compose_screenshots(
                &paths,
                layout,
                divider,
                params.theme.as_deref(),
                chrome_options.as_ref(),
                &self.config.output_dir,
                output.as_deref(),
                // Composition follows the server's global embed_description
                // policy; there is deliberately no per-call toggle.
                self.config.embed_description,
            )
            .map_err(|e| McpError::internal_error(format!("Compose failed: {}", e), None))?;

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "Composed screenshot saved to: {}",
            path.display()
        ))]))
    }
}

impl ScreenshotServer {
    /// The tool definitions this server publishes, exactly as an MCP client
    /// sees them (including each tool's JSON input schema).
    pub fn tool_definitions() -> Vec<Tool> {
        Self::tool_router().list_all()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ScreenshotServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Terminal screenshot MCP server. Use execute_and_screenshot to run \
                 commands and capture PNG screenshots of the terminal output, including \
                 PS1 prompt, colors, and full ANSI rendering. Use render_ansi to render \
                 previously captured terminal output from a file. Use redact_screenshot \
                 to selectively redact an existing screenshot by regex pattern or cell \
                 coordinates (run execute_and_screenshot first without redaction, inspect \
                 the plain text, then redact what is sensitive). Use compose_screenshots \
                 to place two or more screenshots side by side or stacked into one image.",
            )
    }
}

/// Start the MCP server on stdio.
pub async fn run_mcp_server(config: Config, renderer: Renderer) -> anyhow::Result<()> {
    tracing::info!("Starting termshot MCP server (stdio transport)");
    let server = ScreenshotServer::new(config, renderer);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Minimum and maximum allowed terminal dimensions. Zero panics the vt100
/// parser and unbounded values can drive enormous image allocations.
const MIN_DIM: u16 = 1;
const MAX_DIM: u16 = 500;

/// Validate terminal cols/rows before they reach the renderer/vt100 parser.
fn validate_dimensions(cols: u16, rows: u16) -> Result<(), McpError> {
    for (label, value) in [("cols", cols), ("rows", rows)] {
        if !(MIN_DIM..=MAX_DIM).contains(&value) {
            return Err(McpError::invalid_params(
                format!(
                    "{} must be between {} and {} (got {})",
                    label, MIN_DIM, MAX_DIM, value
                ),
                None,
            ));
        }
    }
    Ok(())
}

fn chrome_options_from_params(
    config: &Config,
    chrome: Option<String>,
    title: Option<String>,
    timestamp: Option<bool>,
    rounded: Option<bool>,
    inferred_title: Option<&str>,
) -> Option<ChromeOptions> {
    let has_overrides =
        chrome.is_some() || title.is_some() || timestamp.is_some() || rounded.is_some();
    let mut options = ChromeOptions::from_config(&config.chrome);
    if let Some(preset) = chrome {
        options.enabled = preset != "none";
        options.preset = preset;
    }
    if let Some(title) = title {
        options.title = Some(title);
    } else if options.enabled {
        options.title = inferred_title.map(str::to_owned);
    }
    if let Some(timestamp) = timestamp {
        options.timestamp = timestamp;
    }
    if let Some(rounded) = rounded {
        options.rounded = rounded;
    }

    if !(has_overrides || options.enabled && inferred_title.is_some()) {
        return None;
    }
    Some(options)
}
