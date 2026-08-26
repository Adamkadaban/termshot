use crate::config::{ChromeConfig, ThemeConfig};
use crate::redaction::{RedactionEngine, RedactionMap};
use anyhow::{Context, Result};
use chrono::Utc;
use fontdue::{Font, FontSettings};
use image::{ImageBuffer, Rgba, RgbaImage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use vt100::Screen;

/// JetBrains Mono font compiled directly into the binary so that release
/// archives and `cargo install` builds always have a working font, even when
/// the `fonts/` directory is not shipped alongside the executable.
const EMBEDDED_FONT: &[u8] = include_bytes!("../fonts/JetBrainsMono-Regular.ttf");

/// Default background color drawn behind redacted cells (bright red, #d41919)
/// when a rule does not specify its own color. Kept in sync with
/// [`crate::redaction::DEFAULT_BLOCK_COLOR`]; used as a reference in tests.
#[cfg(test)]
const REDACT_BG: Rgba<u8> = Rgba([212, 25, 25, 255]);

/// Supersampling factor: the whole image (terminal + chrome) is rendered at
/// this multiple of the base cell/chrome metrics for crisp, retina-quality
/// output. Both `render_screen` and `compose_with_chrome` must use the same
/// factor so terminal content and chrome stay proportional.
const RENDER_SCALE: u32 = 2;

/// Metadata describing how a screenshot was rendered, so it can later be
/// re-rendered (e.g. by the `redact_screenshot` MCP tool) with identical
/// geometry and styling. The MCP server keeps this in memory (alongside the
/// raw terminal bytes) for the lifetime of the process; it is not written to
/// disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RenderMeta {
    pub cols: u16,
    pub rows: u16,
    pub theme: Option<String>,
    pub chrome: Option<ChromeOptions>,
    /// Whether the image width was auto-cropped to fit the content. Persisted
    /// so a later re-render (e.g. `redact_screenshot`) reproduces the same
    /// geometry. Defaults to true for metadata written before this field
    /// existed.
    #[serde(default = "default_auto_crop")]
    pub auto_crop: bool,
}

fn default_auto_crop() -> bool {
    true
}

/// Result of rendering: output PNG path, plain text, per-rule redaction
/// audit counts (empty when no redaction was applied), and the render metadata
/// needed to later re-render (e.g. for `redact_screenshot`).
pub type RenderOutput = (PathBuf, String, Vec<(String, usize)>, RenderMeta);

/// How [`compose_images`] arranges multiple source screenshots on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeLayout {
    /// Place images left-to-right; canvas height is the tallest image.
    Horizontal,
    /// Stack images top-to-bottom; canvas width is the widest image.
    Vertical,
}

impl ComposeLayout {
    /// Parse a user-supplied layout name. Accepts a few friendly aliases.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "horizontal" | "h" | "row" | "side-by-side" => Ok(Self::Horizontal),
            "vertical" | "v" | "column" | "stacked" => Ok(Self::Vertical),
            other => anyhow::bail!(
                "unknown compose layout '{}': use 'horizontal' or 'vertical'",
                other
            ),
        }
    }
}

/// Muted divider color drawn between panes on a dark background. Chosen to
/// read like a tmux pane border (tmux's default `colour8` grey) so the seam is
/// clearly visible without competing with the terminal content.
const DIVIDER_COLOR_DARK: Rgba<u8> = Rgba([0x6e, 0x76, 0x80, 255]);
/// Muted divider color drawn between panes on a light background.
const DIVIDER_COLOR_LIGHT: Rgba<u8> = Rgba([0x9a, 0x9a, 0x9a, 255]);

/// Pick a muted divider color that reads well against `background`, using a
/// perceptual luminance threshold to decide dark vs. light.
fn divider_color_for(background: Rgba<u8>) -> Rgba<u8> {
    let [r, g, b, _] = background.0;
    let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luminance < 128.0 {
        DIVIDER_COLOR_DARK
    } else {
        DIVIDER_COLOR_LIGHT
    }
}

/// Sample the background color of a rendered screenshot so a composite can fill
/// any exposed canvas with a color that matches the panes (never a black or
/// mismatched margin). Prefers a point just inside the top edge - which skips
/// any rounded/transparent corner - and falls back to `fallback` if no fully
/// opaque pixel is found.
fn sample_background(img: &RgbaImage, fallback: Rgba<u8>) -> Rgba<u8> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return fallback;
    }
    let candidates = [
        (w / 2, 2.min(h - 1)),
        (4.min(w - 1), 4.min(h - 1)),
        (2.min(w - 1), h / 2),
        (0, 0),
    ];
    for (x, y) in candidates {
        let p = *img.get_pixel(x, y);
        if p[3] == 255 {
            return Rgba([p[0], p[1], p[2], 255]);
        }
    }
    fallback
}

/// Convert bare LF (`\n` not already preceded by CR) to CRLF so raw ANSI that
/// was not captured through a TTY - piped command output or a redirected log
/// fed to `render` - lands on proper lines instead of staircasing to the right.
///
/// The check is idempotent: input that already uses CRLF (e.g. PTY output from
/// `exec`) contains no bare LF and is returned borrowed without allocating.
fn normalize_newlines(data: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let has_bare_lf = data
        .iter()
        .enumerate()
        .any(|(i, &b)| b == b'\n' && (i == 0 || data[i - 1] != b'\r'));
    if !has_bare_lf {
        return std::borrow::Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut prev = 0u8;
    for &b in data {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    std::borrow::Cow::Owned(out)
}

///
/// `divider` is the divider thickness in pixels. For a horizontal layout the
/// panes are stretched to a common height (the tallest source) and separated by
/// a vertical divider; for a vertical layout they are stretched to a common
/// width (the widest source) and separated by a horizontal divider. Stretching
/// keeps the panes aligned so the seams line up like real terminal splits.
pub fn compose_images(
    paths: &[PathBuf],
    layout: ComposeLayout,
    divider: u32,
    background: Rgba<u8>,
) -> Result<RgbaImage> {
    if paths.len() < 2 {
        anyhow::bail!("compose requires at least two images (got {})", paths.len());
    }

    let mut sources: Vec<RgbaImage> = Vec::with_capacity(paths.len());
    for p in paths {
        let img = image::open(p)
            .with_context(|| format!("Failed to open image {:?}", p))?
            .to_rgba8();
        sources.push(img);
    }

    // Fill any exposed canvas (and choose the divider color) from the first
    // pane's own background, so the composite never shows a black or otherwise
    // mismatched margin. Falls back to the caller-supplied background.
    let canvas_bg = sample_background(&sources[0], background);

    // Stretch every pane to a common cross-axis size so the panes align.
    let filter = image::imageops::FilterType::Lanczos3;
    let panes: Vec<RgbaImage> = match layout {
        ComposeLayout::Horizontal => {
            let target_h = sources.iter().map(|i| i.height()).max().unwrap_or(1).max(1);
            sources
                .into_iter()
                .map(|img| {
                    if img.height() == target_h {
                        img
                    } else {
                        image::imageops::resize(&img, img.width().max(1), target_h, filter)
                    }
                })
                .collect()
        }
        ComposeLayout::Vertical => {
            let target_w = sources.iter().map(|i| i.width()).max().unwrap_or(1).max(1);
            sources
                .into_iter()
                .map(|img| {
                    if img.width() == target_w {
                        img
                    } else {
                        image::imageops::resize(&img, target_w, img.height().max(1), filter)
                    }
                })
                .collect()
        }
    };

    let total_divider = divider * (panes.len() as u32 - 1);
    let (width, height) = match layout {
        ComposeLayout::Horizontal => (
            panes.iter().map(|i| i.width()).sum::<u32>() + total_divider,
            panes.iter().map(|i| i.height()).max().unwrap_or(0),
        ),
        ComposeLayout::Vertical => (
            panes.iter().map(|i| i.width()).max().unwrap_or(0),
            panes.iter().map(|i| i.height()).sum::<u32>() + total_divider,
        ),
    };

    let divider_color = divider_color_for(canvas_bg);
    let mut canvas: RgbaImage = ImageBuffer::from_pixel(width.max(1), height.max(1), canvas_bg);
    let mut offset = 0u32;
    for (idx, src) in panes.iter().enumerate() {
        match layout {
            ComposeLayout::Horizontal => {
                image::imageops::overlay(&mut canvas, src, offset as i64, 0);
                offset += src.width();
                if idx + 1 < panes.len() && divider > 0 {
                    fill_rect(&mut canvas, offset, 0, divider, height, divider_color);
                    offset += divider;
                }
            }
            ComposeLayout::Vertical => {
                image::imageops::overlay(&mut canvas, src, 0, offset as i64);
                offset += src.height();
                if idx + 1 < panes.len() && divider > 0 {
                    fill_rect(&mut canvas, 0, offset, width, divider, divider_color);
                    offset += divider;
                }
            }
        }
    }

    Ok(canvas)
}

/// Fill an axis-aligned rectangle on `canvas` with a solid `color`, clipping to
/// the canvas bounds.
fn fill_rect(canvas: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    let (cw, ch) = canvas.dimensions();
    let x_end = (x + w).min(cw);
    let y_end = (y + h).min(ch);
    for py in y..y_end {
        for px in x..x_end {
            canvas.put_pixel(px, py, color);
        }
    }
}

/// A request to redact a screenshot: the compiled rule engine plus an optional
/// filter limiting which rule names apply.
pub struct RedactionRequest<'a> {
    pub engine: &'a RedactionEngine,
    pub rules: Option<Vec<String>>,
}

/// Controls how the returned plain text is produced.
///
/// By default (`strip_ansi = false`, `redact_text = false`) the *original*
/// terminal output is returned with ANSI color codes intact, so an agent sees
/// exactly what was on screen and can decide what to redact. Redaction still
/// always applies to the rendered PNG regardless of these options.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextOptions {
    /// Strip ANSI escape codes from the returned text (return plain text).
    pub strip_ansi: bool,
    /// Also apply redaction to the returned text (mask matches with blocks).
    pub redact_text: bool,
    /// Embed the terminal text in the PNG's `Description` metadata so the
    /// screenshot is readable by screen readers. When redaction ran, the
    /// *redacted* text is embedded so the image never carries the secrets its
    /// pixels hide.
    pub embed_description: bool,
}

const MAX_TITLE_CHARS: usize = 60;

