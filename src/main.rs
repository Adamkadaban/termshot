use anyhow::Context;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use termshot::capture::LineSelection;
use termshot::config::Config;
use termshot::redaction::{
    ManualRedactionSpec, ManualRedactions, REDACTION_DISABLED_MSG, RedactionEngine,
    explicit_request_is_blocked, resolve_should_redact,
};
use termshot::renderer::{
    ChromeOptions, ComposeLayout, ExtendedRenderOptions, FontSelection, RedactionRequest, Renderer,
    RendererOptions, TextOptions, fallback_output_name,
};
use termshot::{executor, server};

#[derive(Parser)]
#[command(
    name = "termshot",
    about = "Capture terminal screenshots with full ANSI rendering",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Path to config file (default: ~/.config/termshot/config.toml)
    #[arg(long, global = true)]
    config: Option<String>,

    /// Directory of extra redaction rule files (.toml + generic .yaml) to
    /// load in addition to the built-in and config rules.
    #[arg(long, global = true)]
    rules_path: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server (stdio transport)
    Mcp,

    /// Execute a command and save a terminal screenshot
    Exec {
        /// The command to execute (everything after --)
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,

        /// Terminal width in columns (1-500)
        #[arg(short = 'c', long, default_value_t = 120, value_parser = clap::value_parser!(u16).range(1..=500))]
        cols: u16,

        /// Terminal viewport height in rows (1-500). This is the terminal the
        /// command runs in - it decides where long lines wrap - not a limit on
        /// what the screenshot shows: output that scrolls off the top is kept
        /// and rendered too.
        #[arg(short = 'r', long, default_value_t = 40, value_parser = clap::value_parser!(u16).range(1..=500))]
        rows: u16,

        /// Timeout in seconds
        #[arg(short, long, default_value_t = 30)]
        timeout: u64,

        /// Show only the first N lines of the output. By default the
        /// screenshot shows every line the command produced, including
        /// everything that scrolled out of the --rows-high viewport.
        #[arg(long, value_name = "N", conflicts_with = "tail_lines", value_parser = clap::value_parser!(u64).range(1..))]
        head_lines: Option<u64>,

        /// Show only the last N lines of the output, like `tail -n`.
        /// Mutually exclusive with --head-lines.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
        tail_lines: Option<u64>,

        /// Chrome preset: none, minimal, gnome, macos, report
        #[arg(long)]
        chrome: Option<String>,

        /// Path to regular font file (overrides embedded JetBrains Mono)
        #[arg(long)]
        font: Option<PathBuf>,

        /// Path to bold font file (uses real bold instead of faux bold)
        #[arg(long)]
        font_bold: Option<PathBuf>,

        /// Hide interactive shell prompt (PS1) from screenshot.
        /// By default the command runs in an interactive login shell so
        /// the prompt is visible. Use --no-prompt to run without it.
        #[arg(long = "no-prompt", default_value_t = false)]
        no_prompt: bool,

        /// Theme name (e.g. dark, catppuccin-mocha, dracula, nord)
        #[arg(long)]
        theme: Option<String>,

        /// Optional chrome title
        #[arg(long)]
        title: Option<String>,

        /// Add a UTC timestamp watermark below the terminal content
        #[arg(long, default_value_t = false)]
        timestamp: bool,

        /// Output file path (default: auto-generated in output dir)
        #[arg(short, long)]
        output: Option<String>,

        /// Redact sensitive data (IPs, keys, tokens, ...) from this screenshot.
        #[arg(long, default_value_t = false)]
        redact: bool,

        /// Comma-separated list of redaction rule names to apply
        /// (default: all enabled rules). Implies --redact.
        #[arg(long, value_name = "RULES")]
        redact_rules: Option<String>,

        /// Force redaction off even when auto-redaction is enabled in config.
        #[arg(long = "no-redact", default_value_t = false)]
        no_redact: bool,

        /// Also redact the returned plain text. By default only the PNG image
        /// is redacted and the text keeps the original (unredacted) content.
        #[arg(long = "redact-text", default_value_t = false)]
        redact_text: bool,

        /// Apply a manual redaction given as JSON. Repeatable, applied in order.
        ///
        /// Takes exactly the specifications the MCP `redact_screenshot` tool
        /// takes - a regex pattern:
        ///
        ///   {"pattern":"[a-f0-9]{32}","replacement":"HASH","keep_prefix":4,"keep_suffix":0,"color":"#d41919"}
        ///
        /// or an explicit cell range of the rendered screenshot:
        ///
        ///   {"row":3,"col_start":12,"col_end":44,"label":"SECRET","color":"#d41919"}
        ///
        /// Only `pattern` (or the three coordinates) is required. Passing this
        /// redacts the screenshot even without --redact; add --redact to run
        /// the built-in rules too. Conflicts with --no-redact.
        #[arg(
            long = "redaction",
            value_name = "JSON",
            conflicts_with = "no_redact",
            verbatim_doc_comment
        )]
        redaction: Vec<String>,

        /// Return plain text with ANSI color codes stripped. By default the
        /// original output is returned with colors preserved.
        #[arg(long = "plain-text", default_value_t = false)]
        plain_text: bool,

        /// Trim the image width to the rightmost content, keeping the same
        /// padding on the right as on the left, instead of using the full
        /// terminal width. Pass `--auto-crop false` to keep full width.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        auto_crop: bool,

        /// Do not embed the terminal text in the PNG's `Description` metadata.
        /// By default the (redacted, when redaction ran) text is embedded so
        /// screen readers can read the screenshot.
        #[arg(long = "no-description", default_value_t = false)]
        no_description: bool,

        /// Draw soft rounded corners (default: on). With chrome the window
        /// frame is rounded; without chrome the terminal content itself gets
        /// rounded corners on a transparent background.
        #[arg(long, overrides_with = "no_rounded")]
        rounded: bool,

        /// Square (un-rounded) corners. Overrides --rounded and the config
        /// default.
        #[arg(long = "no-rounded", overrides_with = "rounded")]
        no_rounded: bool,
    },

    /// Render an ANSI file (or piped stdin) to a PNG screenshot
    Render {
        /// Path to a file containing raw ANSI terminal output, or `-` to read
        /// from stdin (e.g. `cmd --color=always | termshot render -`).
        input: String,

        /// Terminal width in columns (1-500)
        #[arg(short = 'c', long, default_value_t = 120, value_parser = clap::value_parser!(u16).range(1..=500))]
        cols: u16,

        /// Terminal viewport height in rows (1-500). Decides where long lines
        /// wrap; input taller than the viewport is still rendered whole.
        #[arg(short = 'r', long, default_value_t = 40, value_parser = clap::value_parser!(u16).range(1..=500))]
        rows: u16,

        /// Render only the first N lines of the input. By default every line
        /// is rendered, including everything that scrolled out of the
        /// --rows-high viewport.
        #[arg(long, value_name = "N", conflicts_with = "tail_lines", value_parser = clap::value_parser!(u64).range(1..))]
        head_lines: Option<u64>,

        /// Render only the last N lines of the input, like `tail -n`.
        /// Mutually exclusive with --head-lines.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
        tail_lines: Option<u64>,

        /// Theme name
        #[arg(long)]
        theme: Option<String>,

        /// Chrome preset: none, minimal, gnome, macos, report
        #[arg(long)]
        chrome: Option<String>,

        /// Optional chrome title
        #[arg(long)]
        title: Option<String>,

        /// Add a UTC timestamp watermark below the terminal content
        #[arg(long, default_value_t = false)]
        timestamp: bool,

        /// Output file path
        #[arg(short, long)]
        output: Option<String>,

        /// Return plain text with ANSI color codes stripped. By default the
        /// original output is returned with colors preserved.
        #[arg(long = "plain-text", default_value_t = false)]
        plain_text: bool,

        /// Redact sensitive data (IPs, keys, tokens, ...) from this render.
        #[arg(long, default_value_t = false)]
        redact: bool,

        /// Comma-separated list of redaction rule names to apply
        /// (default: all enabled rules). Implies --redact.
        #[arg(long, value_name = "RULES")]
        redact_rules: Option<String>,

        /// Force redaction off even when auto-redaction is enabled in config.
        #[arg(long = "no-redact", default_value_t = false)]
        no_redact: bool,

        /// Also redact the returned plain text. By default only the PNG image
        /// is redacted and the text keeps the original (unredacted) content.
        #[arg(long = "redact-text", default_value_t = false)]
        redact_text: bool,

        /// Apply a manual redaction given as JSON. Repeatable, applied in order.
        ///
        /// Takes exactly the specifications the MCP `redact_screenshot` tool
        /// takes - a regex pattern:
        ///
        ///   {"pattern":"[a-f0-9]{32}","replacement":"HASH","keep_prefix":4,"keep_suffix":0,"color":"#d41919"}
        ///
        /// or an explicit cell range of the rendered screenshot:
        ///
        ///   {"row":3,"col_start":12,"col_end":44,"label":"SECRET","color":"#d41919"}
        ///
        /// Only `pattern` (or the three coordinates) is required. Passing this
        /// redacts the screenshot even without --redact; add --redact to run
        /// the built-in rules too. Conflicts with --no-redact.
        #[arg(
            long = "redaction",
            value_name = "JSON",
            conflicts_with = "no_redact",
            verbatim_doc_comment
        )]
        redaction: Vec<String>,

        /// Trim the image width to the rightmost content, keeping the same
        /// padding on the right as on the left, instead of using the full
        /// terminal width. Pass `--auto-crop false` to keep full width.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        auto_crop: bool,

        /// Do not embed the terminal text in the PNG's `Description` metadata.
        #[arg(long = "no-description", default_value_t = false)]
        no_description: bool,

        /// Draw soft rounded corners (default: on). With chrome the window
        /// frame is rounded; without chrome the terminal content itself gets
        /// rounded corners on a transparent background.
        #[arg(long, overrides_with = "no_rounded")]
        rounded: bool,

        /// Square (un-rounded) corners. Overrides --rounded and the config
        /// default.
        #[arg(long = "no-rounded", overrides_with = "rounded")]
        no_rounded: bool,
    },

    /// List available themes
    Themes,

    /// Compose two or more screenshots side by side or stacked into one image
    Compose {
        /// Input PNG paths to combine, in order (at least two).
        #[arg(required = true, num_args = 2..)]
        images: Vec<PathBuf>,

        /// Layout: vertical (stacked top-to-bottom, tmux-style) or horizontal
        /// (side by side). Defaults to vertical.
        #[arg(long, default_value = "vertical")]
        layout: String,

        /// Divider thickness in pixels between adjacent panes (tmux-style
        /// split). Use 0 for no divider line.
        #[arg(long, default_value_t = 2)]
        divider: u32,

        /// Theme name whose background color fills the canvas
        #[arg(long)]
        theme: Option<String>,

        /// Wrap the composed result in a single outer window frame. Chrome
        /// preset: none, minimal, gnome, macos, report. Inputs should be raw
        /// (chrome-less) screenshots so only this outer frame is drawn.
        #[arg(long)]
        chrome: Option<String>,

        /// Optional chrome title (only used with --chrome)
        #[arg(long)]
        title: Option<String>,

        /// Output file path (default: auto-generated in output dir)
        #[arg(short, long)]
        output: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Set up logging to stderr
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("termshot=info".parse()?),
        )
        .with_writer(std::io::stderr)
        .init();

    // `Config` is frozen at its 1.0.0 shape, so the scrollback capacity - a
    // setting added since - travels beside it in the loaded configuration.
    let loaded = Config::load_with_options(cli.config.as_deref(), cli.rules_path.as_deref())?;
    let max_scrollback_lines = loaded.max_scrollback_lines;
    let config = loaded.into_config();

    match cli.command {
        Commands::Mcp => {
            // One renderer serves every request: it holds a font chain per
            // theme, so a request that selects another theme gets that theme's
            // fonts without rebuilding anything.
            let renderer = build_renderer(&config, max_scrollback_lines, None, None)?;
            server::run_mcp_server(config, renderer).await?;
        }
        Commands::Exec {
            command,
            cols,
            rows,
            timeout,
            head_lines,
            tail_lines,
            no_prompt,
            theme,
            chrome,
            font,
            font_bold,
            title,
            timestamp,
            output,
            redact,
            redact_rules,
            no_redact,
            redact_text,
            redaction,
            plain_text,
            auto_crop,
            no_description,
            rounded,
            no_rounded,
        } => {
            // Fonts: an explicit --font/--font-bold always wins; otherwise
            // each theme uses the fonts it declares, then any globally
            // configured font, then the embedded font. The renderer resolves
            // that chain per theme, exactly as the MCP server does.
            let renderer = build_renderer(
                &config,
                max_scrollback_lines,
                font.as_deref(),
                font_bold.as_deref(),
            )?;

            let lines = line_selection(head_lines, tail_lines)?;
            let timeout = Duration::from_secs(timeout);

            // Compile any --redaction specifications before the command runs,
            // so an invalid regex, color, or JSON object fails without having
            // executed anything.
            let manual = manual_redactions(&redaction)?;

            let exec_result = if !no_prompt {
                // Each CLI argument is a separate command. The shell will
                // show a PS1 prompt before each one.
                let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
                executor::execute_command(&cmd_refs, &config.shell, rows, cols, timeout).await?
            } else {
                // Without prompt, join everything into a single shell -c invocation
                let cmd_str = command.join(" ");
                executor::execute_command_simple(&cmd_str, &config.shell, rows, cols, timeout)
                    .await?
            };

            let theme_name = theme.as_deref();
            let chrome_options = chrome_options_from_args(
                &config,
                chrome,
                title,
                timestamp,
                rounded_override(rounded, no_rounded),
                command.first().map(String::as_str),
            );

            // Resolve whether redaction runs, then build the engine on demand.
            let redaction_engine =
                resolve_redaction(&config, redact, redact_rules.as_deref(), no_redact)?;
            let redaction_request = redaction_engine.as_ref().map(|engine| RedactionRequest {
                engine,
                rules: redact_rules.as_ref().map(|s| parse_rule_list(s)),
            });

            let output_name = {
                let cwd = std::env::current_dir().ok();
                fallback_output_name(cwd.as_deref(), &command.join(" "))
            };
            let (image_path, terminal_text, redactions, meta) = renderer
                .render_bytes_with_extended_options(
                    &exec_result.raw_output,
                    cols,
                    rows,
                    &config.output_dir,
                    Some(output_name.as_str()),
                    theme_name,
                    chrome_options.as_ref(),
                    redaction_request.as_ref(),
                    TextOptions {
                        strip_ansi: plain_text,
                        redact_text,
                        embed_description: config.embed_description && !no_description,
                        // An interactive capture's raw stream is a terminal
                        // session, not a document: its text comes from the screen
                        // so it matches the image exactly.
                        from_screen: !no_prompt,
                    },
                    auto_crop,
                    render_options(lines, manual.as_ref()),
                )?;

            if meta.truncated {
                eprintln!("{}", truncation_notice(&renderer, rows, cols));
            }

            if !redactions.is_empty() {
                let summary = redactions
                    .iter()
                    .map(|(name, count)| format!("{}x {}", count, name))
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!("Redacted: {}", summary);
            }

            // If user specified an output path, move the file there
            let final_path = if let Some(out) = output {
                move_screenshot(&image_path, &out)?
            } else {
                image_path
            };

            // Print screenshot path to stdout (easy to capture); the terminal
            // output and all diagnostics go to stderr.
            println!("{}", final_path.display());
            eprintln!("--- Terminal Output ---");
            eprintln!("{}", terminal_text);

            if exec_result.timed_out {
                eprintln!("(timed out)");
            } else if let Some(code) = exec_result.exit_code
                && code != 0
            {
                eprintln!("(exit {})", code);
            }
        }
        Commands::Render {
            input,
            cols,
            rows,
            head_lines,
            tail_lines,
            theme,
            chrome,
            title,
            timestamp,
            output,
            plain_text,
            redact,
            redact_rules,
            no_redact,
            redact_text,
            redaction,
            auto_crop,
            no_description,
            rounded,
            no_rounded,
        } => {
            let renderer = build_renderer(&config, max_scrollback_lines, None, None)?;
            let lines = line_selection(head_lines, tail_lines)?;
            let manual = manual_redactions(&redaction)?;

            let data = if input == "-" {
                use std::io::Read;
                let mut buf = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut buf)
                    .context("failed to read ANSI data from stdin")?;
                buf
            } else {
                std::fs::read(&input)
                    .with_context(|| format!("failed to read ANSI file {:?}", input))?
            };
            let theme_name = theme.as_deref();
            let chrome_options = chrome_options_from_args(
                &config,
                chrome,
                title,
                timestamp,
                rounded_override(rounded, no_rounded),
                None,
            );
            let output_name = if input == "-" {
                Some("render".to_string())
            } else {
                std::path::Path::new(&input)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            };

            // `render` honors the same redaction policy as `exec`: rendering a
            // captured log is exactly the case where the content has not been
            // eyeballed first.
            let redaction_engine =
                resolve_redaction(&config, redact, redact_rules.as_deref(), no_redact)?;
            let redaction_request = redaction_engine.as_ref().map(|engine| RedactionRequest {
                engine,
                rules: redact_rules.as_ref().map(|s| parse_rule_list(s)),
            });

            let (image_path, plain_text, redactions, meta) = renderer
                .render_bytes_with_extended_options(
                    &data,
                    cols,
                    rows,
                    &config.output_dir,
                    output_name.as_deref(),
                    theme_name,
                    chrome_options.as_ref(),
                    redaction_request.as_ref(),
                    TextOptions {
                        strip_ansi: plain_text,
                        redact_text,
                        embed_description: config.embed_description && !no_description,
                        // The file's bytes *are* the document here, so they are
                        // returned whole rather than clipped to the last screenful.
                        from_screen: false,
                    },
                    auto_crop,
                    render_options(lines, manual.as_ref()),
                )?;

            if meta.truncated {
                eprintln!("{}", truncation_notice(&renderer, rows, cols));
            }

            if !redactions.is_empty() {
                let summary = redactions
                    .iter()
                    .map(|(name, count)| format!("{}x {}", count, name))
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!("Redacted: {}", summary);
            }

            let final_path = if let Some(out) = output {
                move_screenshot(&image_path, &out)?
            } else {
                image_path
            };

            println!("{}", final_path.display());
            eprintln!("--- Terminal Output ---");
            eprint!("{}", plain_text);
        }
        Commands::Compose {
            images,
            layout,
            divider,
            theme,
            chrome,
            title,
            output,
        } => {
            let renderer = build_renderer(&config, max_scrollback_lines, None, None)?;
            let layout = ComposeLayout::parse(&layout)?;
            let chrome_options =
                chrome_options_from_args(&config, chrome, title, false, None, None);
            let output_path = output.map(PathBuf::from);
            let path = renderer.compose_screenshots(
                &images,
                layout,
                divider,
                theme.as_deref(),
                chrome_options.as_ref(),
                &config.output_dir,
                output_path.as_deref(),
                config.embed_description,
            )?;
            println!("{}", path.display());
        }
        Commands::Themes => {
            let renderer = build_renderer(&config, max_scrollback_lines, None, None)?;
            let names = renderer.theme_names();
            let default = &config.default_theme;
            for name in &names {
                let kind = if config.user_theme_names.contains(name) {
                    "user"
                } else {
                    "built-in"
                };
                if name == default {
                    println!("{} ({}, default)", name, kind);
                } else {
                    println!("{} ({})", name, kind);
                }
            }
        }
    }

    Ok(())
}

