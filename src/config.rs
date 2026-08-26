use crate::redaction::RedactionConfig;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Top-level config file structure (loaded from TOML).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigFile {
    /// Directory where screenshot PNGs are saved.
    pub output_dir: String,
    /// Path to a monospace font file (overrides bundled font).
    pub font_path: Option<String>,
    /// Font size in pixels.
    pub font_size: f32,
    /// Default terminal columns.
    pub cols: u16,
    /// Default terminal rows.
    pub rows: u16,
    /// Default command timeout in seconds.
    pub timeout_secs: u64,
    /// Shell to use for command execution.
    pub shell: Option<String>,
    /// Embed the terminal text in each PNG's UTF-8 `Description` metadata
    /// (PNG `iTXt`) so screenshots are readable by screen readers.
    pub embed_description: bool,
    /// Name of the default theme to use.
    pub default_theme: String,
    /// Default chrome settings.
    pub chrome: ChromeConfig,
    /// Custom theme definitions (keyed by name).
    pub themes: HashMap<String, ThemeConfig>,
    /// Redaction settings ([redaction] section).
    pub redaction: RedactionConfig,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            output_dir: "/tmp/termshot".to_string(),
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChromeConfig {
    pub enabled: bool,
    pub preset: String,
    pub title: Option<String>,
    pub timestamp: bool,
    pub shadow: bool,
    pub radius: u32,
    /// Draw soft rounded corners. Independent of `enabled`: with chrome the
    /// window frame is rounded; without chrome the terminal content itself gets
    /// rounded corners on a transparent background. Defaults to `true`.
    pub rounded: bool,
    pub outer_padding: u32,
    pub title_bar_height: u32,
}

impl Default for ChromeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            preset: "none".to_string(),
            title: None,
            timestamp: false,
            shadow: false,
            radius: 14,
            rounded: true,
            outer_padding: 0,
            title_bar_height: 34,
        }
    }
}

/// A theme definition in the config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    /// Foreground color as "#RRGGBB".
    pub foreground: String,
    /// Background color as "#RRGGBB".
    pub background: String,
    /// Path to regular font file (overrides default embedded font).
    #[serde(default)]
    pub font: Option<String>,
    /// Path to bold font file (if not set, faux bold is used).
    #[serde(default)]
    pub font_bold: Option<String>,
    /// Extra font files searched, in order, for characters the primary font
    /// does not have. The embedded JetBrains Mono is always tried before these,
    /// so it never needs to be listed here.
    #[serde(default)]
    pub fallback_fonts: Vec<String>,
    /// 16-color ANSI palette as ["#RRGGBB", ...].
    pub palette: [String; 16],
    /// Directory the theme was loaded from, used to resolve relative font
    /// paths. Not part of the serialized TOML.
    #[serde(skip)]
    pub base_dir: Option<PathBuf>,
}

impl ThemeConfig {
    /// Resolve this theme's regular and bold font paths, expanding `~` and
    /// resolving relative paths against the theme's own directory. Missing
    /// files are dropped (with a warning) so rendering falls back to the
    /// globally configured or embedded font.
    ///
    /// The renderer builds one font chain per theme from these paths, so a
    /// theme's fonts apply wherever that theme is rendered - CLI or MCP.
    pub fn resolved_font_paths(&self) -> (Option<PathBuf>, Option<PathBuf>) {
        let base = self.base_dir.as_deref();
        let regular = self.font.as_ref().and_then(|p| resolve_font_path(p, base));
        let bold = self
            .font_bold
            .as_ref()
            .and_then(|p| resolve_font_path(p, base));
        (regular, bold)
    }

    /// Resolve this theme's extra fallback font paths, in the order they were
    /// listed. Paths are expanded exactly like the primary fonts; entries that
    /// do not exist are dropped with a warning.
    ///
    /// The embedded JetBrains Mono is *not* part of this list: the renderer
    /// always tries it before these fonts, so users never have to declare it.
    pub fn resolved_fallback_font_paths(&self) -> Vec<PathBuf> {
        let base = self.base_dir.as_deref();
        self.fallback_fonts
            .iter()
            .filter_map(|p| resolve_fallback_font_path(p, base))
            .collect()
    }
}

/// Resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub output_dir: PathBuf,
    pub font_path: Option<PathBuf>,
    pub font_size: f32,
    pub default_cols: u16,
    pub default_rows: u16,
    pub default_timeout_secs: u64,
    pub shell: String,
    /// Embed the terminal text in each PNG's `Description` metadata.
    pub embed_description: bool,
    pub default_theme: String,
    pub chrome: ChromeConfig,
    pub themes: HashMap<String, ThemeConfig>,
    /// Names of themes that came from user sources (the themes directory or
    /// inline `[themes.*]` config tables), as opposed to compiled-in builtins.
    pub user_theme_names: BTreeSet<String>,
    /// Redaction settings ([redaction] section).
    pub redaction: RedactionConfig,
}

impl Config {
    /// Load config by merging (in order): defaults, config file, env vars.
    /// `rules_path` overrides the `[redaction] rules_path` directory (e.g. from
    /// the `--rules-path` CLI flag).
    pub fn load(config_path: Option<&str>, rules_path: Option<&str>) -> Result<Self> {
        let config_dir = dirs_config().join("termshot");

        // Create the config directory and default config/theme files the first
        // time termshot runs (only for the default location, never when the
        // user explicitly points at a config file).
        if config_path.is_none()
            && let Err(e) = bootstrap_defaults(&config_dir)
        {
            tracing::warn!("Failed to write default config files: {}", e);
        }

        // Start with defaults
        let mut file_config = ConfigFile::default();

        // Try loading config file
        let paths_to_try = if let Some(p) = config_path {
            vec![PathBuf::from(p)]
        } else {
            vec![
                config_dir.join("config.toml"),
                PathBuf::from("termshot.toml"),
            ]
        };

        let mut loaded_config_path: Option<PathBuf> = None;
        for path in &paths_to_try {
            if path.exists() {
                let contents = std::fs::read_to_string(path)?;
                file_config = toml::from_str(&contents)?;
                tracing::info!("Loaded config from {:?}", path);
                loaded_config_path = Some(path.clone());
                break;
            }
        }

        // Build the merged theme map. Resolution order (lowest to highest
        // priority): builtin compiled themes, inline `[themes.*]` config
        // tables, then user themes in ~/.config/termshot/themes/.
        let mut themes: HashMap<String, ThemeConfig> = builtin_themes();
        let mut user_theme_names: BTreeSet<String> = BTreeSet::new();

        // Inline themes from the config file. Relative font paths resolve
        // against the directory containing the config file.
        let inline_base = loaded_config_path
            .as_ref()
            .and_then(|p| p.parent().map(PathBuf::from));
        for (name, mut theme) in file_config.themes {
            theme.base_dir = inline_base.clone();
            themes.insert(name.clone(), theme);
            user_theme_names.insert(name);
        }

        // User theme files: each .toml in ~/.config/termshot/themes/.
        let themes_dir = config_dir.join("themes");
        match load_user_themes(&themes_dir) {
            Ok(user_themes) => {
                for (name, theme) in user_themes {
                    themes.insert(name.clone(), theme);
                    user_theme_names.insert(name);
                }
            }
            Err(e) => tracing::warn!("Failed to load user themes from {:?}: {}", themes_dir, e),
        }

        // Env var overrides (TERMSHOT_*).
        let output_dir = env_var("OUTPUT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&file_config.output_dir));

        // Font path is optional: only set when explicitly configured via env
        // var or the config file. When None, the renderer uses the embedded
        // font, so release archives and `cargo install` builds always work.
        let font_path = env_var("FONT_PATH")
            .map(PathBuf::from)
            .or_else(|| file_config.font_path.as_ref().map(PathBuf::from));

        let font_size = env_var("FONT_SIZE")
            .and_then(|s| s.parse().ok())
            .unwrap_or(file_config.font_size);

        let default_cols = env_var("COLS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(file_config.cols);

        let default_rows = env_var("ROWS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(file_config.rows);

        let default_timeout_secs = env_var("TIMEOUT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(file_config.timeout_secs);

        let shell = env_var("SHELL")
            .or(file_config.shell)
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/bash".to_string());

        let mut default_theme = env_var("THEME").unwrap_or(file_config.default_theme);

        // Fall back to "dark" when the configured default theme is unknown.
        if !themes.contains_key(&default_theme) {
            tracing::warn!(
                "Default theme '{}' not found, falling back to 'dark'",
                default_theme
            );
            default_theme = "dark".to_string();
        }

        let mut chrome = file_config.chrome;
        if let Some(preset) = env_var("CHROME") {
            chrome.enabled = preset != "none";
            chrome.preset = preset;
        }

        std::fs::create_dir_all(&output_dir)?;

        // Merge redaction rules from the rule directories.
        let mut redaction = file_config.redaction;
        merge_rule_dirs(&mut redaction, &config_dir, rules_path);

        Ok(Self {
            output_dir,
            font_path,
            font_size,
            default_cols,
            default_rows,
            default_timeout_secs,
            shell,
            embed_description: file_config.embed_description,
            default_theme,
            chrome,
            themes,
            user_theme_names,
            redaction,
        })
    }

    /// Resolve the regular and bold font paths declared by a theme, expanding
    /// `~` and resolving relative paths against the theme's directory. Missing
    /// files are dropped (with a warning) so rendering falls back to the
    /// embedded font.
    pub fn theme_font_paths(&self, theme_name: &str) -> (Option<PathBuf>, Option<PathBuf>) {
        match self.themes.get(theme_name) {
            Some(theme) => theme.resolved_font_paths(),
            None => (None, None),
        }
    }

    /// Resolve the extra fallback font paths declared by a theme, in the order
    /// they were listed. Paths are expanded exactly like the primary fonts;
    /// entries that do not exist are dropped with a warning.
    ///
    /// The embedded JetBrains Mono is *not* part of this list: the renderer
    /// always tries it before these fonts, so users never have to declare it.
    pub fn theme_fallback_font_paths(&self, theme_name: &str) -> Vec<PathBuf> {
        match self.themes.get(theme_name) {
            Some(theme) => theme.resolved_fallback_font_paths(),
            None => Vec::new(),
        }
    }
}

/// Directory of user redaction rule files, alongside `themes/`. Every `.toml`,
/// `.yaml`, and `.yml` file dropped in here is loaded automatically.
pub fn user_rules_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("rules")
}

/// Load rule files into `redaction.rules`, in increasing order of precedence:
/// the user rules directory (`~/.config/termshot/rules`), then any explicit
/// directory from `[redaction] rules_path` or the `--rules-path` flag (which
/// wins). A rule overrides an earlier one - builtin or file-loaded - with the
/// same `name`.
fn merge_rule_dirs(redaction: &mut RedactionConfig, config_dir: &Path, rules_path: Option<&str>) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let user_dir = user_rules_dir(config_dir);
    if user_dir.is_dir() {
        dirs.push(user_dir);
    }
    if let Some(dir) = rules_path
        .map(|s| s.to_string())
        .or_else(|| redaction.rules_path.clone())
    {
        let dir = expand_tilde(&dir);
        // An explicit path that just points at the user directory is not a
        // second source; loading it twice would only duplicate the rules.
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }

    for dir in dirs {
        let loaded = crate::redaction::load_rules_from_dir(&dir);
        if !loaded.is_empty() {
            tracing::info!("Loaded {} redaction rule(s) from {:?}", loaded.len(), dir);
        }
        redaction.rules.extend(loaded);
    }
}

/// Read a `TERMSHOT_<name>` environment override.
///
/// The pre-rename `SCREENSHOT_MCP_<name>` spelling is still accepted (with a
/// warning) so existing setups keep working for one release.
fn env_var(name: &str) -> Option<String> {
    if let Ok(value) = std::env::var(format!("TERMSHOT_{}", name)) {
        return Some(value);
    }
    match std::env::var(format!("SCREENSHOT_MCP_{}", name)) {
        Ok(value) => {
            tracing::warn!(
                "SCREENSHOT_MCP_{} is deprecated, use TERMSHOT_{} instead",
                name,
                name
            );
            Some(value)
        }
        Err(_) => None,
    }
}

fn dirs_config() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".config")
        })
}