/// Parsed RGBA theme ready for rendering.
#[derive(Debug, Clone)]
pub struct Theme {
    pub foreground: Rgba<u8>,
    pub background: Rgba<u8>,
    pub palette: [Rgba<u8>; 16],
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChromeOptions {
    pub enabled: bool,
    pub preset: String,
    pub title: Option<String>,
    pub timestamp: bool,
    pub shadow: bool,
    pub radius: u32,
    /// Draw soft rounded corners. Independent of `enabled`: with chrome the
    /// window frame is rounded; without chrome the terminal image itself is
    /// given rounded corners (transparent outside the curve). Defaults to
    /// `true`; the serde default keeps older [`RenderMeta`] records working.
    #[serde(default = "default_true")]
    pub rounded: bool,
    pub outer_padding: u32,
    pub title_bar_height: u32,
}

fn default_true() -> bool {
    true
}

impl ChromeOptions {
    pub fn from_config(config: &ChromeConfig) -> Self {
        Self {
            enabled: config.enabled && config.preset != "none",
            preset: config.preset.clone(),
            title: config.title.clone(),
            timestamp: config.timestamp,
            shadow: config.shadow,
            radius: config.radius,
            rounded: config.rounded,
            outer_padding: config.outer_padding,
            title_bar_height: config.title_bar_height,
        }
    }
}

impl Theme {
    /// Parse a ThemeConfig (hex strings) into an RGBA Theme.
    pub fn from_config(config: &ThemeConfig) -> Result<Self> {
        Ok(Self {
            foreground: parse_hex_color(&config.foreground)?,
            background: parse_hex_color(&config.background)?,
            palette: {
                let mut arr = [Rgba([0, 0, 0, 255]); 16];
                for (i, hex) in config.palette.iter().enumerate() {
                    arr[i] = parse_hex_color(hex)?;
                }
                arr
            },
        })
    }