/// Build the renderer used by every subcommand.
///
/// The renderer owns one font chain per theme (resolved from each theme's
/// `font`, `font_bold`, and `fallback_fonts`), so CLI and MCP render a given
/// theme with exactly the same fonts. `font`/`font_bold` are the user's
/// explicit `--font`/`--font-bold` overrides, which win over every theme's own
/// fonts.
fn build_renderer(
    config: &Config,
    max_scrollback_lines: usize,
    font: Option<&std::path::Path>,
    font_bold: Option<&std::path::Path>,
) -> anyhow::Result<Renderer> {
    let selection = FontSelection {
        font_override: font.map(PathBuf::from),
        font_bold_override: font_bold.map(PathBuf::from),
        global_font: config.font_path.clone(),
        global_fallback_fonts: Vec::new(),
    };
    Renderer::new_with_options(
        &selection,
        config.font_size,
        &config.themes,
        &config.default_theme,
        &config.chrome,
        RendererOptions::default().with_max_scrollback_lines(max_scrollback_lines),
    )
}

/// Compile the repeatable `--redaction '<JSON>'` options into the shared
/// manual redaction set, or `None` when none were given.
///
/// The specifications are exactly the ones the MCP `redact_screenshot` tool
/// takes, parsed by the same code, so a pattern or cell range behaves
/// identically from either entry point - including the on-image `[LABEL]` tags,
/// which the CLI draws just as the MCP default does.
fn manual_redactions(specs: &[String]) -> anyhow::Result<Option<ManualRedactions>> {
    if specs.is_empty() {
        return Ok(None);
    }
    let parsed = ManualRedactionSpec::parse_all(specs)?;
    Ok(Some(ManualRedactions::new(&parsed, true)?))
}