/// Build the map of compiled-in builtin themes.
fn builtin_themes() -> HashMap<String, ThemeConfig> {
    let builtins: Vec<(&str, ThemeConfig)> = vec![
        (
            "dark",
            ThemeConfig {
                font: None,
                font_bold: None,
                fallback_fonts: Vec::new(),
                base_dir: None,
                foreground: "#cccccc".into(),
                background: "#1e1e1e".into(),
                palette: [
                    "#1e1e1e", "#cc0000", "#4e9a06", "#c4a000", "#3465a4", "#75507b", "#0698a0",
                    "#d3d7cf", "#555753", "#ef2929", "#8ae234", "#fce94f", "#729fcf", "#ad7fa8",
                    "#34e2e2", "#eeeeec",
                ]
                .map(String::from),
            },
        ),
        (
            "catppuccin-mocha",
            ThemeConfig {
                font: None,
                font_bold: None,
                fallback_fonts: Vec::new(),
                base_dir: None,
                foreground: "#cdd6f4".into(),
                background: "#1e1e2e".into(),
                palette: [
                    "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5",
                    "#bac2de", "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7",
                    "#94e2d5", "#a6adc8",
                ]
                .map(String::from),
            },
        ),
        (
            "catppuccin-latte",
            ThemeConfig {
                font: None,
                font_bold: None,
                fallback_fonts: Vec::new(),
                base_dir: None,
                foreground: "#4c4f69".into(),
                background: "#eff1f5".into(),
                palette: [
                    "#5c5f77", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299",
                    "#acb0be", "#6c6f85", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb",
                    "#179299", "#bcc0cc",
                ]
                .map(String::from),
            },
        ),
        (
            "catppuccin-frappe",
            ThemeConfig {
                font: None,
                font_bold: None,
                fallback_fonts: Vec::new(),
                base_dir: None,
                foreground: "#c6d0f5".into(),
                background: "#303446".into(),
                palette: [
                    "#51576d", "#e78284", "#a6d189", "#e5c890", "#8caaee", "#f4b8e4", "#81c8be",
                    "#b5bfe2", "#626880", "#e78284", "#a6d189", "#e5c890", "#8caaee", "#f4b8e4",
                    "#81c8be", "#a5adce",
                ]
                .map(String::from),
            },
        ),
        (
            "catppuccin-macchiato",
            ThemeConfig {
                font: None,
                font_bold: None,
                fallback_fonts: Vec::new(),
                base_dir: None,
                foreground: "#cad3f5".into(),
                background: "#24273a".into(),
                palette: [
                    "#494d64", "#ed8796", "#a6da95", "#eed49f", "#8aadf4", "#f5bde6", "#8bd5ca",
                    "#b8c0e0", "#5b6078", "#ed8796", "#a6da95", "#eed49f", "#8aadf4", "#f5bde6",
                    "#8bd5ca", "#a5adcb",
                ]
                .map(String::from),
            },
        ),
        (
            "solarized-dark",
            ThemeConfig {
                font: None,
                font_bold: None,
                fallback_fonts: Vec::new(),
                base_dir: None,
                foreground: "#839496".into(),
                background: "#002b36".into(),
                palette: [
                    "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198",
                    "#eee8d5", "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4",
                    "#93a1a1", "#fdf6e3",
                ]
                .map(String::from),
            },
        ),
        (
            "solarized-light",
            ThemeConfig {
                font: None,
                font_bold: None,
                fallback_fonts: Vec::new(),
                base_dir: None,
                foreground: "#657b83".into(),
                background: "#fdf6e3".into(),
                palette: [
                    "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198",
                    "#eee8d5", "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4",
                    "#93a1a1", "#fdf6e3",
                ]
                .map(String::from),
            },
        ),
        (
            "dracula",
            ThemeConfig {
                font: None,
                font_bold: None,
                fallback_fonts: Vec::new(),
                base_dir: None,
                foreground: "#f8f8f2".into(),
                background: "#282a36".into(),
                palette: [
                    "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd",
                    "#f8f8f2", "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df",
                    "#a4ffff", "#ffffff",
                ]
                .map(String::from),
            },
        ),
        (
            "nord",
            ThemeConfig {
                font: None,
                font_bold: None,
                fallback_fonts: Vec::new(),
                base_dir: None,
                foreground: "#d8dee9".into(),
                background: "#2e3440".into(),
                palette: [
                    "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0",
                    "#e5e9f0", "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead",
                    "#8fbcbb", "#eceff4",
                ]
                .map(String::from),
            },
        ),
        (
            "gruvbox-dark",
            ThemeConfig {
                font: None,
                font_bold: None,
                fallback_fonts: Vec::new(),
                base_dir: None,
                foreground: "#ebdbb2".into(),
                background: "#282828".into(),
                palette: [
                    "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a",
                    "#a89984", "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b",
                    "#8ec07c", "#ebdbb2",
                ]
                .map(String::from),
            },
        ),
        (
            "tokyo-night",
            ThemeConfig {
                font: None,
                font_bold: None,
                fallback_fonts: Vec::new(),
                base_dir: None,
                foreground: "#a9b1d6".into(),
                background: "#1a1b26".into(),
                palette: [
                    "#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff",
                    "#a9b1d6", "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7",
                    "#7dcfff", "#c0caf5",
                ]
                .map(String::from),
            },
        ),
    ];

    builtins
        .into_iter()
        .map(|(name, theme)| (name.to_string(), theme))
        .collect()
}