    /// Fallback dark theme (no config needed).
    pub fn dark() -> Self {
        Self {
            foreground: Rgba([204, 204, 204, 255]),
            background: Rgba([30, 30, 30, 255]),
            palette: [
                Rgba([30, 30, 30, 255]),
                Rgba([204, 0, 0, 255]),
                Rgba([78, 154, 6, 255]),
                Rgba([196, 160, 0, 255]),
                Rgba([52, 101, 164, 255]),
                Rgba([117, 80, 123, 255]),
                Rgba([6, 152, 160, 255]),
                Rgba([211, 215, 207, 255]),
                Rgba([85, 87, 83, 255]),
                Rgba([239, 41, 41, 255]),
                Rgba([138, 226, 52, 255]),
                Rgba([252, 233, 79, 255]),
                Rgba([114, 159, 207, 255]),
                Rgba([173, 127, 168, 255]),
                Rgba([52, 226, 226, 255]),
                Rgba([238, 238, 236, 255]),
            ],
        }
    }
}

fn parse_hex_color(hex: &str) -> Result<Rgba<u8>> {
    let hex = hex.trim().trim_start_matches('#');
    // Guard on ASCII hex digits before slicing: a non-ASCII string (e.g.
    // "#abc\u{20ac}") is six *bytes* but slicing it would split a UTF-8
    // character and panic.
    anyhow::ensure!(
        hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid hex color: #{}",
        hex
    );
    let r = u8::from_str_radix(&hex[0..2], 16)?;
    let g = u8::from_str_radix(&hex[2..4], 16)?;
    let b = u8::from_str_radix(&hex[4..6], 16)?;
    Ok(Rgba([r, g, b, 255]))
}

/// Font ascent (baseline offset from the top of the line box) at `size`,
/// with a proportional fallback for fonts that carry no horizontal line
/// metrics, so an unusual but parseable font cannot panic the renderer.
fn font_ascent(font: &Font, size: f32) -> f32 {
    font.horizontal_line_metrics(size)
        .map(|m| m.ascent)
        .unwrap_or(size * 0.8)
}

/// Line height (baseline-to-baseline advance) at `size`, with a fallback for
/// fonts that carry no horizontal line metrics.
fn font_line_height(font: &Font, size: f32) -> f32 {
    font.horizontal_line_metrics(size)
        .map(|m| m.new_line_size)
        .unwrap_or(size * 1.2)
}

/// Save an RGBA image as a PNG, optionally embedding `description` as a
/// `tEXt` chunk under the standard `Description` keyword so screen readers and
/// other assistive tooling can read the terminal text back out of the image.
///
/// The text is normalized to the Latin-1 subset that PNG `tEXt` allows
/// (non-representable characters become `?`) and capped at
/// [`MAX_DESCRIPTION_BYTES`].
pub fn save_png(img: &RgbaImage, path: &Path, description: Option<&str>) -> Result<()> {
    let file = std::fs::File::create(path)
        .with_context(|| format!("Failed to create image file {:?}", path))?;
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, img.width(), img.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Some(text) = description {
        let text = latin1_text_chunk(text);
        if !text.is_empty() {
            encoder
                .add_text_chunk("Description".to_string(), text)
                .with_context(|| format!("Failed to embed description in {:?}", path))?;
        }
    }
    let mut writer = encoder
        .write_header()
        .with_context(|| format!("Failed to write PNG header for {:?}", path))?;
    writer
        .write_image_data(img.as_raw())
        .with_context(|| format!("Failed to write PNG data for {:?}", path))?;
    writer
        .finish()
        .with_context(|| format!("Failed to finalize PNG {:?}", path))?;
    Ok(())
}

/// Maximum size of an embedded `Description` text chunk. Long captures are
/// truncated so a screenshot's metadata never dwarfs its pixels.
const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;

/// Convert arbitrary text into the Latin-1 subset PNG `tEXt` chunks accept:
/// newlines are kept, other control characters are dropped, redaction blocks
/// (`█`) become `#` so masked spans stay obvious, and any other codepoint
/// above U+00FF becomes `?`. The result is capped at
/// [`MAX_DESCRIPTION_BYTES`].
fn latin1_text_chunk(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_DESCRIPTION_BYTES));
    for ch in text.chars() {
        let mapped = match ch {
            '\n' => '\n',
            '\u{2588}' => '#',
            c if (c as u32) < 0x20 || (c as u32 == 0x7f) => continue,
            c if (c as u32) <= 0xff => c,
            _ => '?',
        };
        if out.len() + mapped.len_utf8() > MAX_DESCRIPTION_BYTES {
            break;
        }
        out.push(mapped);
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Renders a vt100 screen buffer to a PNG image.
pub struct Renderer {
    font: Font,
    font_bold: Option<Font>,
    font_size: f32,
    cell_width: u32,
    cell_height: u32,
    themes: HashMap<String, Theme>,
    default_theme: String,
    default_chrome: ChromeOptions,
    padding: u32,
}

impl Renderer {
    pub fn new(
        font_path: Option<&Path>,
        font_bold_path: Option<&Path>,
        font_size: f32,
        theme_configs: &HashMap<String, ThemeConfig>,
        default_theme: &str,
        chrome_config: &ChromeConfig,
    ) -> Result<Self> {
        // Use an explicitly configured font file if provided, otherwise fall
        // back to the font embedded in the binary.
        let font_data = match font_path {
            Some(path) => {
                std::fs::read(path).with_context(|| format!("Failed to read font: {:?}", path))?
            }
            None => EMBEDDED_FONT.to_vec(),
        };
        let font = Font::from_bytes(font_data, FontSettings::default())
            .map_err(|e| anyhow::anyhow!("Failed to parse font: {}", e))?;

        // Load bold font if provided
        let font_bold = if let Some(path) = font_bold_path {
            let data = std::fs::read(path)
                .with_context(|| format!("Failed to read bold font: {:?}", path))?;
            Some(
                Font::from_bytes(data, FontSettings::default())
                    .map_err(|e| anyhow::anyhow!("Failed to parse bold font: {}", e))?,
            )
        } else {
            None
        };

        let metrics = font.metrics('M', font_size);
        let cell_width = metrics.advance_width.ceil() as u32;
        let cell_height = font_line_height(&font, font_size).ceil() as u32;

        // Parse all themes
        let mut themes = HashMap::new();
        for (name, config) in theme_configs {
            match Theme::from_config(config) {
                Ok(theme) => {
                    themes.insert(name.clone(), theme);
                }
                Err(e) => {
                    tracing::warn!("Failed to parse theme '{}': {}", name, e);
                }
            }
        }
        // Always have a fallback
        themes.entry("dark".to_string()).or_insert_with(Theme::dark);

        Ok(Self {
            font,
            font_bold,
            font_size,
            cell_width,
            cell_height,
            themes,
            default_theme: default_theme.to_string(),
            default_chrome: ChromeOptions::from_config(chrome_config),
            padding: 16,
        })
    }

    /// Get a theme by name, falling back to default then "dark".
    pub fn get_theme(&self, name: Option<&str>) -> &Theme {
        let name = name.unwrap_or(&self.default_theme);
        self.themes
            .get(name)
            .or_else(|| self.themes.get(&self.default_theme))
            .or_else(|| self.themes.get("dark"))
            .expect("no themes available")
    }

    /// List available theme names.
    pub fn theme_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.themes.keys().cloned().collect();
        names.sort();
        names
    }

    /// Render raw ANSI bytes to a PNG image.
    /// Returns the path to the saved image, the plain text content, the
    /// per-rule redaction audit counts (empty when no redaction was applied),
    /// and the [`RenderMeta`] describing how it was rendered.
    ///
    /// `output_name` is an optional descriptive base name (typically the
    /// command line) used to derive the PNG filename; it is sanitized and made
    /// unique within `output_dir`. When `None`, a generic name is used.
    ///
    /// Redaction (when `redaction` is supplied) always masks the rendered PNG.
    /// The returned text is controlled by `text`: by default it is the original
    /// output with ANSI colors intact; set `redact_text` to also scrub it, or
    /// `strip_ansi` to return plain (color-free) text.
    ///
    /// No sidecar files are written; callers that need to re-render (e.g. the
    /// MCP server) should keep the raw bytes and returned [`RenderMeta`] in
    /// memory.
    #[allow(clippy::too_many_arguments)]
    pub fn render_bytes(
        &self,
        data: &[u8],
        cols: u16,
        rows: u16,
        output_dir: &Path,
        output_name: Option<&str>,
        theme_name: Option<&str>,
        chrome: Option<&ChromeOptions>,
        redaction: Option<&RedactionRequest>,
        text: TextOptions,
        auto_crop: bool,
    ) -> Result<RenderOutput> {
        // Normalize bare LF to CRLF before feeding the terminal parser so raw,
        // non-TTY captured ANSI (piped output or redirected logs passed to
        // `render`) lands on proper lines instead of staircasing. This is a
        // no-op for PTY-sourced bytes (from `exec`), which already use CRLF,
        // and the original `data` is still used for the returned text.
        let normalized = normalize_newlines(data);
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(&normalized);
        let screen = parser.screen();

        // Redaction pass: scan the parsed buffer before rendering.
        if let Some(req) = redaction {
            tracing::debug!("redaction: {} active rule(s)", req.engine.rule_count());
        }
        let redaction_map =
            redaction.map(|req| req.engine.redact_screen(screen, req.rules.as_deref()));

        if let Some(map) = &redaction_map {
            if !map.is_empty() {
                tracing::info!(
                    "redaction: masked {} cell(s) ({})",
                    map.cell_count(),
                    map.audit_summary()
                );
            }
        }

        let plain_text = self.output_text(data, screen, cols, redaction_map.as_ref(), text);

        let theme = self.get_theme(theme_name);
        let chrome = chrome.unwrap_or(&self.default_chrome);
        let image =
            self.render_to_image(screen, theme, chrome, redaction_map.as_ref(), auto_crop)?;

        let base = sanitize_base_name(output_name.unwrap_or(""));
        let path = unique_png_path(output_dir, &base);
        let description = self.description_text(screen, cols, redaction_map.as_ref(), text);
        save_png(&image, &path, description.as_deref())?;

        let meta = RenderMeta {
            cols,
            rows,
            theme: theme_name.map(str::to_owned),
            chrome: Some(chrome.clone()),
            auto_crop,
        };

        let audit = redaction_map.map(|m| m.counts).unwrap_or_default();
        Ok((path, plain_text, audit, meta))
    }

    /// Re-render a screenshot from its original raw terminal bytes and
    /// [`RenderMeta`], applying a prebuilt redaction map, and overwrite the PNG
    /// in place. Returns the new plain text (controlled by `text`) and
    /// per-label audit counts.
    pub fn render_redaction_to(
        &self,
        data: &[u8],
        meta: &RenderMeta,
        map: &RedactionMap,
        out_path: &Path,
        text: TextOptions,
    ) -> Result<(String, Vec<(String, usize)>)> {
        let mut parser = vt100::Parser::new(meta.rows, meta.cols, 0);
        parser.process(data);
        let screen = parser.screen();

        let plain_text = self.output_text(data, screen, meta.cols, Some(map), text);
        let theme = self.get_theme(meta.theme.as_deref());
        let chrome = meta
            .chrome
            .clone()
            .unwrap_or_else(|| self.default_chrome.clone());
        let image = self.render_to_image(screen, theme, &chrome, Some(map), meta.auto_crop)?;
        let description = self.description_text(screen, meta.cols, Some(map), text);
        save_png(&image, out_path, description.as_deref())?;
        Ok((plain_text, map.counts.clone()))
    }

    /// Compose several existing screenshots into a single image laid out
    /// horizontally or vertically, using the given theme's background color as
    /// the fill. When `output` is `None` a file name is auto-generated in
    /// `output_dir`. Returns the path of the saved composite PNG.
    ///
    /// The inputs should be RAW terminal screenshots (chrome `none`): they are
    /// combined into a single tmux-style split, and when `chrome` is supplied
    /// and enabled the *composed* result is wrapped in one outer window frame,
    /// rather than each pane carrying its own title bar.
    #[allow(clippy::too_many_arguments)]
    pub fn compose_screenshots(
        &self,
        paths: &[PathBuf],
        layout: ComposeLayout,
        divider: u32,
        theme_name: Option<&str>,
        chrome: Option<&ChromeOptions>,
        output_dir: &Path,
        output: Option<&Path>,
    ) -> Result<PathBuf> {
        let theme = self.get_theme(theme_name);
        let composed = compose_images(paths, layout, divider, theme.background)?;
        let composed = match chrome {
            Some(chrome) if chrome.enabled => self.compose_with_chrome(composed, theme, chrome),
            // Frameless composite: round the outer corners (transparent
            // outside the curve) so a bare gallery still gets soft corners
            // instead of a hard rectangle, matching single-screenshot output.
            _ => {
                let mut composed = composed;
                if self.default_chrome.rounded {
                    self.round_image_corners(
                        &mut composed,
                        self.default_chrome.radius * RENDER_SCALE,
                    );
                }
                composed
            }
        };

        let path = match output {
            Some(p) => p.to_path_buf(),
            None => {
                let id = &uuid::Uuid::new_v4().to_string()[..8];
                output_dir.join(format!("termshot_composed_{}.png", id))
            }
        };
        composed
            .save(&path)
            .with_context(|| format!("Failed to save composed image to {:?}", path))?;
        Ok(path)
    }

    /// Compute the plain text to return to the caller.
    ///
    /// Precedence:
    /// * `redact_text` (with matches) -> stripped text with redaction blocks;
    /// * else `strip_ansi` -> plain (color-free) text from the parsed screen;
    /// * else the original raw output with ANSI color codes preserved.
    fn output_text(
        &self,
        data: &[u8],
        screen: &Screen,
        cols: u16,
        redaction: Option<&RedactionMap>,
        opts: TextOptions,
    ) -> String {
        if opts.redact_text {
            if let Some(map) = redaction {
                if !map.is_empty() {
                    return map.redacted_plain_text(screen);
                }
            }
        }
        if opts.strip_ansi {
            screen
                .rows(0, cols)
                .collect::<Vec<String>>()
                .join("\n")
                .trim_end()
                .to_string()
        } else {
            String::from_utf8_lossy(data).trim_end().to_string()
        }
    }

    /// Build the accessibility text embedded in the PNG's `Description`
    /// metadata, or `None` when embedding is disabled.
    ///
    /// This is always ANSI-free plain text, and always the *redacted* version
    /// when redaction masked anything, so the metadata can never leak a secret
    /// the pixels hide.
    fn description_text(
        &self,
        screen: &Screen,
        cols: u16,
        redaction: Option<&RedactionMap>,
        opts: TextOptions,
    ) -> Option<String> {
        if !opts.embed_description {
            return None;
        }
        if let Some(map) = redaction {
            if !map.is_empty() {
                return Some(map.redacted_plain_text(screen));
            }
        }
        Some(
            screen
                .rows(0, cols)
                .collect::<Vec<String>>()
                .join("\n")
                .trim_end()
                .to_string(),
        )
    }

    /// Render a screen (with optional redaction) to a composed RGBA image.
    fn render_to_image(
        &self,
        screen: &Screen,
        theme: &Theme,
        chrome: &ChromeOptions,
        redaction: Option<&RedactionMap>,
        auto_crop: bool,
    ) -> Result<RgbaImage> {
        let terminal_image = self.render_screen(screen, theme, redaction, auto_crop)?;
        if chrome.enabled {
            // With chrome the frame is rounded (or squared when `rounded` is
            // off); see `compose_with_chrome`.
            return Ok(self.compose_with_chrome(terminal_image, theme, chrome));
        }
        // No chrome: optionally round the terminal content itself so a bare
        // screenshot still has soft corners on a transparent background,
        // like macOS window captures or code-screenshot tools.
        let mut img = terminal_image;
        if chrome.rounded {
            self.round_image_corners(&mut img, chrome.radius * RENDER_SCALE);
        }
        Ok(img)
    }

    /// Render a vt100 Screen to an RGBA image.
    /// Renders at 2x resolution internally and downscales for sharper text.
    fn render_screen(
        &self,
        screen: &Screen,
        theme: &Theme,
        redaction: Option<&RedactionMap>,
        auto_crop: bool,
    ) -> Result<RgbaImage> {
        let rows = screen.size().0 as u32;
        let cols = screen.size().1 as u32;

        let content_rows = self.find_content_rows(screen, rows, cols);
        // Optionally crop the image width to the rightmost column that
        // actually holds content, so narrow output doesn't sit in a wide,
        // mostly-empty frame. Skipped when content already fills most of the
        // terminal width, to avoid awkward micro-crops.
        let content_cols = if auto_crop {
            self.find_content_cols(screen, content_rows, cols)
        } else {
            cols
        };

        let scale: u32 = RENDER_SCALE;
        let cell_w = self.cell_width * scale;
        let cell_h = self.cell_height * scale;
        let padding = self.padding * scale;
        let hi_font_size = self.font_size * scale as f32;

        let img_width = content_cols * cell_w + padding * 2;
        let img_height = content_rows * cell_h + padding * 2;

        let mut img: RgbaImage = ImageBuffer::from_pixel(img_width, img_height, theme.background);

        for row in 0..content_rows {
            for col in 0..content_cols {
                if let Some(cell) = screen.cell(row as u16, col as u16) {
                    let x = col * cell_w + padding;
                    let y = row * cell_h + padding;

                    // Redaction: draw a block (with an optional short label)
                    // in place of sensitive cell contents, using the color the
                    // matching rule requested.
                    if let Some(rc) = redaction.and_then(|m| m.get(row as u16, col as u16)) {
                        let block =
                            Rgba([rc.block_color[0], rc.block_color[1], rc.block_color[2], 255]);
                        self.draw_rect(&mut img, x, y, cell_w, cell_h, block);
                        if let Some(label_ch) = rc.label_char {
                            let label = Rgba([
                                rc.label_color[0],
                                rc.label_color[1],
                                rc.label_color[2],
                                255,
                            ]);
                            self.draw_char_with_font(
                                &mut img,
                                x,
                                y,
                                label_ch,
                                label,
                                false,
                                hi_font_size,
                                &self.font,
                            );
                        }
                        continue;
                    }

                    let (fg_color, bg_color) = self.resolve_cell_colors(cell, theme);

                    // Draw background
                    if bg_color != theme.background {
                        self.draw_rect(&mut img, x, y, cell_w, cell_h, bg_color);
                    }

                    // Draw character(s) at 2x font size
                    let ch = cell.contents();
                    if !ch.is_empty() && ch != " " {
                        let use_bold_font = cell.bold() && self.font_bold.is_some();
                        let render_font = if use_bold_font {
                            self.font_bold.as_ref().unwrap()
                        } else {
                            &self.font
                        };
                        for c in ch.chars() {
                            self.draw_char_with_font(
                                &mut img,
                                x,
                                y,
                                c,
                                fg_color,
                                cell.italic(),
                                hi_font_size,
                                render_font,
                            );
                            // Faux bold only when no bold font is available
                            if cell.bold() && !use_bold_font {
                                self.draw_char_with_font(
                                    &mut img,
                                    x + 1,
                                    y,
                                    c,
                                    fg_color,
                                    cell.italic(),
                                    hi_font_size,
                                    &self.font,
                                );
                            }
                        }
                    }

                    // Underline
                    if cell.underline() {
                        let uy = y + cell_h.saturating_sub(2 * scale);
                        self.draw_rect(&mut img, x, uy, cell_w, scale, fg_color);
                    }
                }
            }
        }

        // Output at 2x resolution for crisp, retina-quality rendering
        Ok(img)
    }

    fn compose_with_chrome(
        &self,
        terminal: RgbaImage,
        theme: &Theme,
        chrome: &ChromeOptions,
    ) -> RgbaImage {
        if !chrome.enabled {
            return terminal;
        }

        // Chrome is drawn at the same supersampling factor as the terminal so
        // the title bar, controls, and text stay proportional to the content.
        let scale = RENDER_SCALE;
        let radius = if chrome.rounded {
            chrome.radius * scale
        } else {
            0
        };

        let title_bar = match chrome.preset.as_str() {
            "minimal" => 0,
            _ => chrome.title_bar_height * scale,
        };
        let shadow = if chrome.shadow { 16 * scale } else { 0 };
        let frame_pad = chrome.outer_padding * scale;
        let bottom_pad = if chrome.timestamp {
            frame_pad.max(18 * scale)
        } else {
            frame_pad
        };

        let frame_w = terminal.width() + frame_pad * 2;
        let frame_h = terminal.height() + frame_pad + bottom_pad + title_bar;
        let width = frame_w + shadow;
        let height = frame_h + shadow;

        // Transparent background so rounded corners don't have a colored border
        let mut img: RgbaImage = ImageBuffer::from_pixel(width, height, Rgba([0, 0, 0, 0]));

        let frame_x = shadow / 2;
        let frame_y = shadow / 2;

        if chrome.shadow {
            self.draw_shadow(
                &mut img,
                frame_x,
                frame_y,
                frame_w,
                frame_h,
                radius,
                4 * scale,
                6 * scale,
                12 * scale,
            );
        }

        let frame_bg = match chrome.preset.as_str() {
            "macos" => Rgba([28, 28, 30, 255]),
            "report" => Rgba([18, 20, 24, 255]),
            _ => theme.background,
        };
        self.draw_rounded_rect(
            &mut img, frame_x, frame_y, frame_w, frame_h, radius, frame_bg,
        );

        if title_bar > 0 {
            let title_bg = match chrome.preset.as_str() {
                "macos" => Rgba([44, 44, 46, 255]),
                "gnome" => Rgba([32, 34, 39, 255]),
                "report" => Rgba([30, 32, 36, 255]),
                _ => Rgba([
                    theme.background[0].saturating_add(10),
                    theme.background[1].saturating_add(10),
                    theme.background[2].saturating_add(10),
                    255,
                ]),
            };

            // Title bar painted with rounded top corners that match the
            // frame, and a squared-off bottom edge. Drawing a rounded rect and
            // then squaring the lower body avoids leaving frame-colored wedges
            // in the rounded top corners.
            self.draw_rounded_rect(
                &mut img, frame_x, frame_y, frame_w, title_bar, radius, title_bg,
            );
            if title_bar > radius {
                self.draw_rect(
                    &mut img,
                    frame_x,
                    frame_y + radius,
                    frame_w,
                    title_bar - radius,
                    title_bg,
                );
            }

            self.draw_title_bar_accents(
                &mut img, chrome, frame_x, frame_y, frame_w, title_bar, theme, scale,
            );
            if let Some(title) = chrome.title.as_deref() {
                let title = truncate_title(title);
                self.draw_text_line(
                    &mut img,
                    frame_x + frame_w / 2,
                    frame_y + title_bar / 2,
                    &title,
                    theme.foreground,
                    self.font_size * 0.85 * scale as f32,
                );
            }
        }

        let term_x = frame_x + frame_pad;
        let term_y = frame_y + frame_pad + title_bar;
        for y in 0..terminal.height() {
            for x in 0..terminal.width() {
                let px = terminal.get_pixel(x, y);
                img.put_pixel(term_x + x, term_y + y, *px);
            }
        }

        if chrome.timestamp {
            let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
            let color = muted_text_color(theme);
            let right_x = frame_x + frame_w.saturating_sub(frame_pad.max(6 * scale));
            let center_y = term_y + terminal.height() + bottom_pad / 2;
            self.draw_text_right_aligned(
                &mut img,
                right_x,
                center_y,
                &timestamp,
                color,
                self.font_size * 0.65 * scale as f32,
            );
        }

        img
    }

    /// Draw the window's drop shadow.
    ///
    /// The shadow is a single pass over the ring of pixels *outside* the frame:
    /// pixels the opaque frame will cover are skipped with a cheap bounding-box
    /// test, and the remaining ones get one signed-distance evaluation against
    /// the offset rounded rectangle with a smooth falloff. The previous
    /// implementation composited twelve full-frame rounded rectangles, ~99% of
    /// which were then painted over - that cost about 97% of total render time
    /// (7.2 s vs. 0.24 s for a 200-row capture).
    ///
    /// `frame_*` is the window frame rectangle, `offset_*` how far the shadow
    /// is displaced from it, and `blur` the falloff distance in pixels.
    #[allow(clippy::too_many_arguments)]
    fn draw_shadow(
        &self,
        img: &mut RgbaImage,
        frame_x: u32,
        frame_y: u32,
        frame_w: u32,
        frame_h: u32,
        radius: u32,
        offset_x: u32,
        offset_y: u32,
        blur: u32,
    ) {
        if frame_w == 0 || frame_h == 0 || blur == 0 {
            return;
        }
        /// Peak shadow opacity directly under the frame edge.
        const PEAK_ALPHA: f32 = 110.0;

        let (img_w, img_h) = img.dimensions();
        let shadow_x = (frame_x + offset_x) as f32;
        let shadow_y = (frame_y + offset_y) as f32;
        let (sw, sh) = (frame_w as f32, frame_h as f32);
        let r = (radius as f32).min(sw / 2.0).min(sh / 2.0);
        // Rounded-rect signed distance: negative inside, positive outside.
        let sdf = |px: f32, py: f32| -> f32 {
            let cx = shadow_x + sw / 2.0;
            let cy = shadow_y + sh / 2.0;
            let qx = (px - cx).abs() - (sw / 2.0 - r);
            let qy = (py - cy).abs() - (sh / 2.0 - r);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
            outside + qx.max(qy).min(0.0) - r
        };

        // Pixels well inside the frame are fully covered by the opaque window
        // and never need shadow work; this keeps the pass proportional to the
        // frame's perimeter rather than its area.
        let skip_x0 = frame_x + radius;
        let skip_x1 = frame_x + frame_w.saturating_sub(radius);
        let skip_y0 = frame_y + radius;
        let skip_y1 = frame_y + frame_h.saturating_sub(radius);

        let y0 = frame_y.saturating_sub(blur);
        let y1 = (frame_y + frame_h + offset_y + blur).min(img_h);
        let x0 = frame_x.saturating_sub(blur);
        let x1 = (frame_x + frame_w + offset_x + blur).min(img_w);
        let blur_f = blur as f32;

        for py in y0..y1 {
            let inside_y = py >= skip_y0 && py < skip_y1;
            for px in x0..x1 {
                if inside_y && px >= skip_x0 && px < skip_x1 {
                    continue;
                }
                let d = sdf(px as f32 + 0.5, py as f32 + 0.5);
                if d >= blur_f {
                    continue;
                }
                // Smoothstep falloff from the shadow edge outwards.
                let t = (1.0 - (d.max(0.0) / blur_f)).clamp(0.0, 1.0);
                let alpha = (PEAK_ALPHA * t * t * (3.0 - 2.0 * t)).round() as u8;
                if alpha == 0 {
                    continue;
                }
                self.put_pixel_blend(img, px, py, Rgba([0, 0, 0, alpha]));
            }
        }
    }

    /// Soften the corners of an already-rendered, fully opaque image by fading
    /// the alpha of pixels that fall outside a rounded-rectangle mask to zero,
    /// with a one-pixel anti-aliased edge. Used when `rounded` is enabled but
    /// no chrome frame is drawn, so a bare screenshot has soft corners on a
    /// transparent background. RGB is preserved; only alpha is scaled.
    fn round_image_corners(&self, img: &mut RgbaImage, radius: u32) {
        let (w, h) = img.dimensions();
        if radius == 0 || w == 0 || h == 0 {
            return;
        }
        // Clamp so the arcs never overlap on tiny images.
        let radius = radius.min(w / 2).min(h / 2);
        if radius == 0 {
            return;
        }
        let r = radius as f32;
        let (wf, hf) = (w as f32, h as f32);
        for py in 0..h {
            // Vertical distance into the top/bottom corner bands (0 elsewhere).
            let fy = py as f32 + 0.5;
            let dy = if fy < r {
                r - fy
            } else if fy > hf - r {
                fy - (hf - r)
            } else {
                0.0
            };
            for px in 0..w {
                let fx = px as f32 + 0.5;
                let dx = if fx < r {
                    r - fx
                } else if fx > wf - r {
                    fx - (wf - r)
                } else {
                    0.0
                };
                // Only the four corner squares need adjustment.
                if dx == 0.0 || dy == 0.0 {
                    continue;
                }
                let dist = (dx * dx + dy * dy).sqrt();
                // Coverage: 1 inside the arc, 0 outside, with a 1px soft edge.
                let coverage = (r + 0.5 - dist).clamp(0.0, 1.0);
                if coverage >= 1.0 {
                    continue;
                }
                let pixel = img.get_pixel_mut(px, py);
                pixel[3] = (pixel[3] as f32 * coverage).round() as u8;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_rounded_rect(
        &self,
        img: &mut RgbaImage,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        radius: u32,
        color: Rgba<u8>,
    ) {
        // Clamp the radius to what the rectangle can actually accommodate and
        // use that same value in every coordinate calculation, so oversized
        // radii never underflow the unsigned subtractions below.
        let radius = radius.min(w / 2).min(h / 2);
        let r = radius as i32;
        for py in y..y + h {
            for px in x..x + w {
                let dx = if px < x + radius {
                    (x + radius) as i32 - px as i32
                } else if px >= x + w - radius {
                    px as i32 - (x + w - radius - 1) as i32
                } else {
                    0
                };
                let dy = if py < y + radius {
                    (y + radius) as i32 - py as i32
                } else if py >= y + h - radius {
                    py as i32 - (y + h - radius - 1) as i32
                } else {
                    0
                };
                if (dx == 0 || dy == 0 || dx * dx + dy * dy <= r * r)
                    && px < img.width()
                    && py < img.height()
                {
                    self.put_pixel_blend(img, px, py, color);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_title_bar_accents(
        &self,
        img: &mut RgbaImage,
        chrome: &ChromeOptions,
        frame_x: u32,
        frame_y: u32,
        width: u32,
        title_bar_height: u32,
        theme: &Theme,
        scale: u32,
    ) {
        match chrome.preset.as_str() {
            "macos" => {
                let colors = [
                    Rgba([255, 95, 86, 255]),
                    Rgba([255, 189, 46, 255]),
                    Rgba([39, 201, 63, 255]),
                ];
                for (i, color) in colors.into_iter().enumerate() {
                    self.draw_circle(
                        img,
                        frame_x + (18 + (i as u32 * 16)) * scale,
                        frame_y + title_bar_height / 2,
                        5 * scale,
                        color,
                    );
                }
            }
            "gnome" => {
                // Draw a compact pill on the right to imply window controls.
                let pill_w = 44 * scale;
                let pill_h = 14 * scale;
                let pill_x = frame_x + width.saturating_sub(16 * scale + pill_w);
                let pill_y = frame_y + (title_bar_height / 2).saturating_sub(pill_h / 2);
                self.draw_rounded_rect(
                    img,
                    pill_x,
                    pill_y,
                    pill_w,
                    pill_h,
                    7 * scale,
                    Rgba([255, 255, 255, 24]),
                );
                for offset in [12u32, 22, 32] {
                    self.draw_circle(
                        img,
                        pill_x + offset * scale,
                        pill_y + pill_h / 2,
                        2 * scale,
                        Rgba([255, 255, 255, 90]),
                    );
                }
            }
            "report" => {
                self.draw_rect(
                    img,
                    frame_x,
                    frame_y + title_bar_height.saturating_sub(scale),
                    width,
                    scale,
                    Rgba([255, 255, 255, 20]),
                );
            }
            _ => {
                let accent = Rgba([
                    theme.foreground[0],
                    theme.foreground[1],
                    theme.foreground[2],
                    18,
                ]);
                self.draw_rect(
                    img,
                    frame_x,
                    frame_y + title_bar_height.saturating_sub(scale),
                    width,
                    scale,
                    accent,
                );
            }
        }
    }

    fn draw_circle(&self, img: &mut RgbaImage, cx: u32, cy: u32, r: u32, color: Rgba<u8>) {
        let r = r as i32;
        for y in (cy as i32 - r)..=(cy as i32 + r) {
            for x in (cx as i32 - r)..=(cx as i32 + r) {
                let dx = x - cx as i32;
                let dy = y - cy as i32;
                if dx * dx + dy * dy <= r * r && x >= 0 && y >= 0 {
                    let x = x as u32;
                    let y = y as u32;
                    if x < img.width() && y < img.height() {
                        self.put_pixel_blend(img, x, y, color);
                    }
                }
            }
        }
    }

    fn draw_text_line(
        &self,
        img: &mut RgbaImage,
        center_x: u32,
        center_y: u32,
        text: &str,
        color: Rgba<u8>,
        size: f32,
    ) {
        let total_width = self.text_width(text, size);
        self.draw_text_at(
            img,
            center_x as i32 - (total_width as i32 / 2),
            center_y,
            text,
            color,
            size,
        );
    }

    fn draw_text_right_aligned(
        &self,
        img: &mut RgbaImage,
        right_x: u32,
        center_y: u32,
        text: &str,
        color: Rgba<u8>,
        size: f32,
    ) {
        let total_width = self.text_width(text, size);
        self.draw_text_at(
            img,
            right_x as i32 - total_width as i32,
            center_y,
            text,
            color,
            size,
        );
    }

    fn text_width(&self, text: &str, size: f32) -> u32 {
        // The chrome font is monospace, so every character occupies one fixed
        // cell whose width is derived from 'M'. Using a single cell advance for
        // every glyph keeps `text_width` in sync with `draw_text_at` and avoids
        // per-glyph rounding drift.
        let cell_advance = self.font.metrics('M', size).advance_width.round() as u32;
        text.chars().count() as u32 * cell_advance
    }

    /// Draw a run of chrome text (title, timestamp) starting at `start_x`,
    /// vertically centered on `center_y`.
    ///
    /// The chrome font is monospace, so every character advances by the exact
    /// same integer cell width; per-glyph advances would produce visible
    /// spacing wobble. Glyph placement inside each cell is delegated to
    /// [`draw_glyph`](Self::draw_glyph), the same routine that renders terminal
    /// content, so both paths share baseline and side-bearing handling.
    fn draw_text_at(
        &self,
        img: &mut RgbaImage,
        start_x: i32,
        center_y: u32,
        text: &str,
        color: Rgba<u8>,
        size: f32,
    ) {
        let cell_advance = self.font.metrics('M', size).advance_width.round() as i32;
        let line_height = font_line_height(&self.font, size);
        let line_top = center_y as i32 - (line_height as i32 / 2);
        let mut cursor_x = start_x;
        for ch in text.chars() {
            self.draw_glyph(img, cursor_x, line_top, ch, color, false, size, &self.font);
            cursor_x += cell_advance;
        }
    }

    fn find_content_rows(&self, screen: &Screen, rows: u32, cols: u32) -> u32 {
        let mut last_row_with_content = 0u32;
        for row in 0..rows {
            for col in 0..cols {
                if let Some(cell) = screen.cell(row as u16, col as u16) {
                    let contents = cell.contents();
                    // A row counts as content if it has visible text or any
                    // styled cell (e.g. a colored background block), so
                    // background-only rows are not cropped away.
                    let has_text = !contents.is_empty() && contents != " ";
                    let styled = cell.bgcolor() != vt100::Color::Default || cell.inverse();
                    if has_text || styled {
                        last_row_with_content = row + 1;
                        break;
                    }
                }
            }
        }
        (last_row_with_content + 1).min(rows)
    }

    /// Scan the screen buffer to find the rightmost column that holds content
    /// (visible text or a styled/inverse cell) and return the width, in cells,
    /// to render. A small right padding is added and a minimum width enforced.
    ///
    /// If the content already fills more than 90% of the terminal width the
    /// full `cols` width is kept, so nearly-full output isn't shaved into an
    /// awkward micro-crop.
    fn find_content_cols(&self, screen: &Screen, rows: u32, cols: u32) -> u32 {
        let mut max_col = 0u32;
        for row in 0..rows {
            for col in (0..cols).rev() {
                if let Some(cell) = screen.cell(row as u16, col as u16) {
                    let contents = cell.contents();
                    let has_text = !contents.is_empty() && contents != " ";
                    let styled = cell.bgcolor() != vt100::Color::Default || cell.inverse();
                    if has_text || styled {
                        max_col = max_col.max(col + 1);
                        break;
                    }
                }
            }
        }

        // Only auto-crop when content is meaningfully narrower than the
        // terminal; if it fills >90% of the width, keep the full width.
        if max_col as f32 > cols as f32 * 0.9 {
            return cols;
        }

        // Add a small right padding (2 cells) and ensure a minimum width.
        (max_col + 2).min(cols).max(20).min(cols)
    }

    fn resolve_cell_colors(&self, cell: &vt100::Cell, theme: &Theme) -> (Rgba<u8>, Rgba<u8>) {
        let mut fg = self.resolve_color(cell.fgcolor(), true, theme);
        let mut bg = self.resolve_color(cell.bgcolor(), false, theme);

        if cell.inverse() {
            std::mem::swap(&mut fg, &mut bg);
        }

        if cell.bold() {
            // Bold + standard color (0-7) should use the bright variant (8-15),
            // matching how GNOME Terminal and most emulators render bold.
            fg = match cell.fgcolor() {
                vt100::Color::Idx(idx) if idx < 8 => theme.palette[(idx + 8) as usize],
                vt100::Color::Default => theme.palette[15], // bold default = bright white
                _ => {
                    // For 256/RGB colors, lighten toward white
                    Rgba([
                        fg[0].saturating_add((255 - fg[0]) / 4),
                        fg[1].saturating_add((255 - fg[1]) / 4),
                        fg[2].saturating_add((255 - fg[2]) / 4),
                        fg[3],
                    ])
                }
            };
        }

        if cell.dim() {
            fg = Rgba([fg[0] / 2, fg[1] / 2, fg[2] / 2, fg[3]]);
        }

        (fg, bg)
    }

    fn resolve_color(&self, color: vt100::Color, is_foreground: bool, theme: &Theme) -> Rgba<u8> {
        match color {
            vt100::Color::Default => {
                if is_foreground {
                    theme.foreground
                } else {
                    theme.background
                }
            }
            vt100::Color::Idx(idx) => {
                if idx < 16 {
                    theme.palette[idx as usize]
                } else if idx < 232 {
                    // 216-color cube
                    let idx = idx - 16;
                    let r = (idx / 36) % 6;
                    let g = (idx / 6) % 6;
                    let b = idx % 6;
                    Rgba([
                        if r == 0 { 0 } else { 55 + 40 * r },
                        if g == 0 { 0 } else { 55 + 40 * g },
                        if b == 0 { 0 } else { 55 + 40 * b },
                        255,
                    ])
                } else {
                    // Grayscale ramp
                    let level = 8 + 10 * (idx - 232);
                    Rgba([level, level, level, 255])
                }
            }
            vt100::Color::Rgb(r, g, b) => Rgba([r, g, b, 255]),
        }
    }

    fn draw_rect(&self, img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
        for dy in 0..h {
            for dx in 0..w {
                let px = x + dx;
                let py = y + dy;
                if px < img.width() && py < img.height() {
                    self.put_pixel_blend(img, px, py, color);
                }
            }
        }
    }

    fn put_pixel_blend(&self, img: &mut RgbaImage, x: u32, y: u32, color: Rgba<u8>) {
        if color[3] == 255 {
            img.put_pixel(x, y, color);
            return;
        }
        if color[3] == 0 {
            return;
        }

        // Proper "source-over" alpha compositing that also computes the
        // resulting alpha channel. The previous implementation forced the
        // output alpha to 255, which turned semi-transparent pixels (such as
        // the drop shadow) into fully opaque ones. Because the shadow is
        // offset down-and-right, that left the top-left rounded corner clean
        // while the other three corners picked up opaque shadow artifacts.
        let bg = img.get_pixel(x, y);
        let src_a = color[3] as f32 / 255.0;
        let dst_a = bg[3] as f32 / 255.0;
        let out_a = src_a + dst_a * (1.0 - src_a);
        if out_a <= 0.0 {
            img.put_pixel(x, y, Rgba([0, 0, 0, 0]));
            return;
        }
        let blend = |sc: u8, dc: u8| -> u8 {
            let v = (sc as f32 * src_a + dc as f32 * dst_a * (1.0 - src_a)) / out_a;
            v.round().clamp(0.0, 255.0) as u8
        };
        let out = Rgba([
            blend(color[0], bg[0]),
            blend(color[1], bg[1]),
            blend(color[2], bg[2]),
            (out_a * 255.0).round().clamp(0.0, 255.0) as u8,
        ]);
        img.put_pixel(x, y, out);
    }

    /// Rasterize and blend a single glyph into `img`.
    ///
    /// This is the one glyph renderer in the codebase: both the terminal grid
    /// (via [`draw_char_with_font`](Self::draw_char_with_font)) and the chrome
    /// title/timestamp text (via [`draw_text_at`](Self::draw_text_at)) go
    /// through it, so they share identical positioning rules:
    ///
    /// * `cell_x` is the left edge of the character cell; the glyph is placed
    ///   at `cell_x + xmin` so its side bearing is respected.
    /// * `line_top` is the top of the line box; the glyph sits on the shared
    ///   baseline at `line_top + ascent`, offset by its own `ymin`. Using the
    ///   baseline (instead of centering each bitmap) is what keeps descenders
    ///   hanging and stops the per-glyph vertical wobble.
    /// * `italic` applies a synthetic shear, used by terminal content.
    /// * `font` may be the regular or bold face; `color`'s alpha modulates the
    ///   glyph coverage and the result is composited source-over.
    #[allow(clippy::too_many_arguments)]
    fn draw_glyph(
        &self,
        img: &mut RgbaImage,
        cell_x: i32,
        line_top: i32,
        ch: char,
        color: Rgba<u8>,
        italic: bool,
        font_size: f32,
        font: &Font,
    ) {
        let (metrics, bitmap) = font.rasterize(ch, font_size);
        if bitmap.is_empty() || metrics.width == 0 || metrics.height == 0 {
            return;
        }

        let ascent = font_ascent(font, font_size);
        let glyph_x = cell_x + metrics.xmin;
        let glyph_y = line_top + ascent.round() as i32 - metrics.height as i32 - metrics.ymin;

        let shear = |gy: usize| -> i32 {
            if !italic {
                return 0;
            }
            let from_bottom = metrics.height as i32 - gy as i32;
            (from_bottom as f32 * 0.22) as i32
        };

        for gy in 0..metrics.height {
            let dx_italic = shear(gy);
            for gx in 0..metrics.width {
                let coverage = bitmap[gy * metrics.width + gx];
                if coverage == 0 {
                    continue;
                }
                let px = glyph_x + gx as i32 + dx_italic;
                let py = glyph_y + gy as i32;
                if px < 0 || py < 0 {
                    continue;
                }
                let (px, py) = (px as u32, py as u32);
                if px >= img.width() || py >= img.height() {
                    continue;
                }
                let alpha = ((coverage as u32 * color[3] as u32) / 255) as u8;
                self.put_pixel_blend(img, px, py, Rgba([color[0], color[1], color[2], alpha]));
            }
        }
    }

    /// Draw a single terminal cell's character. `x`/`y` are the top-left
    /// corner of the cell in image space.
    #[allow(clippy::too_many_arguments)]
    fn draw_char_with_font(
        &self,
        img: &mut RgbaImage,
        x: u32,
        y: u32,
        ch: char,
        color: Rgba<u8>,
        italic: bool,
        font_size: f32,
        font: &Font,
    ) {
        self.draw_glyph(img, x as i32, y as i32, ch, color, italic, font_size, font);
    }
}

/// Maximum length (in characters) of a generated screenshot base filename.
const MAX_FILENAME_CHARS: usize = 60;

/// Sanitize an arbitrary string (typically a command line) into a safe,
/// descriptive PNG base filename: lowercased, with every run of
/// non-alphanumeric characters collapsed to a single hyphen, trimmed of
/// leading/trailing hyphens, and capped at [`MAX_FILENAME_CHARS`] characters.
/// Empty or all-symbol input yields `"screenshot"`.
fn sanitize_base_name(input: &str) -> String {
    let mut out = String::new();
    let mut pending_hyphen = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_hyphen && !out.is_empty() {
                out.push('-');
            }
            pending_hyphen = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_hyphen = true;
        }
    }
    let mut out: String = out.chars().take(MAX_FILENAME_CHARS).collect();
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "screenshot".to_string()
    } else {
        out
    }
}

/// Build a unique `<base>.png` path in `output_dir`, appending `-2`, `-3`, ...
/// before the extension until an unused name is found.
fn unique_png_path(output_dir: &Path, base: &str) -> PathBuf {
    let first = output_dir.join(format!("{}.png", base));
    if !first.exists() {
        return first;
    }
    let mut n = 2u32;
    loop {
        let candidate = output_dir.join(format!("{}-{}.png", base, n));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Derive a fallback screenshot base name from the process working directory
/// and a command line, as `{cwd_basename} {first_word}` (later sanitized to
/// e.g. `nrs-cargo`). This is only a fallback for when no explicit name is
/// given: callers and agents should prefer a descriptive `output_name`
/// (e.g. `finding-01-sqli`) whenever possible.
pub fn fallback_output_name(cwd: Option<&Path>, command: &str) -> String {
    let dir = cwd
        .and_then(Path::file_name)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let first = command.split_whitespace().next().unwrap_or("");
    match (dir.trim().is_empty(), first.is_empty()) {
        (false, false) => format!("{} {}", dir, first),
        (true, false) => first.to_string(),
        (false, true) => dir,
        (true, true) => String::new(),
    }
}

fn truncate_title(title: &str) -> String {
    if title.chars().count() <= MAX_TITLE_CHARS {
        return title.to_string();
    }
    format!(
        "{}...",
        title.chars().take(MAX_TITLE_CHARS).collect::<String>()
    )
}

fn is_light(color: Rgba<u8>) -> bool {
    let luminance = 0.2126 * color[0] as f32 + 0.7152 * color[1] as f32 + 0.0722 * color[2] as f32;
    luminance > 150.0
}

fn muted_text_color(theme: &Theme) -> Rgba<u8> {
    if is_light(theme.background) {
        Rgba([0, 0, 0, 128])
    } else {
        Rgba([255, 255, 255, 128])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_options_from_config_respects_enabled_and_preset() {
        let config = ChromeConfig {
            enabled: true,
            preset: "gnome".to_string(),
            title: Some("demo".to_string()),
            timestamp: false,
            shadow: true,
            radius: 12,
            rounded: true,
            outer_padding: 20,
            title_bar_height: 30,
        };

        let options = ChromeOptions::from_config(&config);
        assert!(options.enabled);
        assert_eq!(options.preset, "gnome");
        assert_eq!(options.title.as_deref(), Some("demo"));
        assert!(!options.timestamp);
    }

    #[test]
    fn compose_with_chrome_increases_canvas_size() {
        let renderer = Renderer {
            font: Font::from_bytes(
                std::fs::read("fonts/JetBrainsMono-Regular.ttf").expect("font present"),
                FontSettings::default(),
            )
            .expect("font parse"),
            font_bold: None,
            font_size: 16.0,
            cell_width: 8,
            cell_height: 16,
            themes: HashMap::new(),
            default_theme: "dark".to_string(),
            default_chrome: ChromeOptions {
                enabled: false,
                preset: "none".to_string(),
                title: None,
                timestamp: false,
                shadow: true,
                radius: 14,
                rounded: true,
                outer_padding: 18,
                title_bar_height: 34,
            },
            padding: 16,
        };

        let terminal = ImageBuffer::from_pixel(100, 50, Rgba([10, 10, 10, 255]));
        let theme = Theme::dark();
        let chrome = ChromeOptions {
            enabled: true,
            preset: "gnome".to_string(),
            title: Some("demo".to_string()),
            timestamp: false,
            shadow: true,
            radius: 14,
            rounded: true,
            outer_padding: 18,
            title_bar_height: 34,
        };

        let result = renderer.compose_with_chrome(terminal, &theme, &chrome);
        assert!(result.width() > 100);
        assert!(result.height() > 50);
    }

    #[test]
    fn timestamp_reserves_bottom_padding() {
        let renderer = Renderer {
            font: Font::from_bytes(
                std::fs::read("fonts/JetBrainsMono-Regular.ttf").expect("font present"),
                FontSettings::default(),
            )
            .expect("font parse"),
            font_bold: None,
            font_size: 16.0,
            cell_width: 8,
            cell_height: 16,
            themes: HashMap::new(),
            default_theme: "dark".to_string(),
            default_chrome: ChromeOptions {
                enabled: false,
                preset: "none".to_string(),
                title: None,
                timestamp: false,
                shadow: false,
                radius: 14,
                rounded: true,
                outer_padding: 0,
                title_bar_height: 34,
            },
            padding: 16,
        };
        let terminal = ImageBuffer::from_pixel(100, 50, Rgba([10, 10, 10, 255]));
        let chrome = ChromeOptions {
            enabled: true,
            preset: "report".to_string(),
            title: None,
            timestamp: true,
            shadow: false,
            radius: 14,
            rounded: true,
            outer_padding: 0,
            title_bar_height: 34,
        };

        let result = renderer.compose_with_chrome(terminal, &Theme::dark(), &chrome);
        // Chrome metrics are drawn at RENDER_SCALE (2x): title bar 34*2=68 and
        // the timestamp reserves max(0, 18*2)=36 of bottom padding.
        assert_eq!(result.height(), 50 + 68 + 36);
    }

    #[test]
    fn long_titles_are_truncated_on_character_boundaries() {
        let title = "é".repeat(MAX_TITLE_CHARS + 1);
        let truncated = truncate_title(&title);
        assert_eq!(truncated.chars().count(), MAX_TITLE_CHARS + 3);
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn round_image_corners_makes_corners_transparent_but_keeps_body() {
        let renderer = bare_renderer();
        let mut img = ImageBuffer::from_pixel(40, 40, Rgba([20, 20, 20, 255]));
        renderer.round_image_corners(&mut img, 10);
        // The extreme corner sits outside the arc and becomes transparent.
        assert_eq!(img.get_pixel(0, 0)[3], 0);
        assert_eq!(img.get_pixel(39, 39)[3], 0);
        // Edge midpoints and the center are not in a corner: fully opaque.
        assert_eq!(img.get_pixel(20, 0)[3], 255);
        assert_eq!(img.get_pixel(0, 20)[3], 255);
        assert_eq!(img.get_pixel(20, 20)[3], 255);
        // RGB is preserved even where alpha is faded.
        assert_eq!([img.get_pixel(0, 0)[0], img.get_pixel(0, 0)[1]], [20, 20]);
    }

    #[test]
    fn round_image_corners_is_noop_for_zero_radius() {
        let renderer = bare_renderer();
        let mut img = ImageBuffer::from_pixel(20, 20, Rgba([10, 10, 10, 255]));
        renderer.round_image_corners(&mut img, 0);
        assert_eq!(img.get_pixel(0, 0)[3], 255);
    }

    #[test]
    fn bare_render_rounds_corners_when_enabled_and_squares_when_disabled() {
        let mut renderer = bare_renderer();
        // Give the (chrome-less) default a real corner radius to round against.
        renderer.default_chrome.radius = 8;
        let theme = Theme::dark();
        let mut parser = vt100::Parser::new(3, 20, 0);
        parser.process(b"hello world");
        let screen = parser.screen();

        renderer.default_chrome.rounded = true;
        let rounded = renderer
            .render_to_image(screen, &theme, &renderer.default_chrome, None, false)
            .unwrap();
        assert_eq!(rounded.get_pixel(0, 0)[3], 0, "rounded: corner transparent");

        renderer.default_chrome.rounded = false;
        let square = renderer
            .render_to_image(screen, &theme, &renderer.default_chrome, None, false)
            .unwrap();
        assert_eq!(square.get_pixel(0, 0)[3], 255, "square: corner opaque");
    }

    #[test]
    fn chrome_options_default_rounded_is_true() {
        let options = ChromeOptions::from_config(&ChromeConfig::default());
        assert!(options.rounded);
    }

    #[test]
    fn title_uses_fixed_monospace_cell_advance() {
        // The wobble fix hinges on every glyph advancing by the same fixed
        // cell width, so `text_width` must equal char_count * cell_advance and
        // be identical for narrow vs. wide characters of equal length.
        let r = bare_renderer();
        let size = 20.0;
        let one_m = r.text_width("M", size);
        assert!(one_m > 0);
        assert_eq!(r.text_width("MMMMMMMMMM", size), one_m * 10);
        assert_eq!(r.text_width("iiiiiiiiii", size), one_m * 10);
        assert_eq!(r.text_width("Mi.lWx/9-", size), one_m * 9);
    }

    /// Chrome text (window title *and* timestamp watermark, which share
    /// [`Renderer::draw_text_at`]) must sit on one baseline: flat and round
    /// glyphs align at the bottom while a descender hangs below it. Centering
    /// each glyph bitmap instead - the old behavior - made descenders sit *on*
    /// the baseline and gave titles a visible wobble.
    #[test]
    fn chrome_text_sits_on_a_shared_baseline() {
        let r = bare_renderer();
        let size = 24.0;
        let mut img: RgbaImage = ImageBuffer::from_pixel(400, 80, Rgba([0, 0, 0, 255]));
        r.draw_text_at(&mut img, 10, 40, "oxy", Rgba([255, 255, 255, 255]), size);

        // Bottom-most inked row per glyph cell (cells are a fixed advance wide).
        let advance = r.text_width("M", size) as i32;
        let bottom_of = |cell: i32| -> u32 {
            let (x0, x1) = (10 + cell * advance, 10 + (cell + 1) * advance);
            (0..img.height())
                .rfind(|&y| {
                    (x0.max(0) as u32..(x1.max(0) as u32).min(img.width()))
                        .any(|x| img.get_pixel(x, y)[0] > 40)
                })
                .expect("glyph should have been drawn")
        };

        let (o, x, y) = (bottom_of(0), bottom_of(1), bottom_of(2));
        // 'o' and 'x' share the baseline (round glyphs may overshoot by 1px).
        assert!(
            o.abs_diff(x) <= 1,
            "flat and round glyphs must share a baseline: o={o}, x={x}"
        );
        // 'y' descends below it.
        assert!(
            y > o + 1,
            "descender must hang below the baseline: y={y}, o={o}"
        );
    }

    /// The timestamp watermark is drawn right-aligned through the same routine,
    /// so it must end at the requested edge and share that one baseline.
    #[test]
    fn timestamp_is_right_aligned_on_the_same_baseline() {
        let r = bare_renderer();
        let size = 20.0;
        let text = "2026-08-26 01:38:03 UTC";
        let right_x = 380u32;
        let mut img: RgbaImage = ImageBuffer::from_pixel(400, 60, Rgba([0, 0, 0, 255]));
        r.draw_text_right_aligned(
            &mut img,
            right_x,
            30,
            text,
            Rgba([255, 255, 255, 255]),
            size,
        );

        let advance = r.text_width("M", size);
        let start_x = right_x - r.text_width(text, size);

        // Per character cell, the top and bottom inked rows.
        let mut extents: Vec<(usize, u32, u32)> = Vec::new();
        for (cell, _) in text.chars().enumerate().filter(|(_, c)| *c != ' ') {
            let x0 = start_x + cell as u32 * advance;
            let x1 = (x0 + advance).min(img.width());
            let rows: Vec<u32> = (0..img.height())
                .filter(|&y| (x0..x1).any(|x| img.get_pixel(x, y)[0] > 40))
                .collect();
            assert!(!rows.is_empty(), "cell {cell} was not drawn");
            extents.push((cell, rows[0], *rows.last().unwrap()));
        }

        // Digits and letters (the tall glyphs) must share one baseline; round
        // shapes may overshoot it by a single pixel.
        let tallest = extents.iter().map(|(_, t, b)| b - t).max().unwrap();
        let full: Vec<u32> = extents
            .iter()
            .filter(|(_, t, b)| b - t >= tallest - 1)
            .map(|(_, _, b)| *b)
            .collect();
        let (lo, hi) = (*full.iter().min().unwrap(), *full.iter().max().unwrap());
        assert!(
            hi - lo <= 1,
            "timestamp glyphs drifted off the baseline: {lo} vs {hi}"
        );

        // Hyphens and colons sit above the baseline, proving glyphs are placed
        // by their own metrics rather than centered in the line box.
        let hyphen = extents
            .iter()
            .find(|(cell, _, _)| text.chars().nth(*cell) == Some('-'))
            .expect("hyphen cell");
        assert!(
            hyphen.2 < lo,
            "a hyphen must not reach the baseline: {} vs {lo}",
            hyphen.2
        );

        // The run ends at the requested right edge and starts a text width back.
        let inked: Vec<u32> = (0..img.width())
            .filter(|&x| (0..img.height()).any(|y| img.get_pixel(x, y)[0] > 40))
            .collect();
        assert!(*inked.last().unwrap() < right_x);
        assert!(right_x - *inked.last().unwrap() <= advance);
        assert!(inked[0] >= start_x);
    }

    #[test]
    fn sample_background_prefers_opaque_pane_color() {
        let mut img = ImageBuffer::from_pixel(20, 20, Rgba([12, 34, 56, 255]));
        // A transparent (rounded) corner must not be chosen.
        img.put_pixel(0, 0, Rgba([0, 0, 0, 0]));
        assert_eq!(
            sample_background(&img, Rgba([9, 9, 9, 255])),
            Rgba([12, 34, 56, 255])
        );
        // A fully transparent image falls back to the provided default.
        let clear = ImageBuffer::from_pixel(20, 20, Rgba([0, 0, 0, 0]));
        assert_eq!(
            sample_background(&clear, Rgba([9, 9, 9, 255])),
            Rgba([9, 9, 9, 255])
        );
    }

    #[test]
    fn normalize_newlines_converts_bare_lf_and_is_idempotent() {
        use std::borrow::Cow;
        // Bare LF (piped / redirected output) is upgraded to CRLF.
        assert_eq!(
            normalize_newlines(b"a\nb\n").as_ref(),
            b"a\r\nb\r\n".as_slice()
        );
        // Existing CRLF (PTY output from `exec`) is untouched and not cloned.
        assert!(matches!(
            normalize_newlines(b"a\r\nb\r\n"),
            Cow::Borrowed(_)
        ));
        // Data with no newline at all is borrowed unchanged.
        assert!(matches!(
            normalize_newlines(b"no newline"),
            Cow::Borrowed(_)
        ));
        // A leading LF is handled without underflow.
        assert_eq!(normalize_newlines(b"\nx").as_ref(), b"\r\nx".as_slice());
    }

    fn bare_renderer() -> Renderer {
        Renderer {
            font: Font::from_bytes(
                std::fs::read("fonts/JetBrainsMono-Regular.ttf").expect("font present"),
                FontSettings::default(),
            )
            .expect("font parse"),
            font_bold: None,
            font_size: 16.0,
            cell_width: 8,
            cell_height: 16,
            themes: HashMap::new(),
            default_theme: "dark".to_string(),
            default_chrome: ChromeOptions {
                enabled: false,
                preset: "none".to_string(),
                title: None,
                timestamp: false,
                shadow: false,
                radius: 0,
                rounded: true,
                outer_padding: 0,
                title_bar_height: 0,
            },
            padding: 16,
        }
    }

    #[test]
    fn redaction_masks_sensitive_cells_in_rendered_image() {
        use crate::redaction::{RedactionConfig, RedactionEngine};

        let renderer = bare_renderer();
        let theme = Theme::dark();
        let engine = RedactionEngine::from_config(&RedactionConfig::default()).unwrap();

        let mut parser = vt100::Parser::new(3, 40, 0);
        parser.process(b"ip 10.20.30.40 up");
        let screen = parser.screen();

        let map = engine.redact_screen(screen, None);
        assert!(!map.is_empty(), "expected the IPv4 address to be redacted");

        let redacted = renderer
            .render_screen(screen, &theme, Some(&map), true)
            .unwrap();
        let plain = renderer.render_screen(screen, &theme, None, true).unwrap();

        // "[IP]" labels columns 3-6; sample a later plain-block column so we
        // land on solid redaction red rather than a label glyph.
        let scale = 2u32;
        let cell_w = renderer.cell_width * scale;
        let cell_h = renderer.cell_height * scale;
        let padding = renderer.padding * scale;
        let px = 10 * cell_w + padding + cell_w / 2;
        let py = padding + cell_h / 2;

        assert_eq!(
            *redacted.get_pixel(px, py),
            REDACT_BG,
            "redacted cell should be painted red"
        );
        assert_ne!(
            *plain.get_pixel(px, py),
            REDACT_BG,
            "un-redacted render should not contain redaction red"
        );
    }

    #[test]
    fn render_bytes_scrubs_plain_text_and_reports_audit() {
        use crate::redaction::{RedactionConfig, RedactionEngine};

        let mut renderer = bare_renderer();
        renderer.themes.insert("dark".to_string(), Theme::dark());

        let engine = RedactionEngine::from_config(&RedactionConfig::default()).unwrap();
        let request = RedactionRequest {
            engine: &engine,
            rules: None,
        };

        let out_dir = std::path::Path::new("target/redaction-test-out");
        std::fs::create_dir_all(out_dir).unwrap();

        let secret = b"host 203.0.113.55 admin@example.com\n";
        let (path, plain_text, audit, _meta) = renderer
            .render_bytes(
                secret,
                40,
                3,
                out_dir,
                Some("redaction test"),
                Some("dark"),
                None,
                Some(&request),
                TextOptions {
                    strip_ansi: false,
                    redact_text: true,
                    embed_description: true,
                },
                true,
            )
            .unwrap();

        // With redact_text = true the returned text must not leak values.
        assert!(!plain_text.contains("203.0.113.55"));
        assert!(!plain_text.contains("admin@example.com"));
        assert!(plain_text.contains('\u{2588}'));

        // The rendered PNG must not embed the sensitive characters as glyphs;
        // verify at least by confirming redaction red is present in the file.
        let img = image::open(&path).unwrap().to_rgba8();
        let has_red = img.pixels().any(|p| *p == REDACT_BG);
        assert!(has_red, "rendered PNG should contain redaction blocks");

        // Audit reports counts but never values.
        assert!(audit.iter().any(|(name, _)| name == "ipv4"));
        assert!(audit.iter().any(|(name, _)| name == "email"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn render_bytes_default_keeps_original_text_but_redacts_image() {
        use crate::redaction::{RedactionConfig, RedactionEngine};

        let mut renderer = bare_renderer();
        renderer.themes.insert("dark".to_string(), Theme::dark());

        let engine = RedactionEngine::from_config(&RedactionConfig::default()).unwrap();
        let request = RedactionRequest {
            engine: &engine,
            rules: None,
        };

        let out_dir = std::path::Path::new("target/redaction-default-out");
        std::fs::create_dir_all(out_dir).unwrap();

        // Default TextOptions: image is redacted, but the returned text keeps
        // the original (unredacted) values so an agent can inspect them.
        let secret = b"host 203.0.113.55 here\n";
        let (path, plain_text, audit, _meta) = renderer
            .render_bytes(
                secret,
                40,
                3,
                out_dir,
                Some("default redaction test"),
                Some("dark"),
                None,
                Some(&request),
                TextOptions::default(),
                true,
            )
            .unwrap();

        assert!(
            plain_text.contains("203.0.113.55"),
            "default text should keep original value: {}",
            plain_text
        );
        assert!(!plain_text.contains('\u{2588}'));
        assert!(audit.iter().any(|(name, _)| name == "ipv4"));

        // The PNG is still redacted.
        let img = image::open(&path).unwrap().to_rgba8();
        assert!(img.pixels().any(|p| *p == REDACT_BG));

        std::fs::remove_file(&path).ok();
    }

    /// Read the PNG `tEXt` chunk stored under `Description`, if any.
    fn png_description(path: &std::path::Path) -> Option<String> {
        let file = std::io::BufReader::new(std::fs::File::open(path).unwrap());
        let decoder = png::Decoder::new(file);
        let reader = decoder.read_info().unwrap();
        reader
            .info()
            .uncompressed_latin1_text
            .iter()
            .find(|chunk| chunk.keyword == "Description")
            .map(|chunk| chunk.text.clone())
    }

    #[test]
    fn png_embeds_terminal_text_as_description() {
        let mut renderer = bare_renderer();
        renderer.themes.insert("dark".to_string(), Theme::dark());
        let out_dir = std::path::Path::new("target/description-test-out");
        std::fs::create_dir_all(out_dir).unwrap();

        let (path, _text, _audit, _meta) = renderer
            .render_bytes(
                b"hello accessible world\n",
                40,
                3,
                out_dir,
                Some("description on"),
                Some("dark"),
                None,
                None,
                TextOptions {
                    embed_description: true,
                    ..TextOptions::default()
                },
                true,
            )
            .unwrap();
        let description = png_description(&path).expect("Description chunk missing");
        assert!(
            description.contains("hello accessible world"),
            "unexpected description: {:?}",
            description
        );
        std::fs::remove_file(&path).ok();

        // Opt-out writes no metadata at all.
        let (path, _text, _audit, _meta) = renderer
            .render_bytes(
                b"hello accessible world\n",
                40,
                3,
                out_dir,
                Some("description off"),
                Some("dark"),
                None,
                None,
                TextOptions::default(),
                true,
            )
            .unwrap();
        assert_eq!(png_description(&path), None);
        std::fs::remove_file(&path).ok();
    }

    /// The embedded description must never carry a value the image redacts.
    #[test]
    fn embedded_description_is_redacted() {
        use crate::redaction::{RedactionConfig, RedactionEngine};

        let mut renderer = bare_renderer();
        renderer.themes.insert("dark".to_string(), Theme::dark());
        let engine = RedactionEngine::from_config(&RedactionConfig::default()).unwrap();
        let request = RedactionRequest {
            engine: &engine,
            rules: None,
        };
        let out_dir = std::path::Path::new("target/description-test-out");
        std::fs::create_dir_all(out_dir).unwrap();

        let (path, _text, _audit, _meta) = renderer
            .render_bytes(
                b"key AKIAIOSFODNN7EXAMPLE end\n",
                60,
                3,
                out_dir,
                Some("description redacted"),
                Some("dark"),
                None,
                Some(&request),
                TextOptions {
                    embed_description: true,
                    ..TextOptions::default()
                },
                true,
            )
            .unwrap();

        let description = png_description(&path).expect("Description chunk missing");
        assert!(
            !description.contains("AKIAIOSFODNN7EXAMPLE"),
            "description leaked a redacted secret: {:?}",
            description
        );
        assert!(description.contains("key "));
        // Redaction blocks are transliterated to '#' (tEXt is Latin-1 only).
        assert!(description.contains("####"), "got: {:?}", description);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parse_hex_color_rejects_non_ascii_input() {
        // Six *bytes*, but slicing them would split a UTF-8 character.
        assert!(parse_hex_color("#abc\u{20ac}").is_err());
        assert!(parse_hex_color("#zzzzzz").is_err());
        assert_eq!(parse_hex_color("#1e1e1e").unwrap(), Rgba([30, 30, 30, 255]));
    }

    #[test]
    fn sanitize_base_name_derives_readable_slugs() {
        assert_eq!(sanitize_base_name("ls -la"), "ls-la");
        assert_eq!(sanitize_base_name("whoami uname -a"), "whoami-uname-a");
        // Leading/trailing/duplicate separators collapse and trim.
        assert_eq!(sanitize_base_name("  git   status!! "), "git-status");
        // All-symbol / empty input falls back to a generic name.
        assert_eq!(sanitize_base_name("///"), "screenshot");
        assert_eq!(sanitize_base_name(""), "screenshot");
        // Length is capped and never ends on a hyphen.
        let long = "a ".repeat(80);
        let slug = sanitize_base_name(&long);
        assert!(slug.chars().count() <= 60);
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn fallback_output_name_combines_cwd_and_first_word() {
        use std::path::Path;
        assert_eq!(
            fallback_output_name(Some(Path::new("/home/adam/Desktop/nrs")), "cargo test"),
            "nrs cargo"
        );
        assert_eq!(
            fallback_output_name(Some(Path::new("/home/adam/Desktop/nrs/src")), "ls -la"),
            "src ls"
        );
        // Missing cwd or command degrades gracefully.
        assert_eq!(fallback_output_name(None, "git log --oneline"), "git");
        assert_eq!(
            fallback_output_name(Some(Path::new("/tmp/work")), ""),
            "work"
        );
        assert_eq!(fallback_output_name(None, ""), "");
    }

    #[test]
    fn unique_png_path_appends_numeric_suffixes() {
        let dir = std::path::Path::new("target/unique-name-test");
        std::fs::create_dir_all(dir).unwrap();
        // Clean any leftovers from a previous run.
        for name in ["cmd.png", "cmd-2.png", "cmd-3.png"] {
            std::fs::remove_file(dir.join(name)).ok();
        }

        let first = unique_png_path(dir, "cmd");
        assert_eq!(first, dir.join("cmd.png"));
        std::fs::write(&first, b"x").unwrap();

        let second = unique_png_path(dir, "cmd");
        assert_eq!(second, dir.join("cmd-2.png"));
        std::fs::write(&second, b"x").unwrap();

        let third = unique_png_path(dir, "cmd");
        assert_eq!(third, dir.join("cmd-3.png"));

        for name in ["cmd.png", "cmd-2.png", "cmd-3.png"] {
            std::fs::remove_file(dir.join(name)).ok();
        }
    }
}