/// Per-render options for a subcommand: the line selection plus any manual
/// redactions, which the renderer applies to the capture it is about to draw.
fn render_options(
    lines: LineSelection,
    manual: Option<&ManualRedactions>,
) -> ExtendedRenderOptions<'_> {
    let options = ExtendedRenderOptions::default().with_lines(lines);
    match manual {
        Some(manual) => options.with_manual(manual),
        None => options,
    }
}

/// Resolve whether redaction should run for this invocation and, if so, build
/// the engine. Shared by `exec` and `render` so both entry points honor the
/// same `[redaction] enabled/auto` policy: an explicit request fails closed if
/// the engine cannot be built, while automatic redaction degrades to a warning.
fn resolve_redaction(
    config: &Config,
    redact: bool,
    redact_rules: Option<&str>,
    no_redact: bool,
) -> anyhow::Result<Option<RedactionEngine>> {
    let want_redact = redact || redact_rules.is_some();
    // The master switch wins over an explicit request, but silently returning
    // an unredacted screenshot to someone who typed --redact would be worse
    // than failing.
    if explicit_request_is_blocked(&config.redaction, want_redact, no_redact) {
        anyhow::bail!(REDACTION_DISABLED_MSG);
    }
    if !resolve_should_redact(&config.redaction, want_redact, no_redact) {
        return Ok(None);
    }
    match RedactionEngine::from_config(&config.redaction) {
        Ok(engine) => {
            // An explicit --redact-rules list with an unknown name must fail
            // loudly instead of silently redacting nothing.
            if let Some(rules) = redact_rules {
                engine.validate_rule_names(&parse_rule_list(rules))?;
            }
            Ok(Some(engine))
        }
        Err(e) if want_redact => Err(anyhow::anyhow!(
            "redaction was requested but could not be enabled: {}",
            e
        )),
        Err(e) => {
            // Auto-redaction is best-effort: warn and continue.
            eprintln!("(auto-redaction disabled: {})", e);
            Ok(None)
        }
    }
}