/// Expand a leading `~` in a path to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return PathBuf::from(std::env::var("HOME").unwrap_or_default());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// Expand `~` and resolve a relative font path against the directory the theme
/// was loaded from. Shared by the primary, bold, and fallback font paths so
/// every font a theme declares is resolved identically.
fn resolve_font_path_raw(path: &str, base_dir: Option<&Path>) -> PathBuf {
    let expanded = expand_tilde(path);
    if expanded.is_absolute() {
        expanded
    } else if let Some(base) = base_dir {
        base.join(expanded)
    } else {
        expanded
    }
}

/// Resolve a font path from a theme: expand `~`, resolve relative paths against
/// the theme's directory, and verify the file exists. Returns None (with a
/// warning) when the font is missing so the caller falls back to the embedded
/// font.
fn resolve_font_path(path: &str, base_dir: Option<&Path>) -> Option<PathBuf> {
    let resolved = resolve_font_path_raw(path, base_dir);
    if resolved.exists() {
        Some(resolved)
    } else {
        tracing::warn!(
            "Font '{}' not found (resolved to {:?}), falling back to embedded font",
            path,
            resolved
        );
        None
    }
}

/// Resolve a configured fallback font path. Missing files are skipped with a
/// warning: a broken entry in `fallback_fonts` must never stop the renderer
/// from starting, it only shrinks the fallback chain.
fn resolve_fallback_font_path(path: &str, base_dir: Option<&Path>) -> Option<PathBuf> {
    let resolved = resolve_font_path_raw(path, base_dir);
    if resolved.exists() {
        Some(resolved)
    } else {
        tracing::warn!(
            "Fallback font '{}' not found (resolved to {:?}), ignoring it",
            path,
            resolved
        );
        None
    }
}

