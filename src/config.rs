use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

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
    /// Name of the default theme to use.
    pub default_theme: String,
    /// Custom theme definitions (keyed by name).
    pub themes: HashMap<String, ThemeConfig>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            output_dir: "/tmp/screenshot-mcp".to_string(),
            font_path: None,
            font_size: 16.0,
            cols: 120,
            rows: 40,
            timeout_secs: 30,
            shell: None,
            default_theme: "dark".to_string(),
            themes: HashMap::new(),
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
    /// 16-color ANSI palette as ["#RRGGBB", ...].
    pub palette: [String; 16],
}

/// Resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub output_dir: PathBuf,
    pub font_path: PathBuf,
    pub font_size: f32,
    pub default_cols: u16,
    pub default_rows: u16,
    pub default_timeout_secs: u64,
    pub shell: String,
    pub default_theme: String,
    pub themes: HashMap<String, ThemeConfig>,
}

impl Config {
    /// Load config by merging (in order): defaults, config file, env vars.
    pub fn load(config_path: Option<&str>) -> Result<Self> {
        // Start with defaults
        let mut file_config = ConfigFile::default();

        // Try loading config file
        let paths_to_try = if let Some(p) = config_path {
            vec![PathBuf::from(p)]
        } else {
            vec![
                dirs_config().join("termshot").join("config.toml"),
                PathBuf::from("termshot.toml"),
            ]
        };

        for path in &paths_to_try {
            if path.exists() {
                let contents = std::fs::read_to_string(path)?;
                file_config = toml::from_str(&contents)?;
                tracing::info!("Loaded config from {:?}", path);
                break;
            }
        }

        // Inject built-in themes (user themes in config file override these)
        inject_builtin_themes(&mut file_config.themes);

        // Env var overrides
        let output_dir = std::env::var("SCREENSHOT_MCP_OUTPUT_DIR")
            .map(|s| PathBuf::from(s))
            .unwrap_or_else(|_| PathBuf::from(&file_config.output_dir));

        let font_path = std::env::var("SCREENSHOT_MCP_FONT_PATH")
            .map(PathBuf::from)
            .or_else(|_| file_config.font_path.as_ref().map(PathBuf::from).ok_or(()))
            .unwrap_or_else(|_| find_bundled_font());

        let font_size = std::env::var("SCREENSHOT_MCP_FONT_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(file_config.font_size);

        let default_cols = std::env::var("SCREENSHOT_MCP_COLS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(file_config.cols);

        let default_rows = std::env::var("SCREENSHOT_MCP_ROWS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(file_config.rows);

        let default_timeout_secs = std::env::var("SCREENSHOT_MCP_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(file_config.timeout_secs);

        let shell = std::env::var("SCREENSHOT_MCP_SHELL")
            .ok()
            .or(file_config.shell)
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/bash".to_string());

        let default_theme =
            std::env::var("SCREENSHOT_MCP_THEME").unwrap_or(file_config.default_theme);

        std::fs::create_dir_all(&output_dir)?;

        Ok(Self {
            output_dir,
            font_path,
            font_size,
            default_cols,
            default_rows,
            default_timeout_secs,
            shell,
            default_theme,
            themes: file_config.themes,
        })
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

fn find_bundled_font() -> PathBuf {
    // Check relative to the executable
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let bundled = exe_dir.join("fonts").join("JetBrainsMono-Regular.ttf");
            if bundled.exists() {
                return bundled;
            }
            // cargo run: target/debug/../.. = project root
            if let Some(parent) = exe_dir.parent() {
                if let Some(grandparent) = parent.parent() {
                    let bundled = grandparent.join("fonts").join("JetBrainsMono-Regular.ttf");
                    if bundled.exists() {
                        return bundled;
                    }
                }
            }
        }
    }
    let cwd_font = PathBuf::from("fonts/JetBrainsMono-Regular.ttf");
    if cwd_font.exists() {
        return cwd_font;
    }
    PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf")
}

/// Insert built-in themes. User-defined themes with the same name take priority.
fn inject_builtin_themes(themes: &mut HashMap<String, ThemeConfig>) {
    let builtins: Vec<(&str, ThemeConfig)> = vec![
        (
            "dark",
            ThemeConfig {
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
            "adamkadaban",
            ThemeConfig {
                // Grabbed from GNOME Terminal profile b1dcc9dd-5262-4d8d-a863-c897e6d979b9
                foreground: "#ffffff".into(),
                background: "#171421".into(),
                palette: [
                    "#171421", "#d41919", "#5ebdab", "#fea44c", "#367bf0", "#9755b3", "#49aee6",
                    "#d0cfcc", "#aa27ac", "#d41919", "#47d4b9", "#ff8a18", "#277fff", "#962ac3",
                    "#05a1f7", "#ffffff",
                ]
                .map(String::from),
            },
        ),
        (
            "catppuccin-mocha",
            ThemeConfig {
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

    for (name, theme) in builtins {
        themes.entry(name.to_string()).or_insert(theme);
    }
}