fn chrome_options_from_args(
    config: &Config,
    chrome: Option<String>,
    title: Option<String>,
    timestamp: bool,
    rounded: Option<bool>,
    inferred_title: Option<&str>,
) -> Option<ChromeOptions> {
    let has_overrides = chrome.is_some() || title.is_some() || timestamp || rounded.is_some();
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
    if timestamp {
        options.timestamp = true;
    }
    if let Some(rounded) = rounded {
        options.rounded = rounded;
    }

    if !(has_overrides || options.enabled && inferred_title.is_some()) {
        return None;
    }
    Some(options)
}

/// Collapse the `--rounded` / `--no-rounded` flag pair into an explicit
/// override: `Some(false)` when `--no-rounded` was given, `Some(true)` when
/// `--rounded` was given, and `None` when neither was passed (so the config
/// default applies). The two flags `overrides_with` each other, so the last one
/// on the command line wins.
fn rounded_override(rounded: bool, no_rounded: bool) -> Option<bool> {
    if no_rounded {
        Some(false)
    } else if rounded {
        Some(true)
    } else {
        None
    }
}

/// Collapse the `--head-lines` / `--tail-lines` flag pair into a line
/// selection. Clap already rejects passing both, so this only has to widen the
/// counts and default to showing everything.
fn line_selection(head: Option<u64>, tail: Option<u64>) -> anyhow::Result<LineSelection> {
    LineSelection::from_head_tail(head.map(|n| n as usize), tail.map(|n| n as usize))
}