/// Load every `.toml` file in the themes directory as a theme. The file name
/// (without extension) is the theme name.
fn load_user_themes(themes_dir: &Path) -> Result<Vec<(String, ThemeConfig)>> {
    let mut themes = Vec::new();
    if !themes_dir.is_dir() {
        return Ok(themes);
    }

    for entry in std::fs::read_dir(themes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read theme {:?}: {}", path, e);
                continue;
            }
        };
        match toml::from_str::<ThemeConfig>(&contents) {
            Ok(mut theme) => {
                theme.base_dir = Some(themes_dir.to_path_buf());
                themes.push((name, theme));
            }
            Err(e) => tracing::warn!("Failed to parse theme {:?}: {}", path, e),
        }
    }

    Ok(themes)
}

/// Write default config files on first run: the config directory, the themes
/// and rules directories, and a starter `config.toml`. No themes are written -
/// the built-in themes cover the defaults, and `themes/` is left empty for the
/// user's own theme files. Existing files are never overwritten.
fn bootstrap_defaults(config_dir: &Path) -> Result<()> {
    // Created empty: any .toml theme file dropped in here is picked up
    // automatically, the same way rules/ works.
    std::fs::create_dir_all(config_dir.join("themes"))?;
    std::fs::create_dir_all(user_rules_dir(config_dir))?;

    let config_path = config_dir.join("config.toml");
    if !config_path.exists() {
        std::fs::write(&config_path, DEFAULT_CONFIG_TOML)?;
        tracing::info!("Wrote default config to {:?}", config_path);
    }

    Ok(())
}

const DEFAULT_CONFIG_TOML: &str = r##"# termshot configuration
# The built-in themes use the bundled JetBrains Mono font, so this works with no
# extra downloads. Drop your own theme files in themes/ to add more - see
# docs/themes.md for the format and per-theme font options.
default_theme = "dark"
font_size = 16.0
cols = 120
rows = 40
timeout_secs = 30

# Embed the terminal text (redacted, when redaction ran) in each PNG's UTF-8
# `Description` metadata (a PNG `iTXt` chunk, so box drawing and non-Latin
# scripts survive), so screenshots are readable by screen readers.
embed_description = true

[chrome]
enabled = false
preset = "none"
title = "termshot"
timestamp = false
# The drop shadow costs extra render time on large captures; off by default.
shadow = false
radius = 14
rounded = true
outer_padding = 0
title_bar_height = 34