/// Warning printed when more output scrolled off than the capture could hold,
/// so the screenshot is missing the oldest lines.
///
/// Reports the capacity that actually applied: a wide terminal is capped below
/// the configured line count by the retained-cell budget, and saying "raise
/// `max_scrollback_lines`" when raising it would change nothing is worse than
/// no advice at all.
fn truncation_notice(renderer: &Renderer, rows: u16, cols: u16) -> String {
    let configured = renderer.max_scrollback_lines();
    let effective = renderer.effective_scrollback_lines(rows, cols);
    let advice = if effective < configured {
        format!(
            "the configured {} was capped to what {} columns can hold in memory, so keep \
             fewer columns or select fewer lines",
            configured, cols
        )
    } else {
        "raise `max_scrollback_lines` in the config to keep more".to_string()
    };
    format!(
        "(output exceeded the {}-line scrollback, so the oldest lines were dropped; {}, \
         or use --head-lines/--tail-lines to pick the end you care about)",
        effective, advice
    )
}

/// Parse a comma-separated `--redact-rules` list into rule names, dropping
/// empty entries and surrounding whitespace.
fn parse_rule_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .map(|r| r.to_string())
        .collect()
}

/// Move a rendered screenshot to a user-specified output path. Falls back to a
/// copy + delete when the source and destination are on different filesystems
/// (where a plain rename fails).
fn move_screenshot(src: &std::path::Path, dst: &str) -> anyhow::Result<PathBuf> {
    let dst = PathBuf::from(dst);
    match std::fs::rename(src, &dst) {
        Ok(()) => Ok(dst),
        Err(_) => {
            std::fs::copy(src, &dst)?;
            std::fs::remove_file(src).ok();
            Ok(dst)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// Extract the `exec` line selection a command line resolves to.
    fn exec_selection(args: &[&str]) -> anyhow::Result<LineSelection> {
        let cli = Cli::try_parse_from(args)?;
        match cli.command {
            Commands::Exec {
                head_lines,
                tail_lines,
                ..
            } => line_selection(head_lines, tail_lines),
            _ => panic!("expected the exec subcommand"),
        }
    }

    /// Showing everything is the default; `--head-lines` / `--tail-lines`
    /// narrow it, and asking for both at once is refused by the parser rather
    /// than silently picking one.
    #[test]
    fn head_and_tail_lines_are_mutually_exclusive() {
        assert_eq!(
            exec_selection(&["termshot", "exec", "--", "seq 1 200"]).unwrap(),
            LineSelection::All
        );
        assert_eq!(
            exec_selection(&["termshot", "exec", "--head-lines", "10", "--", "seq 1 200"]).unwrap(),
            LineSelection::Head(10)
        );
        assert_eq!(
            exec_selection(&["termshot", "exec", "--tail-lines", "10", "--", "seq 1 200"]).unwrap(),
            LineSelection::Tail(10)
        );

        let err = Cli::try_parse_from([
            "termshot",
            "exec",
            "--head-lines",
            "10",
            "--tail-lines",
            "10",
            "--",
            "seq 1 200",
        ])
        .err()
        .expect("--head-lines and --tail-lines must not be accepted together");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);

        // Zero lines is not a selection anyone can render.
        assert!(
            Cli::try_parse_from(["termshot", "exec", "--head-lines", "0", "--", "echo hi"])
                .is_err()
        );
    }

    /// `render` takes the same pair, with the same conflict.
    #[test]
    fn render_also_takes_head_and_tail_lines() {
        let cli = Cli::try_parse_from(["termshot", "render", "--tail-lines", "5", "out.ansi"])
            .expect("parses");
        match cli.command {
            Commands::Render {
                head_lines,
                tail_lines,
                ..
            } => {
                assert_eq!(
                    line_selection(head_lines, tail_lines).unwrap(),
                    LineSelection::Tail(5)
                );
            }
            _ => panic!("expected render"),
        }

        assert!(
            Cli::try_parse_from([
                "termshot",
                "render",
                "--head-lines",
                "5",
                "--tail-lines",
                "5",
                "out.ansi",
            ])
            .is_err()
        );
    }
}