[redaction]
# --- Master switch -----------------------------------------------------------
# Set to false to disable ALL redaction globally: no rule ever runs, and even
# --redact (CLI) / redact: true (MCP) become no-ops. This is the single knob
# for "turn everything off".
enabled = true

# --- Automatic redaction -----------------------------------------------------
# Redact every screenshot without an explicit flag. Off by default: a false
# positive silently mangles a screenshot of ordinary output, which is worse
# than an unmasked capture you chose not to redact. With auto = true the PNG is
# masked while the returned text stays original unless you pass --redact-text.
auto = false

# Redaction block/label colors (defaults: red block, black label).
# color = "#d41919"
# label_color = "#000000"

# Custom rules: every .toml / .yaml file in ~/.config/termshot/rules is loaded
# automatically (that directory is created on first run, like themes/). Point
# at an additional directory here, or with --rules-path on the CLI; rules
# loaded from it override same-named rules from the default directory.
# rules_path = "~/work/termshot-rules"

# Built-in rules (ipv4, ipv6, mac, aws_key, aws_secret, private_key, jwt,
# email, hostname, api_key, and the provider token rules) are compiled in and
# enabled by default.
# Override one by re-declaring it with the same name, or disable it:
#
#   [[redaction.rules]]
#   name = "email"
#   enabled = false
#
# Add your own rules with a name, pattern, and replacement:
#
#   [[redaction.rules]]
#   name = "ticket"
#   pattern = 'TICKET-\d+'
#   replacement = "[REDACTED-TICKET]"
#   enabled = true
#
# Give a rule a custom color:
#
#   [[redaction.rules]]
#   name = "ipv4"
#   color = "#ff6600"

# Optional overrides
# output_dir = "/tmp/termshot"
# font_path = "/path/to/your/monospace.ttf"
# shell = "/bin/bash"
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// A rule file dropped into `<config>/rules` must be picked up with no
    /// configuration at all, the way `themes/` works.
    #[test]
    fn user_rules_directory_is_loaded_by_default() {
        let dir = Path::new("target/config-test/default-rules");
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(user_rules_dir(dir)).unwrap();
        std::fs::write(
            user_rules_dir(dir).join("tickets.toml"),
            "[[rules]]\nname = \"ticket\"\npattern = 'TICKET-\\\\d+'\nreplacement = \"[REDACTED-TICKET]\"\n",
        )
        .unwrap();

        let mut redaction = RedactionConfig::default();
        merge_rule_dirs(&mut redaction, dir, None);

        assert!(
            redaction.rules.iter().any(|r| r.name == "ticket"),
            "rule from the default directory was not loaded: {:?}",
            redaction.rules
        );
    }

    /// An explicit `--rules-path` is loaded on top of the default directory,
    /// and pointing it at that same directory must not load it twice.
    #[test]
    fn explicit_rules_path_adds_to_the_default_directory() {
        let dir = Path::new("target/config-test/explicit-rules");
        let extra = Path::new("target/config-test/explicit-extra");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(extra);
        std::fs::create_dir_all(user_rules_dir(dir)).unwrap();
        std::fs::create_dir_all(extra).unwrap();
        std::fs::write(
            user_rules_dir(dir).join("a.toml"),
            "[[rules]]\nname = \"from_default\"\npattern = 'AAA'\nreplacement = \"[A]\"\n",
        )
        .unwrap();
        std::fs::write(
            extra.join("b.toml"),
            "[[rules]]\nname = \"from_explicit\"\npattern = 'BBB'\nreplacement = \"[B]\"\n",
        )
        .unwrap();

        let mut redaction = RedactionConfig::default();
        merge_rule_dirs(&mut redaction, dir, Some(extra.to_str().unwrap()));
        let names: Vec<&str> = redaction.rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["from_default", "from_explicit"]);

        // Same directory twice: loaded once.
        let mut redaction = RedactionConfig::default();
        merge_rule_dirs(
            &mut redaction,
            dir,
            Some(user_rules_dir(dir).to_str().unwrap()),
        );
        assert_eq!(redaction.rules.len(), 1);
    }

    /// First run creates the rules directory alongside themes/.
    #[test]
    fn bootstrap_creates_the_rules_directory() {
        let dir = Path::new("target/config-test/bootstrap");
        let _ = std::fs::remove_dir_all(dir);
        bootstrap_defaults(dir).unwrap();
        assert!(user_rules_dir(dir).is_dir(), "rules/ was not created");
        assert!(dir.join("themes").is_dir(), "themes/ was not created");
        assert!(dir.join("config.toml").is_file());
    }

    /// A theme may list extra fallback fonts; the field is optional, so themes
    /// written before it existed still parse.
    #[test]
    fn theme_fallback_fonts_are_optional_and_parsed() {
        let palette = r##"palette = ["#000000","#000000","#000000","#000000","#000000","#000000","#000000","#000000","#000000","#000000","#000000","#000000","#000000","#000000","#000000","#000000"]"##;

        let without: ThemeConfig = toml::from_str(&format!(
            "foreground = \"#ffffff\"\nbackground = \"#000000\"\n{}\n",
            palette
        ))
        .expect("theme without fallback_fonts parses");
        assert!(without.fallback_fonts.is_empty());

        let with: ThemeConfig = toml::from_str(&format!(
            "foreground = \"#ffffff\"\nbackground = \"#000000\"\nfont = \"~/.local/share/fonts/MyMono-Regular.otf\"\nfallback_fonts = [\"/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc\", \"extra.ttf\"]\n{}\n",
            palette
        ))
        .expect("theme with fallback_fonts parses");
        assert_eq!(
            with.fallback_fonts,
            vec![
                "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc".to_string(),
                "extra.ttf".to_string()
            ]
        );
    }

    /// Fallback font paths go through the same resolution as the primary
    /// fonts: `~` is expanded, relative paths resolve against the theme
    /// directory, and entries that do not exist are dropped instead of
    /// breaking the theme.
    #[test]
    fn theme_fallback_font_paths_resolve_like_primary_fonts() {
        let dir = Path::new("target/config-test/fallback-fonts");
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let present = dir.join("present.ttf");
        std::fs::write(&present, b"not really a font, only its presence matters").unwrap();

        let home = std::env::var("HOME").unwrap_or_default();
        let home_font = Path::new(&home).join(".config/termshot/does-not-exist.ttf");

        let theme = ThemeConfig {
            foreground: "#ffffff".into(),
            background: "#000000".into(),
            font: None,
            font_bold: None,
            fallback_fonts: vec![
                "present.ttf".into(),
                "~/.config/termshot/does-not-exist.ttf".into(),
                "/definitely/missing/font.ttf".into(),
            ],
            palette: ["#000000"; 16].map(String::from),
            base_dir: Some(dir.to_path_buf()),
        };

        let mut config = Config {
            output_dir: dir.to_path_buf(),
            font_path: None,
            font_size: 16.0,
            default_cols: 80,
            default_rows: 24,
            default_timeout_secs: 30,
            shell: "/bin/bash".to_string(),
            embed_description: true,
            default_theme: "custom".to_string(),
            chrome: ChromeConfig::default(),
            themes: HashMap::new(),
            user_theme_names: BTreeSet::new(),
            redaction: RedactionConfig::default(),
        };
        config.themes.insert("custom".to_string(), theme);

        // The relative path resolved against the theme directory is kept; the
        // two missing files (one via `~`) are dropped with a warning.
        assert_eq!(
            config.theme_fallback_font_paths("custom"),
            vec![present.clone()]
        );
        assert!(!home_font.exists(), "test assumed this path is absent");

        // Unknown theme: no fallbacks, no panic.
        assert!(config.theme_fallback_font_paths("nope").is_empty());
    }

    /// First run must not install any theme files: `themes/` is created empty
    /// for the user's own themes, and the shipped default stays the built-in
    /// `dark`, which needs no external font.
    #[test]
    fn bootstrap_writes_no_theme_files() {
        let dir = Path::new("target/config-test/no-starter-theme");
        let _ = std::fs::remove_dir_all(dir);
        bootstrap_defaults(dir).unwrap();

        let themes_dir = dir.join("themes");
        assert!(themes_dir.is_dir(), "themes/ was not created");
        let entries: Vec<PathBuf> = std::fs::read_dir(&themes_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(
            entries.is_empty(),
            "bootstrap must not ship theme files, found {:?}",
            entries
        );

        let default_config: ConfigFile =
            toml::from_str(&std::fs::read_to_string(dir.join("config.toml")).unwrap()).unwrap();
        assert_eq!(default_config.default_theme, "dark");
        assert!(builtin_themes().contains_key("dark"));
    }
}
