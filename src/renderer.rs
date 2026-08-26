use crate::capture::{
    CapturedScreen, DEFAULT_MAX_SCROLLBACK_LINES, LineSelection, cell_has_content,
    effective_scrollback_lines,
};
use crate::config::{ChromeConfig, ThemeConfig};
use crate::redaction::{RedactionEngine, RedactionMap};
use anyhow::{Context, Result};
use chrono::Utc;
use fontdue::{Font, FontSettings};
use image::{ImageBuffer, Rgba, RgbaImage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// Narrowest image, in terminal cells, that width trimming will produce, so an
/// empty capture still renders as a small padded tile instead of a zero-width
/// PNG. Any real content is trimmed to its exact bound.
const MIN_CONTENT_COLS: u32 = 1;

/// Largest image, in pixels, that will be rendered.
///
/// A capture of a command that printed tens of thousands of lines would
/// otherwise ask the allocator for gigabytes. 64 megapixels is one 256 MB RGBA
/// buffer, and at a standard 120-column terminal it is still several hundred
/// lines of output - far more than fits on any screen. Past it the error points
/// at `--head-lines` / `--tail-lines`.
const MAX_IMAGE_PIXELS: u64 = 64_000_000;

/// Peak bytes of image buffer a single render may hold at once.
///
/// The pixel ceiling alone does not bound memory: composing window chrome holds
/// the framed window and the shadowed canvas at the same time, so the peak is a
/// multiple of the final image. This budget is checked against *all* the
/// full-size buffers a render will hold simultaneously, so a shadowed window is
/// held to roughly half the bare ceiling instead of quietly costing twice as
/// much.
const MAX_RENDER_BYTES: u64 = 320 * 1024 * 1024;

/// Bytes per pixel in the RGBA buffers the renderer allocates.
const BYTES_PER_PIXEL: u64 = 4;

/// Metadata describing how a screenshot was rendered, so it can later be
/// re-rendered (e.g. by the `redact_screenshot` MCP tool) with identical
/// geometry and styling. The MCP server keeps this in memory (alongside the
/// raw terminal bytes) for the lifetime of the process; it is not written to
/// disk.
///
/// This is the shape published in 1.0.0 and it stays that way: it is a public
/// struct with public fields that external code builds with exhaustive
/// literals, so a new field here would be a source-breaking change. Anything
/// the renderer has learned to record since travels in [`RenderContext`].
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
    /// Whether the returned text was taken from the parsed screen rather than
    /// the source bytes (see [`TextOptions::from_screen`]). Kept so a later
    /// re-render returns text of the same kind as the original capture.
    #[serde(default)]
    pub from_screen: bool,
}

impl Default for RenderMeta {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            theme: None,
            chrome: None,
            auto_crop: default_auto_crop(),
            from_screen: false,
        }
    }
}

/// A [`RenderMeta`] plus everything the renderer learned about a capture after
/// 1.0.0 froze that struct's shape.
///
/// Returned by the option-taking APIs ([`Renderer::render_bytes_with_options`])
/// and kept by the MCP server's render cache, so a re-render reproduces not
/// only the original geometry and styling but the same lines of the same
/// capture - and so its cell coordinates mean the same thing.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RenderContext {
    /// The 1.0.0 metadata, unchanged.
    #[serde(flatten)]
    pub meta: RenderMeta,
    /// Which of the retained lines the screenshot showed.
    #[serde(default)]
    pub lines: LineSelection,
    /// Whether more output scrolled off than the scrollback could hold while
    /// capturing, so the oldest lines were dropped before rendering.
    #[serde(default)]
    pub truncated: bool,
}

impl RenderContext {
    /// A context around metadata with no line selection, which is what a 1.0.0
    /// [`RenderMeta`] describes: the whole capture, nothing dropped.
    pub fn from_meta(meta: RenderMeta) -> Self {
        Self {
            meta,
            lines: LineSelection::All,
            truncated: false,
        }
    }
}

impl std::ops::Deref for RenderContext {
    type Target = RenderMeta;

    fn deref(&self) -> &RenderMeta {
        &self.meta
    }
}

fn default_auto_crop() -> bool {
    true
}

/// Result of rendering: output PNG path, plain text, per-rule redaction
/// audit counts (empty when no redaction was applied), and the render metadata
/// needed to later re-render (e.g. for `redact_screenshot`).
pub type RenderOutput = (PathBuf, String, Vec<(String, usize)>, RenderMeta);

/// [`RenderOutput`] with the extended [`RenderContext`] in place of the 1.0.0
/// [`RenderMeta`], as the option-taking render APIs return it.
pub type RenderOutputWithContext = (PathBuf, String, Vec<(String, usize)>, RenderContext);

/// How [`compose_images`] arranges multiple source screenshots on the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeLayout {
    /// Place images left-to-right; canvas height is the tallest image, and
    /// shorter panes are padded (not stretched) to reach it.
    Horizontal,
    /// Stack images top-to-bottom; canvas width is the widest image, and
    /// narrower panes are padded (not stretched) to reach it.
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

/// macOS traffic-light geometry, in unscaled (1x) pixels: the radius of each
/// button, the center of the leftmost one measured from the frame's left edge,
/// and the distance between adjacent centers. All three buttons share the
/// radius, so they are always identical circles.
const TRAFFIC_LIGHT_RADIUS: u32 = 5;
const TRAFFIC_LIGHT_FIRST_CENTER: u32 = 18;
const TRAFFIC_LIGHT_PITCH: u32 = 16;

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
/// panes are padded to a common height (the tallest source) and separated by a
/// vertical divider; for a vertical layout they are padded to a common width
/// (the widest source) and separated by a horizontal divider, so the seams line
/// up like real terminal splits. A pane is padded with its own background
/// color - never rescaled - so its glyphs keep the crisp 2x rendering and their
/// aspect ratio, exactly like a tmux pane that is smaller than its neighbour.
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

    // Pad every pane to a common cross-axis size so the panes align. Resizing
    // would resample the text, so a smaller pane is extended with its own
    // background instead and stays pixel-for-pixel as rendered.
    let (target_w, target_h) = match layout {
        ComposeLayout::Horizontal => (0, sources.iter().map(|i| i.height()).max().unwrap_or(1)),
        ComposeLayout::Vertical => (sources.iter().map(|i| i.width()).max().unwrap_or(1), 0),
    };
    let panes: Vec<RgbaImage> = sources
        .into_iter()
        .map(|img| {
            let w = target_w.max(img.width()).max(1);
            let h = target_h.max(img.height()).max(1);
            if (w, h) == img.dimensions() {
                return img;
            }
            let pane_bg = sample_background(&img, canvas_bg);
            let mut padded: RgbaImage = ImageBuffer::from_pixel(w, h, pane_bg);
            image::imageops::overlay(&mut padded, &img, 0, 0);
            padded
        })
        .collect();

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
    /// Take the colored text from the parsed *screen* rather than echoing the
    /// source bytes.
    ///
    /// Set for interactive captures, whose raw stream is a terminal session
    /// rather than a document: it carries readline's redraws, bracketed-paste
    /// and window-title sequences, cursor motion, and the trailing prompt the
    /// screenshot deliberately drops. Reading those bytes as text shows things
    /// that were never on screen, so the screen itself is the honest source.
    ///
    /// Left unset for `render`, where the bytes *are* the document, so the
    /// text returned is the file itself rather than a re-rendering of it.
    /// A head/tail line selection overrides this: text that does not match the
    /// picture would only mislead.
    pub from_screen: bool,
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

/// Whether `font` genuinely covers `ch`.
///
/// A font that lacks a character maps it to glyph 0 (`.notdef`), which most
/// faces draw as a tofu box. Rasterizing that box would "succeed" and hide the
/// gap, so glyph 0 never counts as coverage - a character mapped to it must
/// keep walking the fallback chain. Glyphs that exist but rasterize to nothing
/// at this size (an empty outline in a broken font) are rejected the same way,
/// while whitespace is accepted because an empty box is exactly right for it.
fn font_covers(font: &Font, ch: char, size: f32) -> bool {
    if !font.has_glyph(ch) {
        return false;
    }
    if ch.is_whitespace() {
        return true;
    }
    let metrics = font.metrics(ch, size);
    metrics.width > 0 && metrics.height > 0
}

/// A font picked for one character, plus whether it came from the fallback
/// chain. Fallback faces have their own metrics, so their glyphs are placed
/// differently (see [`GlyphPlacement`]) to keep the monospace grid intact.
struct ChosenFont<'a> {
    font: &'a Font,
    is_fallback: bool,
}

/// How a glyph is positioned inside its cell.
///
/// Primary-font glyphs use their own side bearing and their own font's ascent,
/// exactly as before fallback existed. Fallback glyphs instead get the primary
/// font's ascent (so every glyph on a line shares one baseline) and are
/// centered inside the cell they occupy (so a face with different metrics
/// cannot shift the monospace grid).
#[derive(Clone, Copy, Default)]
struct GlyphPlacement {
    /// Baseline offset from the top of the line box. `None` means "use the
    /// glyph's own font ascent".
    ascent: Option<f32>,
    /// When set, the glyph is centered horizontally in a box of this width
    /// instead of being placed at its left side bearing.
    center_width: Option<u32>,
}

impl GlyphPlacement {
    /// Placement for a glyph drawn from the primary (or bold) face.
    fn natural() -> Self {
        Self::default()
    }

    /// Placement for a fallback glyph: primary baseline, centered in a cell
    /// (or a wide character's two cells) of `width` pixels.
    fn fallback(ascent: f32, width: u32) -> Self {
        Self {
            ascent: Some(ascent),
            center_width: Some(width),
        }
    }
}

/// Rasterization size for a fallback glyph: `font_size` scaled so the glyph's
/// advance matches `target_advance` (the width of the cell, or of both cells of
/// a wide character, in device pixels).
///
/// Fonts disagree on how much of the em an advance takes - MonoLisa's is 0.64em
/// against JetBrains Mono's 0.6em - so a fallback glyph drawn at the nominal
/// size is narrower than the cell it lands in. That is invisible for a symbol
/// but breaks box drawing, where every `─` would leave a gap and a `bat` frame
/// would come out dashed. Matching the advance makes those runs tile.
///
/// The ratio is clamped so an unusual fallback face (a proportional font, or a
/// glyph with no advance at all) cannot blow the glyph up or shrink it away.
fn fallback_font_size(font: &Font, ch: char, font_size: f32, target_advance: u32) -> f32 {
    let advance = font.metrics(ch, font_size).advance_width;
    if advance <= 0.0 || target_advance == 0 {
        return font_size;
    }
    let ratio = (target_advance as f32 / advance).clamp(0.5, 1.5);
    font_size * ratio
}

/// Save an RGBA image as a PNG, optionally embedding `description` as an
/// `iTXt` chunk under the standard `Description` keyword so screen readers and
/// other assistive tooling can read the terminal text back out of the image.
///
/// `iTXt` (rather than `tEXt`) is used because it is the only PNG text chunk
/// that carries UTF-8: terminal output routinely contains box drawing (`bat`,
/// `tree`), Greek, CJK, and symbols, all of which `tEXt`'s Latin-1 encoding
/// would destroy. The text is only sanitized of control characters and capped
/// at [`MAX_DESCRIPTION_BYTES`] on a character boundary.
pub fn save_png(img: &RgbaImage, path: &Path, description: Option<&str>) -> Result<()> {
    let file = std::fs::File::create(path)
        .with_context(|| format!("Failed to create image file {:?}", path))?;
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, img.width(), img.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Some(text) = description {
        let text = description_text_chunk(text);
        if !text.is_empty() {
            encoder
                .add_itxt_chunk("Description".to_string(), text)
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

/// Maximum size of an embedded `Description` text chunk. Sized so the text of
/// a full capture - which is every retained line, not just the last screenful -
/// fits: at ~60 bytes a line that covers several thousand lines, well past the
/// point where the image itself dominates the file. Longer captures are
/// truncated so a screenshot's metadata never dwarfs its pixels.
const MAX_DESCRIPTION_BYTES: usize = 256 * 1024;

/// Read the UTF-8 `Description` text embedded in a PNG by [`save_png`].
///
/// Only the `iTXt` chunk is consulted: that is the only PNG text chunk that
/// carries UTF-8, and it is the one termshot writes. A file without the chunk
/// (or one that cannot be read or decoded) yields `None` - missing metadata is
/// never an error, since composition must still work for foreign PNGs.
pub fn read_png_description(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path)
        .map_err(|e| tracing::debug!("No description read from {:?}: {}", path, e))
        .ok()?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let reader = decoder
        .read_info()
        .map_err(|e| tracing::debug!("No description read from {:?}: {}", path, e))
        .ok()?;
    let info = reader.info();
    let chunk = info
        .utf8_text
        .iter()
        .find(|c| c.keyword.eq_ignore_ascii_case("Description"))?;
    let text = chunk
        .get_text()
        .map_err(|e| tracing::debug!("Undecodable description in {:?}: {}", path, e))
        .ok()?;
    let text = text.trim_end_matches('\n').to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Build the `Description` metadata for a composed image from the descriptions
/// of the panes it was built from.
///
/// Each pane's text is separated by a clearly marked header so a reader can
/// tell where one terminal ends and the next begins. Panes that carry no
/// description are marked as such rather than silently dropped, so the pane
/// numbering always matches the image. Returns `None` when no pane has a
/// description at all - inventing one would only describe metadata that is not
/// there.
fn composed_description(paths: &[PathBuf]) -> Option<String> {
    let descriptions: Vec<Option<String>> = paths
        .iter()
        .map(|p| read_png_description(p))
        .collect::<Vec<_>>();
    if descriptions.iter().all(Option::is_none) {
        return None;
    }
    let mut out = String::new();
    for (idx, description) in descriptions.iter().enumerate() {
        if idx > 0 {
            out.push_str(&format!("\n\n--- Pane {} ---\n\n", idx + 1));
        }
        match description {
            Some(text) => out.push_str(text),
            None => out.push_str("(no description)"),
        }
    }
    Some(out)
}

/// Render a parsed screen back to colored text: one line per screen row, with
/// SGR sequences re-emitted wherever the style changes.
///
/// This is a *text* rendering, not a terminal stream to be re-parsed: rows are
/// separated by hard newlines, which is exactly what a reader wants and exactly
/// what a capture destined for the renderer must not do (it would erase the
/// terminal's soft-wrap information, and with it the ability to redact a value
/// that crossed the right margin).
fn screen_ansi_text(screen: &CapturedScreen, cols: u16) -> String {
    let (rows, _) = screen.size();
    let mut out = String::new();

    for row in 0..rows {
        if row > 0 {
            out.push('\n');
        }
        let mut style: Option<CellStyle> = None;
        // Trailing blanks carry no information; stop at the last filled cell.
        let last_col = (0..cols)
            .rev()
            .find(|&col| {
                screen
                    .cell(row, col)
                    .map(|c| c.has_contents())
                    .unwrap_or(false)
            })
            .map(|col| col + 1)
            .unwrap_or(0);

        for col in 0..last_col {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let cell_style = CellStyle::of(cell);
            if style != Some(cell_style) {
                out.push_str(&cell_style.sgr());
                style = Some(cell_style);
            }
            let contents = cell.contents();
            if contents.is_empty() {
                out.push(' ');
            } else {
                out.push_str(contents);
            }
        }
        if style.is_some() {
            out.push_str("\x1b[0m");
        }
    }

    out.trim_end().to_string()
}

/// The drawing attributes of one cell, used to emit an SGR sequence only where
/// the style actually changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl CellStyle {
    fn of(cell: &vt100::Cell) -> Self {
        Self {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    /// The SGR escape sequence that selects this style, starting from a reset
    /// so no attribute of a previous cell can linger.
    fn sgr(&self) -> String {
        let mut params: Vec<String> = vec!["0".to_string()];
        for (on, code) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.inverse, "7"),
        ] {
            if on {
                params.push(code.to_string());
            }
        }
        match self.fg {
            vt100::Color::Default => {}
            vt100::Color::Idx(i) if i < 8 => params.push(format!("{}", 30 + i)),
            vt100::Color::Idx(i) if i < 16 => params.push(format!("{}", 90 + i - 8)),
            vt100::Color::Idx(i) => params.push(format!("38;5;{}", i)),
            vt100::Color::Rgb(r, g, b) => params.push(format!("38;2;{};{};{}", r, g, b)),
        }
        match self.bg {
            vt100::Color::Default => {}
            vt100::Color::Idx(i) if i < 8 => params.push(format!("{}", 40 + i)),
            vt100::Color::Idx(i) if i < 16 => params.push(format!("{}", 100 + i - 8)),
            vt100::Color::Idx(i) => params.push(format!("48;5;{}", i)),
            vt100::Color::Rgb(r, g, b) => params.push(format!("48;2;{};{};{}", r, g, b)),
        }
        format!("\x1b[{}m", params.join(";"))
    }
}

/// Prepare terminal text for a PNG `iTXt` `Description` chunk.
///
/// The text stays UTF-8 - box drawing, Greek, CJK and symbols all survive
/// verbatim - so a screen reader gets what the terminal actually showed.
/// Newlines are kept because they carry the line structure of the capture;
/// other control characters are dropped, since `iTXt` text is a plain string
/// and stray escapes would only confuse a reader. The result is capped at
/// [`MAX_DESCRIPTION_BYTES`], truncated on a character boundary so the chunk
/// is never invalid UTF-8.
fn description_text_chunk(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_DESCRIPTION_BYTES));
    for ch in text.chars() {
        if ch != '\n' && ((ch as u32) < 0x20 || (ch as u32) == 0x7f) {
            continue;
        }
        if out.len() + ch.len_utf8() > MAX_DESCRIPTION_BYTES {
            break;
        }
        out.push(ch);
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// The fonts used to draw one theme: its primary face, an optional real bold
/// face, and the fallback chain searched for characters the primary face does
/// not cover. Cell metrics are derived from the primary face, so a theme with a
/// wider font gets a proportionally wider grid.
///
/// One of these is built per distinct font chain when the [`Renderer`] is
/// constructed and shared (behind an `Arc`) by every theme that resolves to the
/// same files, so no font is ever parsed per screenshot - let alone per cell.
pub struct ThemeFonts {
    font: Font,
    font_bold: Option<Font>,
    /// Fonts searched, in order, for characters the primary face does not
    /// have. The embedded JetBrains Mono is always first, followed by the
    /// fonts a theme listed in `fallback_fonts`.
    fallback_fonts: Vec<Font>,
    cell_width: u32,
    cell_height: u32,
}

/// The font files one theme resolves to. Doubles as the cache key that lets
/// themes sharing a chain share one parsed [`ThemeFonts`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FontChainSpec {
    primary: Option<PathBuf>,
    bold: Option<PathBuf>,
    fallbacks: Vec<PathBuf>,
}

impl ThemeFonts {
    /// Load one font chain. The primary and bold faces are hard errors when
    /// they cannot be read or parsed (the user asked for that exact file);
    /// fallback entries are best-effort and only shrink the chain.
    fn load(spec: &FontChainSpec, font_size: f32) -> Result<Self> {
        // Use an explicitly configured font file if provided, otherwise fall
        // back to the font embedded in the binary.
        let font_data = match spec.primary.as_deref() {
            Some(path) => {
                std::fs::read(path).with_context(|| format!("Failed to read font: {:?}", path))?
            }
            None => EMBEDDED_FONT.to_vec(),
        };
        let font = Font::from_bytes(font_data, FontSettings::default())
            .map_err(|e| anyhow::anyhow!("Failed to parse font: {}", e))?;

        // Load bold font if provided
        let font_bold = if let Some(path) = spec.bold.as_deref() {
            let data = std::fs::read(path)
                .with_context(|| format!("Failed to read bold font: {:?}", path))?;
            Some(
                Font::from_bytes(data, FontSettings::default())
                    .map_err(|e| anyhow::anyhow!("Failed to parse bold font: {}", e))?,
            )
        } else {
            None
        };

        // Fallback chain. The embedded JetBrains Mono always comes first so a
        // theme font without box drawing, arrows, or symbols (MonoLisa, for
        // example) still renders them instead of tofu; fonts the theme
        // configured are searched after it. A fallback that cannot be read or
        // parsed is skipped with a warning - one bad entry must never stop the
        // renderer from starting.
        let mut fallback_fonts = Vec::with_capacity(spec.fallbacks.len() + 1);
        match Font::from_bytes(EMBEDDED_FONT, FontSettings::default()) {
            Ok(embedded) => fallback_fonts.push(embedded),
            Err(e) => tracing::warn!("Failed to parse embedded fallback font: {}", e),
        }
        for path in &spec.fallbacks {
            match std::fs::read(path) {
                Ok(data) => match Font::from_bytes(data, FontSettings::default()) {
                    Ok(parsed) => fallback_fonts.push(parsed),
                    Err(e) => tracing::warn!("Ignoring unusable fallback font {:?}: {}", path, e),
                },
                Err(e) => tracing::warn!("Ignoring unreadable fallback font {:?}: {}", path, e),
            }
        }

        let metrics = font.metrics('M', font_size);
        let cell_width = metrics.advance_width.ceil() as u32;
        let cell_height = font_line_height(&font, font_size).ceil() as u32;

        Ok(Self {
            font,
            font_bold,
            fallback_fonts,
            cell_width,
            cell_height,
        })
    }
}

/// Font inputs that are not part of a theme: the user's explicit overrides and
/// the globally configured font.
///
/// A theme's own `font`/`font_bold`/`fallback_fonts` are read from its
/// [`ThemeConfig`], so every theme gets its own chain regardless of how it is
/// selected (CLI flag or MCP request parameter).
#[derive(Debug, Clone, Default)]
pub struct FontSelection {
    /// Explicit user override (CLI `--font`). Wins over a theme's own font.
    pub font_override: Option<PathBuf>,
    /// Explicit user override (CLI `--font-bold`). Wins over a theme's own
    /// bold font.
    pub font_bold_override: Option<PathBuf>,
    /// Globally configured font (`font_path` in the config file), used by
    /// themes that declare no font of their own.
    pub global_font: Option<PathBuf>,
    /// Fallback fonts applied to every theme, searched after the theme's own
    /// fallback fonts.
    pub global_fallback_fonts: Vec<PathBuf>,
}

impl FontSelection {
    /// Resolve the font chain for one theme: explicit overrides first, then
    /// the theme's own fonts, then the globally configured font, and finally
    /// the embedded font (represented by `None`).
    fn spec_for(&self, theme: Option<&ThemeConfig>) -> FontChainSpec {
        let (theme_font, theme_bold) = match theme {
            Some(t) => t.resolved_font_paths(),
            None => (None, None),
        };
        let mut fallbacks = match theme {
            Some(t) => t.resolved_fallback_font_paths(),
            None => Vec::new(),
        };
        fallbacks.extend(self.global_fallback_fonts.iter().cloned());
        FontChainSpec {
            primary: self
                .font_override
                .clone()
                .or(theme_font)
                .or_else(|| self.global_font.clone()),
            bold: self.font_bold_override.clone().or(theme_bold),
            fallbacks,
        }
    }
}

/// Capture settings for a [`Renderer`].
///
/// A struct rather than another constructor parameter so later settings can be
/// added without changing anyone's call: build one with
/// [`RendererOptions::default`] and override only what you need, either through
/// the builder method or with `..RendererOptions::default()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendererOptions {
    /// How many scrolled-off lines a capture retains. See [`crate::capture`]:
    /// the viewport height decides where lines wrap, this decides how much of
    /// the output survives to be rendered. Clamped per capture to what the
    /// retained-cell budget allows for the terminal's width.
    ///
    /// This is a *tail*-retention bound. A [`LineSelection::Head`] render
    /// ignores it and streams the beginning of the output as it scrolls past,
    /// so `Head(n)` is lines 1..n at any capacity.
    pub max_scrollback_lines: usize,
}

impl Default for RendererOptions {
    fn default() -> Self {
        Self {
            max_scrollback_lines: DEFAULT_MAX_SCROLLBACK_LINES,
        }
    }
}

impl RendererOptions {
    /// Set how many scrolled-off lines a capture retains.
    pub fn with_max_scrollback_lines(mut self, lines: usize) -> Self {
        self.max_scrollback_lines = lines;
        self
    }
}

/// Per-render settings for [`Renderer::render_bytes_with_options`].
///
/// Defaults to rendering every retained line, which is exactly what the
/// 1.0.0 [`Renderer::render_bytes`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderOptions {
    /// Which of the retained lines to render.
    pub lines: LineSelection,
}

impl RenderOptions {
    /// Render only the lines `selection` names.
    pub fn with_lines(mut self, selection: LineSelection) -> Self {
        self.lines = selection;
        self
    }
}

/// Renders a vt100 screen buffer to a PNG image.
pub struct Renderer {
    /// Font chain per theme name, so a screenshot rendered with theme `x`
    /// always uses `x`'s primary, bold, and fallback fonts. Built once at
    /// construction; themes with identical font files share one entry.
    theme_fonts: HashMap<String, Arc<ThemeFonts>>,
    /// Chain used for the default theme and for any theme without its own
    /// entry.
    default_fonts: Arc<ThemeFonts>,
    font_size: f32,
    themes: HashMap<String, Theme>,
    default_theme: String,
    default_chrome: ChromeOptions,
    padding: u32,
    /// How many scrolled-off lines a capture retains. See
    /// [`crate::capture`]: the viewport height decides where lines wrap, this
    /// decides how much of the output survives to be rendered.
    max_scrollback_lines: usize,
}

impl Renderer {
    /// Build a renderer with the default capture settings.
    ///
    /// This is the constructor published in 1.0.0 and its behaviour is
    /// unchanged. Use [`Renderer::new_with_options`] to set how much
    /// scrolled-off output a capture retains.
    pub fn new(
        fonts: &FontSelection,
        font_size: f32,
        theme_configs: &HashMap<String, ThemeConfig>,
        default_theme: &str,
        chrome_config: &ChromeConfig,
    ) -> Result<Self> {
        Self::new_with_options(
            fonts,
            font_size,
            theme_configs,
            default_theme,
            chrome_config,
            RendererOptions::default(),
        )
    }

    /// [`Renderer::new`] with explicit capture options.
    pub fn new_with_options(
        fonts: &FontSelection,
        font_size: f32,
        theme_configs: &HashMap<String, ThemeConfig>,
        default_theme: &str,
        chrome_config: &ChromeConfig,
        options: RendererOptions,
    ) -> Result<Self> {
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

        // The default chain is the one used when no theme is requested (and
        // when a theme's own fonts cannot be loaded), so a broken font here is
        // a hard error rather than a silent downgrade.
        let default_spec = fonts.spec_for(theme_configs.get(default_theme));
        let default_fonts = Arc::new(ThemeFonts::load(&default_spec, font_size)?);

        // One font chain per theme, deduplicated by the files it resolves to:
        // the common case (every theme using the same font) parses one chain.
        let mut by_spec: HashMap<FontChainSpec, Arc<ThemeFonts>> = HashMap::new();
        by_spec.insert(default_spec, Arc::clone(&default_fonts));
        let mut theme_fonts: HashMap<String, Arc<ThemeFonts>> = HashMap::new();
        for name in themes.keys() {
            let spec = fonts.spec_for(theme_configs.get(name));
            if let Some(existing) = by_spec.get(&spec) {
                theme_fonts.insert(name.clone(), Arc::clone(existing));
                continue;
            }
            match ThemeFonts::load(&spec, font_size) {
                Ok(loaded) => {
                    let loaded = Arc::new(loaded);
                    by_spec.insert(spec, Arc::clone(&loaded));
                    theme_fonts.insert(name.clone(), loaded);
                }
                Err(e) => {
                    // One theme's unusable font must not stop the renderer:
                    // that theme falls back to the default chain.
                    tracing::warn!(
                        "Failed to load fonts for theme '{}': {}; using the default fonts",
                        name,
                        e
                    );
                    theme_fonts.insert(name.clone(), Arc::clone(&default_fonts));
                }
            }
        }

        Ok(Self {
            theme_fonts,
            default_fonts,
            font_size,
            themes,
            default_theme: default_theme.to_string(),
            default_chrome: ChromeOptions::from_config(chrome_config),
            padding: 16,
            max_scrollback_lines: options.max_scrollback_lines,
        })
    }

    /// Resolve a requested theme name to one that exists, falling back to the
    /// default theme and then "dark". Theme colors and theme fonts are looked
    /// up with the same name, so they can never disagree.
    fn resolve_theme_name<'a>(&'a self, name: Option<&'a str>) -> &'a str {
        let name = name.unwrap_or(&self.default_theme);
        if self.themes.contains_key(name) {
            name
        } else if self.themes.contains_key(&self.default_theme) {
            &self.default_theme
        } else {
            "dark"
        }
    }

    /// Get a theme by name, falling back to default then "dark".
    pub fn get_theme(&self, name: Option<&str>) -> &Theme {
        let name = self.resolve_theme_name(name);
        self.themes
            .get(name)
            .or_else(|| self.themes.get(&self.default_theme))
            .or_else(|| self.themes.get("dark"))
            .expect("no themes available")
    }

    /// Get the font chain for a theme, falling back to the default chain.
    fn fonts_for(&self, name: Option<&str>) -> &ThemeFonts {
        let name = self.resolve_theme_name(name);
        self.theme_fonts
            .get(name)
            .map(Arc::as_ref)
            .unwrap_or(&self.default_fonts)
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
    /// `rows` is the PTY viewport height - it decides where lines wrap - not a
    /// limit on how much output is shown: the capture keeps everything that
    /// scrolled off (up to the renderer's scrollback capacity) and renders all
    /// of it. Use [`render_bytes_with_options`] to render only one end of it.
    ///
    /// No sidecar files are written; callers that need to re-render (e.g. the
    /// MCP server) should keep the raw bytes and returned [`RenderMeta`] in
    /// memory.
    ///
    /// [`render_bytes_with_options`]: Self::render_bytes_with_options
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
        let (path, text, audit, context) = self.render_bytes_with_options(
            data,
            cols,
            rows,
            output_dir,
            output_name,
            theme_name,
            chrome,
            redaction,
            text,
            auto_crop,
            RenderOptions::default(),
        )?;
        Ok((path, text, audit, context.meta))
    }

    /// [`Renderer::render_bytes`], rendering only the lines `options` selects.
    ///
    /// A head or tail selection also decides what the returned text and the
    /// PNG's `Description` metadata contain: text that does not match the
    /// picture would only mislead.
    #[allow(clippy::too_many_arguments)]
    pub fn render_bytes_with_options(
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
        options: RenderOptions,
    ) -> Result<RenderOutputWithContext> {
        let lines = options.lines;
        let capture = self.capture(data, rows, cols, lines);
        let screen = &capture;
        if capture.truncated() {
            tracing::warn!(
                "capture truncated: more output scrolled off than the {}-line scrollback \
                 could hold, so the oldest lines were dropped; raise `max_scrollback_lines` \
                 (a wide terminal caps it below the configured value) or render one end with \
                 a head/tail selection",
                self.effective_scrollback_lines(rows, cols)
            );
        }

        // A caller that asked for specific lines wants text that matches them,
        // so the text comes from the capture rather than from the source bytes.
        let text = TextOptions {
            from_screen: text.from_screen || !lines.is_all(),
            ..text
        };

        // Redaction pass: scan the parsed buffer before rendering.
        if let Some(req) = redaction {
            tracing::debug!("redaction: {} active rule(s)", req.engine.rule_count());
        }
        let redaction_map =
            redaction.map(|req| req.engine.redact_screen(screen, req.rules.as_deref()));

        if let Some(map) = &redaction_map
            && !map.is_empty()
        {
            tracing::info!(
                "redaction: masked {} cell(s) ({})",
                map.cell_count(),
                map.audit_summary()
            );
        }

        let plain_text = self.output_text(data, screen, cols, redaction_map.as_ref(), text);

        let theme = self.get_theme(theme_name);
        // Fonts are selected by the same theme name as the colors, so a theme
        // requested per call (CLI flag or MCP parameter) renders with its own
        // primary, bold, and fallback fonts.
        let fonts = self.fonts_for(theme_name);
        let chrome = chrome.unwrap_or(&self.default_chrome);
        let image = self.render_to_image(
            screen,
            theme,
            fonts,
            chrome,
            redaction_map.as_ref(),
            auto_crop,
        )?;

        let base = sanitize_base_name(output_name.unwrap_or(""));
        let path = unique_png_path(output_dir, &base);
        let description = self.description_text(screen, cols, redaction_map.as_ref(), text);
        save_png(&image, &path, description.as_deref())?;

        let context = RenderContext {
            meta: RenderMeta {
                cols,
                rows,
                theme: theme_name.map(str::to_owned),
                chrome: Some(chrome.clone()),
                auto_crop,
                from_screen: text.from_screen,
            },
            lines,
            truncated: capture.truncated(),
        };

        let audit = redaction_map.map(|m| m.counts).unwrap_or_default();
        Ok((path, plain_text, audit, context))
    }

    /// Capture the terminal session in `data` exactly the way
    /// [`render_bytes_with_options`] does, so a redaction map built from the
    /// returned capture addresses the same cells the renderer will draw.
    ///
    /// [`render_bytes_with_options`]: Self::render_bytes_with_options
    pub fn capture(
        &self,
        data: &[u8],
        rows: u16,
        cols: u16,
        lines: LineSelection,
    ) -> CapturedScreen {
        // Normalize bare LF to CRLF before feeding the terminal parser so raw,
        // non-TTY captured ANSI (piped output or redirected logs passed to
        // `render`) lands on proper lines instead of staircasing. This is a
        // no-op for PTY-sourced bytes (from `exec`), which already use CRLF,
        // and the original `data` is still used for the returned text.
        let normalized = normalize_newlines(data);
        CapturedScreen::parse_selected(&normalized, rows, cols, self.max_scrollback_lines, lines)
    }

    /// How many scrolled-off lines a capture of a `rows` x `cols` terminal
    /// actually retains: the configured limit, capped by the retained-cell
    /// budget that keeps a wide terminal from turning a line count into
    /// gigabytes. See [`crate::capture::effective_scrollback_lines`].
    pub fn effective_scrollback_lines(&self, rows: u16, cols: u16) -> usize {
        effective_scrollback_lines(rows, cols, self.max_scrollback_lines)
    }

    /// Capture the session behind an already rendered screenshot, using the
    /// geometry and line selection it was rendered with.
    pub fn capture_for(&self, data: &[u8], context: &RenderContext) -> CapturedScreen {
        self.capture(data, context.meta.rows, context.meta.cols, context.lines)
    }

    /// The `(rows, cols)` of `screen` a render with these settings actually
    /// paints.
    ///
    /// Not the same as [`CapturedScreen::size`]: the renderer stops at one row
    /// past the last row holding content, and with `auto_crop` it stops at the
    /// rightmost column holding content too, so a retained grid is routinely
    /// taller and wider than the image made from it. Callers that address cells
    /// in a finished screenshot - a coordinate redaction, say - have to be
    /// checked against *these* bounds, because a cell outside them is counted
    /// by the capture but never drawn.
    pub fn rendered_bounds(
        &self,
        screen: &CapturedScreen,
        theme_name: Option<&str>,
        auto_crop: bool,
    ) -> (u16, u16) {
        let layout = self.screen_layout(screen, self.fonts_for(theme_name), auto_crop);
        (
            u16::try_from(layout.content_rows).unwrap_or(u16::MAX),
            u16::try_from(layout.content_cols).unwrap_or(u16::MAX),
        )
    }

    /// [`Renderer::rendered_bounds`] for the theme and `auto_crop` setting a
    /// screenshot was rendered with, so a re-render draws exactly these cells.
    pub fn rendered_bounds_for(
        &self,
        screen: &CapturedScreen,
        context: &RenderContext,
    ) -> (u16, u16) {
        self.rendered_bounds(
            screen,
            context.meta.theme.as_deref(),
            context.meta.auto_crop,
        )
    }

    /// How many scrolled-off lines a capture retains before the terminal starts
    /// dropping the oldest ones.
    pub fn max_scrollback_lines(&self) -> usize {
        self.max_scrollback_lines
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
        self.render_redaction_to_with_context(
            data,
            &RenderContext::from_meta(meta.clone()),
            map,
            out_path,
            text,
        )
    }

    /// [`Renderer::render_redaction_to`], re-rendering the same line selection
    /// the screenshot was made with rather than the whole capture.
    ///
    /// This is what a caller holding a [`RenderContext`] - the MCP server's
    /// render cache, say - should use: a redaction map is addressed in the
    /// coordinates of the rows the image actually shows, so re-rendering a
    /// different set of rows would move every block.
    pub fn render_redaction_to_with_context(
        &self,
        data: &[u8],
        context: &RenderContext,
        map: &RedactionMap,
        out_path: &Path,
        text: TextOptions,
    ) -> Result<(String, Vec<(String, usize)>)> {
        let meta = &context.meta;
        let capture = self.capture_for(data, context);
        let screen = &capture;

        let plain_text = self.output_text(data, screen, meta.cols, Some(map), text);
        let theme = self.get_theme(meta.theme.as_deref());
        let chrome = meta
            .chrome
            .clone()
            .unwrap_or_else(|| self.default_chrome.clone());
        let fonts = self.fonts_for(meta.theme.as_deref());
        let image =
            self.render_to_image(screen, theme, fonts, &chrome, Some(map), meta.auto_crop)?;
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
    ///
    /// When `embed_description` is set (the global `embed_description` config),
    /// the panes' own `Description` metadata is read back and concatenated into
    /// the composite's `Description` chunk, so a composed image is as readable
    /// to assistive tooling as the screenshots it was built from.
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
        embed_description: bool,
    ) -> Result<PathBuf> {
        let theme = self.get_theme(theme_name);
        let fonts = self.fonts_for(theme_name);
        // Read the panes' descriptions before composing, so the composite can
        // carry them even though the pixels are merged.
        let description = if embed_description {
            composed_description(paths)
        } else {
            None
        };
        let composed = compose_images(paths, layout, divider, theme.background)?;
        let composed = match chrome {
            Some(chrome) if chrome.enabled => {
                self.compose_with_chrome(composed, theme, fonts, chrome)
            }
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
        save_png(&composed, &path, description.as_deref())
            .with_context(|| format!("Failed to save composed image to {:?}", path))?;
        Ok(path)
    }

    /// Compute the plain text to return to the caller.
    ///
    /// Precedence:
    /// * `redact_text` (with matches) -> stripped text with redaction blocks;
    /// * else `strip_ansi` -> plain (color-free) text from the parsed screen;
    /// * else `from_screen` -> the screen's text with its colors re-applied;
    /// * else the original raw output with ANSI color codes preserved.
    fn output_text(
        &self,
        data: &[u8],
        screen: &CapturedScreen,
        cols: u16,
        redaction: Option<&RedactionMap>,
        opts: TextOptions,
    ) -> String {
        if opts.redact_text
            && let Some(map) = redaction
            && !map.is_empty()
        {
            return map.redacted_plain_text(screen);
        }
        if opts.strip_ansi {
            screen
                .rows(0, cols)
                .collect::<Vec<String>>()
                .join("\n")
                .trim_end()
                .to_string()
        } else if opts.from_screen {
            screen_ansi_text(screen, cols)
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
        screen: &CapturedScreen,
        cols: u16,
        redaction: Option<&RedactionMap>,
        opts: TextOptions,
    ) -> Option<String> {
        if !opts.embed_description {
            return None;
        }
        if let Some(map) = redaction
            && !map.is_empty()
        {
            return Some(map.redacted_plain_text(screen));
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
    #[allow(clippy::too_many_arguments)]
    fn render_to_image(
        &self,
        screen: &CapturedScreen,
        theme: &Theme,
        fonts: &ThemeFonts,
        chrome: &ChromeOptions,
        redaction: Option<&RedactionMap>,
        auto_crop: bool,
    ) -> Result<RgbaImage> {
        let layout = self.screen_layout(screen, fonts, auto_crop);
        if chrome.enabled {
            // The terminal is drawn straight into the window frame rather than
            // into a buffer of its own that is then copied in: one full-size
            // layer instead of two, for an image that is already the largest
            // allocation in the process.
            let metrics = ChromeMetrics::new(chrome, layout.width, layout.height);
            metrics.check_budget(screen)?;
            return Ok(self.compose_with_chrome_layer(
                theme,
                fonts,
                chrome,
                &metrics,
                |renderer, frame, x, y| {
                    renderer.draw_screen_into(frame, x, y, &layout, screen, theme, fonts, redaction)
                },
            ));
        }
        // No chrome: optionally round the terminal content itself so a bare
        // screenshot still has soft corners on a transparent background,
        // like macOS window captures or code-screenshot tools.
        let mut img = self.render_screen(screen, theme, fonts, redaction, auto_crop)?;
        if chrome.rounded {
            self.round_image_corners(&mut img, chrome.radius * RENDER_SCALE);
        }
        Ok(img)
    }

    /// Geometry of the terminal portion of a screenshot: how many rows and
    /// columns actually hold content, and the pixel size they render to.
    /// Computed before anything is allocated so the memory budget can be
    /// checked against a number rather than against a failed allocation.
    fn screen_layout(
        &self,
        screen: &CapturedScreen,
        fonts: &ThemeFonts,
        auto_crop: bool,
    ) -> ScreenLayout {
        let rows = screen.size().0 as u32;
        let cols = screen.size().1 as u32;

        let content_rows = self.find_content_rows(screen, rows, cols);
        // Optionally trim the image width to the rightmost column that
        // actually holds content, so narrow output doesn't sit in a wide,
        // mostly-empty frame. The trim keeps the image's normal padding on the
        // right and nothing more, so both margins match.
        let content_cols = if auto_crop {
            self.find_content_cols(screen, content_rows, cols)
        } else {
            cols
        };

        let scale = RENDER_SCALE;
        let cell_w = fonts.cell_width * scale;
        let cell_h = fonts.cell_height * scale;
        let padding = self.padding * scale;

        ScreenLayout {
            content_rows,
            content_cols,
            cell_w,
            cell_h,
            padding,
            width: content_cols * cell_w + padding * 2,
            height: content_rows * cell_h + padding * 2,
        }
    }

    /// Render a capture to a standalone RGBA image.
    /// Renders at 2x resolution for sharper text.
    fn render_screen(
        &self,
        screen: &CapturedScreen,
        theme: &Theme,
        fonts: &ThemeFonts,
        redaction: Option<&RedactionMap>,
        auto_crop: bool,
    ) -> Result<RgbaImage> {
        let layout = self.screen_layout(screen, fonts, auto_crop);
        // One buffer, allocated below and returned to the caller.
        check_render_budget(layout.width, layout.height, 1, screen)?;
        let mut img: RgbaImage =
            ImageBuffer::from_pixel(layout.width, layout.height, theme.background);
        self.draw_screen_into(&mut img, 0, 0, &layout, screen, theme, fonts, redaction);
        Ok(img)
    }

    /// Draw a capture into `img` with its top-left corner at `(ox, oy)`.
    ///
    /// The background is painted here too, so a caller can hand over a buffer
    /// it allocated for other reasons - the window frame, say - instead of
    /// making one just for the terminal.
    #[allow(clippy::too_many_arguments)]
    fn draw_screen_into(
        &self,
        img: &mut RgbaImage,
        ox: u32,
        oy: u32,
        layout: &ScreenLayout,
        screen: &CapturedScreen,
        theme: &Theme,
        fonts: &ThemeFonts,
        redaction: Option<&RedactionMap>,
    ) {
        let ScreenLayout {
            content_rows,
            content_cols,
            cell_w,
            cell_h,
            padding,
            width,
            height,
        } = *layout;
        let hi_font_size = self.font_size * RENDER_SCALE as f32;
        let scale = RENDER_SCALE;

        self.draw_rect(img, ox, oy, width, height, theme.background);

        for row in 0..content_rows {
            for col in 0..content_cols {
                if let Some(cell) = screen.cell(row as u16, col as u16) {
                    let x = ox + col * cell_w + padding;
                    let y = oy + row * cell_h + padding;

                    // Redaction: draw a block (with an optional short label)
                    // in place of sensitive cell contents, using the color the
                    // matching rule requested.
                    if let Some(rc) = redaction.and_then(|m| m.get(row as u16, col as u16)) {
                        let block =
                            Rgba([rc.block_color[0], rc.block_color[1], rc.block_color[2], 255]);
                        self.draw_rect(img, x, y, cell_w, cell_h, block);
                        if let Some(label_ch) = rc.label_char {
                            let label = Rgba([
                                rc.label_color[0],
                                rc.label_color[1],
                                rc.label_color[2],
                                255,
                            ]);
                            self.draw_char_with_font(
                                img,
                                fonts,
                                x,
                                y,
                                label_ch,
                                label,
                                false,
                                hi_font_size,
                                &self.font_for_char(fonts, label_ch, false, hi_font_size),
                                cell_w,
                            );
                        }
                        continue;
                    }

                    let (fg_color, bg_color) = self.resolve_cell_colors(cell, theme);

                    // Draw background
                    if bg_color != theme.background {
                        self.draw_rect(img, x, y, cell_w, cell_h, bg_color);
                    }

                    // Draw character(s) at 2x font size
                    let ch = cell.contents();
                    if !ch.is_empty() && ch != " " {
                        // A double-width character owns this cell and the
                        // vt100 continuation cell after it, so a fallback
                        // glyph is centered across both.
                        let cell_span = if cell.is_wide() { cell_w * 2 } else { cell_w };
                        let bold = cell.bold();
                        let has_bold_font = fonts.font_bold.is_some();
                        for c in ch.chars() {
                            let chosen = self.font_for_char(fonts, c, bold, hi_font_size);
                            self.draw_char_with_font(
                                img,
                                fonts,
                                x,
                                y,
                                c,
                                fg_color,
                                cell.italic(),
                                hi_font_size,
                                &chosen,
                                cell_span,
                            );
                            // Faux bold only when no bold font is available
                            if bold && !has_bold_font {
                                self.draw_char_with_font(
                                    img,
                                    fonts,
                                    x + 1,
                                    y,
                                    c,
                                    fg_color,
                                    cell.italic(),
                                    hi_font_size,
                                    &chosen,
                                    cell_span,
                                );
                            }
                        }
                    }

                    // Underline
                    if cell.underline() {
                        let uy = y + cell_h.saturating_sub(2 * scale);
                        self.draw_rect(img, x, uy, cell_w, scale, fg_color);
                    }
                }
            }
        }
    }

    /// Compose a chrome-framed screenshot around a terminal image.
    ///
    /// Kept as the image-in, image-out form for callers that already hold a
    /// rendered terminal; [`Renderer::render_to_image`] uses
    /// [`Renderer::compose_with_chrome_layer`] instead so the terminal is drawn
    /// straight into the frame and only one full-size layer exists at a time.
    fn compose_with_chrome(
        &self,
        terminal: RgbaImage,
        theme: &Theme,
        fonts: &ThemeFonts,
        chrome: &ChromeOptions,
    ) -> RgbaImage {
        if !chrome.enabled {
            return terminal;
        }
        let metrics = ChromeMetrics::new(chrome, terminal.width(), terminal.height());
        self.compose_with_chrome_layer(theme, fonts, chrome, &metrics, |_, frame, ox, oy| {
            for y in 0..terminal.height() {
                for x in 0..terminal.width() {
                    frame.put_pixel(ox + x, oy + y, *terminal.get_pixel(x, y));
                }
            }
        })
    }

    /// Paint the window frame and let `draw_terminal` fill in the terminal
    /// area, at the offset it is given, directly inside the frame layer.
    ///
    /// Only the frame exists while the terminal is drawn. The shadowed canvas
    /// is allocated afterwards and only when there is a shadow to draw: without
    /// one the frame *is* the finished image, since compositing it over an
    /// empty transparent canvas would copy it pixel for pixel.
    fn compose_with_chrome_layer(
        &self,
        theme: &Theme,
        fonts: &ThemeFonts,
        chrome: &ChromeOptions,
        metrics: &ChromeMetrics,
        draw_terminal: impl FnOnce(&Self, &mut RgbaImage, u32, u32),
    ) -> RgbaImage {
        // Chrome is drawn at the same supersampling factor as the terminal so
        // the title bar, controls, and text stay proportional to the content.
        let scale = RENDER_SCALE;
        let &ChromeMetrics {
            radius,
            title_bar,
            shadow,
            frame_pad,
            bottom_pad,
            frame_w,
            frame_h,
            term_h,
            width,
            height,
        } = metrics;

        // The window body is always the terminal's own background, for every
        // preset: a screenshot must not sit inside a mismatched gray (or
        // otherwise off-theme) border. Presets differ in their title bar, not
        // in the color surrounding the capture.
        let frame_bg = theme.background;

        // The window is painted square into its own layer and rounded once, at
        // the end, with the same anti-aliased corner mask a chrome-less
        // screenshot gets. Every preset therefore shares one corner
        // implementation, and because the layer is the only opaque thing
        // composited onto the canvas, nothing outside the rounded frame can
        // stay opaque - no gray, black, or theme-colored halo around the
        // window.
        let mut frame: RgbaImage = ImageBuffer::from_pixel(frame_w, frame_h, frame_bg);

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

            // A plain rectangle: the corner mask applied to the whole layer
            // below rounds the title bar's top corners exactly like the frame's,
            // so there is no seam and no frame-colored wedge to paint over.
            self.draw_rect(&mut frame, 0, 0, frame_w, title_bar, title_bg);

            self.draw_title_bar_accents(&mut frame, chrome, 0, 0, frame_w, title_bar, theme, scale);
            if let Some(title) = chrome.title.as_deref() {
                let title = truncate_title(title);
                self.draw_text_line(
                    &mut frame,
                    fonts,
                    frame_w / 2,
                    title_bar / 2,
                    &title,
                    theme.foreground,
                    self.font_size * 0.85 * scale as f32,
                );
            }
        }

        let term_x = frame_pad;
        let term_y = frame_pad + title_bar;
        draw_terminal(self, &mut frame, term_x, term_y);

        if chrome.timestamp {
            let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
            let color = muted_text_color(theme);
            let right_x = frame_w.saturating_sub(frame_pad.max(6 * scale));
            let center_y = term_y + term_h + bottom_pad / 2;
            self.draw_text_right_aligned(
                &mut frame,
                fonts,
                right_x,
                center_y,
                &timestamp,
                color,
                self.font_size * 0.65 * scale as f32,
            );
        }

        // One anti-aliased corner mask for every preset, identical to the one a
        // chrome-less screenshot gets.
        self.round_image_corners(&mut frame, radius);

        if shadow == 0 {
            // Nothing to composite onto: the canvas would be transparent
            // everywhere the frame is not, and source-over onto a transparent
            // pixel reproduces the source exactly. Return the layer itself
            // rather than allocating a second image of the same size to copy
            // it into.
            return frame;
        }

        // Transparent background so rounded corners don't have a colored border
        let mut img: RgbaImage = ImageBuffer::from_pixel(width, height, Rgba([0, 0, 0, 0]));
        let frame_x = shadow / 2;
        let frame_y = shadow / 2;
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

        // Composite the window over the shadowed canvas source-over, so the
        // shadow stays visible through the corner cut-outs and the area outside
        // the window keeps the frame layer's own alpha.
        for y in 0..frame.height() {
            for x in 0..frame.width() {
                let px = *frame.get_pixel(x, y);
                if px[3] == 0 {
                    continue;
                }
                self.put_pixel_blend(&mut img, frame_x + x, frame_y + y, px);
            }
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

    /// Fill a rounded rectangle with a one-pixel anti-aliased edge.
    ///
    /// Coverage comes from the rounded-rectangle signed distance evaluated at
    /// each pixel center - the same rule [`draw_circle`](Self::draw_circle) and
    /// [`round_image_corners`](Self::round_image_corners) use - so every
    /// rounded shape termshot draws has the same smooth edge instead of a
    /// staircase.
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
        if w == 0 || h == 0 || color[3] == 0 {
            return;
        }
        // Clamp the radius to what the rectangle can actually accommodate.
        let r = radius.min(w / 2).min(h / 2) as f32;
        let (half_w, half_h) = (w as f32 / 2.0, h as f32 / 2.0);
        let (center_x, center_y) = (x as f32 + half_w, y as f32 + half_h);
        for py in y..(y + h).min(img.height()) {
            let qy = ((py as f32 + 0.5) - center_y).abs() - (half_h - r);
            for px in x..(x + w).min(img.width()) {
                let qx = ((px as f32 + 0.5) - center_x).abs() - (half_w - r);
                // Signed distance to the rounded rectangle: negative inside.
                let distance =
                    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                let coverage = (0.5 - distance).clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                let alpha = (color[3] as f32 * coverage).round().clamp(0.0, 255.0) as u8;
                if alpha == 0 {
                    continue;
                }
                self.put_pixel_blend(img, px, py, Rgba([color[0], color[1], color[2], alpha]));
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
                // Every traffic light is the same true circle: one radius, one
                // vertical center, and centers spaced by a fixed pitch, so the
                // three buttons are identical discs rather than rounded squares
                // of drifting size.
                let radius = TRAFFIC_LIGHT_RADIUS * scale;
                let center_y = frame_y as f32 + title_bar_height as f32 / 2.0;
                for (i, color) in colors.into_iter().enumerate() {
                    let center_x = (frame_x
                        + (TRAFFIC_LIGHT_FIRST_CENTER + i as u32 * TRAFFIC_LIGHT_PITCH) * scale)
                        as f32;
                    self.draw_circle(img, center_x, center_y, radius as f32, color);
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
                        (pill_x + offset * scale) as f32,
                        (pill_y + pill_h / 2) as f32,
                        (2 * scale) as f32,
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

    /// Draw a filled circle of radius `r` centered on `(cx, cy)`, with a
    /// one-pixel anti-aliased edge.
    ///
    /// Coverage is the distance from the *pixel center* to the circle center,
    /// so the disc is symmetric about `(cx, cy)` in both axes: with `cx`/`cy`
    /// on a pixel boundary (an integer coordinate) and an even diameter, the
    /// mask has exactly the same width and height and the same number of lit
    /// pixels on either side of the center. Testing squared distance against a
    /// squared radius - rather than approximating a circle with a small rounded
    /// rectangle - is what keeps the macOS traffic lights round instead of
    /// leaving single-pixel spikes at the cardinal points.
    fn draw_circle(&self, img: &mut RgbaImage, cx: f32, cy: f32, r: f32, color: Rgba<u8>) {
        if r <= 0.0 || color[3] == 0 {
            return;
        }
        let (img_w, img_h) = img.dimensions();
        // One extra pixel on each side leaves room for the anti-aliased edge.
        let x0 = (cx - r - 1.0).floor().max(0.0) as u32;
        let y0 = (cy - r - 1.0).floor().max(0.0) as u32;
        let x1 = ((cx + r + 1.0).ceil().max(0.0) as u32).min(img_w);
        let y1 = ((cy + r + 1.0).ceil().max(0.0) as u32).min(img_h);
        for py in y0..y1 {
            let dy = py as f32 + 0.5 - cy;
            for px in x0..x1 {
                let dx = px as f32 + 0.5 - cx;
                let distance = (dx * dx + dy * dy).sqrt();
                // Full coverage inside, zero outside, linear across the last
                // pixel of the edge.
                let coverage = (r + 0.5 - distance).clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                let alpha = (color[3] as f32 * coverage).round().clamp(0.0, 255.0) as u8;
                if alpha == 0 {
                    continue;
                }
                self.put_pixel_blend(img, px, py, Rgba([color[0], color[1], color[2], alpha]));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text_line(
        &self,
        img: &mut RgbaImage,
        fonts: &ThemeFonts,
        center_x: u32,
        center_y: u32,
        text: &str,
        color: Rgba<u8>,
        size: f32,
    ) {
        let total_width = self.text_width(fonts, text, size);
        self.draw_text_at(
            img,
            fonts,
            center_x as i32 - (total_width as i32 / 2),
            center_y,
            text,
            color,
            size,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text_right_aligned(
        &self,
        img: &mut RgbaImage,
        fonts: &ThemeFonts,
        right_x: u32,
        center_y: u32,
        text: &str,
        color: Rgba<u8>,
        size: f32,
    ) {
        let total_width = self.text_width(fonts, text, size);
        self.draw_text_at(
            img,
            fonts,
            right_x as i32 - total_width as i32,
            center_y,
            text,
            color,
            size,
        );
    }

    fn text_width(&self, fonts: &ThemeFonts, text: &str, size: f32) -> u32 {
        // The chrome font is monospace, so every character occupies one fixed
        // cell whose width is derived from 'M'. Using a single cell advance for
        // every glyph keeps `text_width` in sync with `draw_text_at` and avoids
        // per-glyph rounding drift.
        let cell_advance = fonts.font.metrics('M', size).advance_width.round() as u32;
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
    #[allow(clippy::too_many_arguments)]
    fn draw_text_at(
        &self,
        img: &mut RgbaImage,
        fonts: &ThemeFonts,
        start_x: i32,
        center_y: u32,
        text: &str,
        color: Rgba<u8>,
        size: f32,
    ) {
        let cell_advance = fonts.font.metrics('M', size).advance_width.round() as i32;
        let line_height = font_line_height(&fonts.font, size);
        let line_top = center_y as i32 - (line_height as i32 / 2);
        let mut cursor_x = start_x;
        for ch in text.chars() {
            let chosen = self.font_for_char(fonts, ch, false, size);
            let advance = cell_advance.max(0) as u32;
            let (render_size, placement) = if chosen.is_fallback {
                (
                    fallback_font_size(chosen.font, ch, size, advance),
                    GlyphPlacement::fallback(font_ascent(&fonts.font, size), advance),
                )
            } else {
                (size, GlyphPlacement::natural())
            };
            self.draw_glyph(
                img,
                cursor_x,
                line_top,
                ch,
                color,
                false,
                render_size,
                chosen.font,
                placement,
            );
            cursor_x += cell_advance;
        }
    }

    fn find_content_rows(&self, screen: &CapturedScreen, rows: u32, cols: u32) -> u32 {
        let mut last_row_with_content = 0u32;
        for row in 0..rows {
            for col in 0..cols {
                // A row counts as content if it has visible text or any styled
                // cell (e.g. a colored background block), so background-only
                // rows are not cropped away.
                if screen
                    .cell(row as u16, col as u16)
                    .is_some_and(cell_has_content)
                {
                    last_row_with_content = row + 1;
                    break;
                }
            }
        }
        (last_row_with_content + 1).min(rows)
    }

    /// Scan the screen buffer for the rightmost column that holds meaningful
    /// content - visible text, or a styled/inverse cell such as a colored
    /// background block - and return the width, in cells, to render.
    ///
    /// The bound comes from the terminal buffer, never from the pixels of a
    /// finished image: a composed screenshot's margin is the theme background,
    /// which differs per theme and is indistinguishable from an empty cell.
    ///
    /// No spare cells are added. The image's normal [`Self::padding`] is the
    /// only gap kept to the right of the last glyph, so a trimmed screenshot
    /// has the same visual margin on the left and the right. That padding also
    /// absorbs ink that legitimately overhangs its cell - anti-aliased edges,
    /// the italic shear, the one-pixel faux-bold smear, and box drawing glyphs
    /// that reach past their advance - so nothing is clipped. A double-width
    /// character keeps its vt100 continuation cell, and a wrapped row reaches
    /// the last column, so neither is cut in half.
    fn find_content_cols(&self, screen: &CapturedScreen, rows: u32, cols: u32) -> u32 {
        let mut max_col = 0u32;
        for row in 0..rows {
            for col in (0..cols).rev() {
                if let Some(cell) = screen.cell(row as u16, col as u16)
                    && cell_has_content(cell)
                {
                    // A double-width character owns this cell and the
                    // (blank) continuation cell after it.
                    let end = if cell.is_wide() { col + 2 } else { col + 1 };
                    max_col = max_col.max(end.min(cols));
                    break;
                }
            }
        }

        // A floor keeps an empty capture from collapsing to a zero-width
        // image; it never widens output that already has content past it.
        max_col.clamp(MIN_CONTENT_COLS.min(cols), cols)
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
    ///   at `cell_x + xmin` so its side bearing is respected, unless
    ///   `placement` asks for it to be centered (fallback glyphs).
    /// * `line_top` is the top of the line box; the glyph sits on the shared
    ///   baseline at `line_top + ascent`, offset by its own `ymin`. Using the
    ///   baseline (instead of centering each bitmap) is what keeps descenders
    ///   hanging and stops the per-glyph vertical wobble. `placement` may
    ///   override the ascent so fallback faces share the primary baseline.
    /// * `italic` applies a synthetic shear, used by terminal content.
    /// * `font` may be the regular, bold, or a fallback face; `color`'s alpha
    ///   modulates the glyph coverage and the result is composited source-over.
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
        placement: GlyphPlacement,
    ) {
        let (metrics, bitmap) = font.rasterize(ch, font_size);
        if bitmap.is_empty() || metrics.width == 0 || metrics.height == 0 {
            return;
        }

        let ascent = placement
            .ascent
            .unwrap_or_else(|| font_ascent(font, font_size));
        let glyph_x = match placement.center_width {
            // Center by *advance*, not by the ink bitmap: a box drawing corner
            // like `┌` only inks the right half of its cell, and centering its
            // bitmap would pull it away from the line it must join. Offsetting
            // the whole advance keeps every glyph's internal geometry intact.
            Some(width) => {
                cell_x
                    + ((width as f32 - metrics.advance_width) / 2.0).round() as i32
                    + metrics.xmin
            }
            None => cell_x + metrics.xmin,
        };
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
    ///
    /// `cell_span` is the width in pixels of the box the character occupies -
    /// one cell, or two for a wide (double-width) character - and is used to
    /// center fallback glyphs.
    #[allow(clippy::too_many_arguments)]
    fn draw_char_with_font(
        &self,
        img: &mut RgbaImage,
        fonts: &ThemeFonts,
        x: u32,
        y: u32,
        ch: char,
        color: Rgba<u8>,
        italic: bool,
        font_size: f32,
        font: &ChosenFont<'_>,
        cell_span: u32,
    ) {
        let (render_size, placement) = if font.is_fallback {
            (
                fallback_font_size(font.font, ch, font_size, cell_span),
                GlyphPlacement::fallback(font_ascent(&fonts.font, font_size), cell_span),
            )
        } else {
            (font_size, GlyphPlacement::natural())
        };
        self.draw_glyph(
            img,
            x as i32,
            y as i32,
            ch,
            color,
            italic,
            render_size,
            font.font,
            placement,
        );
    }

    /// Pick the font used to draw `ch`, walking the fallback chain in order:
    /// the primary face (the bold face for a bold cell), then the embedded
    /// JetBrains Mono, then any fonts the theme configured. When nothing
    /// covers the character the primary face is returned, so the terminal
    /// shows its usual missing-glyph behavior rather than silently borrowing
    /// an unrelated glyph.
    fn font_for_char<'f>(
        &self,
        fonts: &'f ThemeFonts,
        ch: char,
        bold: bool,
        font_size: f32,
    ) -> ChosenFont<'f> {
        let primary = match (bold, fonts.font_bold.as_ref()) {
            (true, Some(bold_font)) => bold_font,
            _ => &fonts.font,
        };
        if font_covers(primary, ch, font_size) {
            return ChosenFont {
                font: primary,
                is_fallback: false,
            };
        }
        for font in &fonts.fallback_fonts {
            if font_covers(font, ch, font_size) {
                return ChosenFont {
                    font,
                    is_fallback: true,
                };
            }
        }
        ChosenFont {
            font: primary,
            is_fallback: false,
        }
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

/// Pixel geometry of the terminal portion of a screenshot, worked out before
/// anything is allocated.
#[derive(Debug, Clone, Copy)]
struct ScreenLayout {
    content_rows: u32,
    content_cols: u32,
    cell_w: u32,
    cell_h: u32,
    padding: u32,
    /// Pixel size of the terminal image, padding included.
    width: u32,
    height: u32,
}

/// Pixel geometry of a chrome-framed screenshot, worked out before anything is
/// allocated so [`ChromeMetrics::check_budget`] can refuse an impossible render
/// instead of the allocator aborting on it.
#[derive(Debug, Clone, Copy)]
struct ChromeMetrics {
    radius: u32,
    title_bar: u32,
    /// Extra canvas reserved for the drop shadow; zero when there is none, in
    /// which case the frame layer is the finished image.
    shadow: u32,
    frame_pad: u32,
    bottom_pad: u32,
    frame_w: u32,
    frame_h: u32,
    /// Pixel height of the terminal area inside the frame; the timestamp is
    /// laid out below it.
    term_h: u32,
    /// Final image size.
    width: u32,
    height: u32,
}

impl ChromeMetrics {
    fn new(chrome: &ChromeOptions, term_w: u32, term_h: u32) -> Self {
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
        let frame_w = term_w + frame_pad * 2;
        let frame_h = term_h + frame_pad + bottom_pad + title_bar;
        Self {
            radius,
            title_bar,
            shadow,
            frame_pad,
            bottom_pad,
            frame_w,
            frame_h,
            term_h,
            width: frame_w + shadow,
            height: frame_h + shadow,
        }
    }

    /// Refuse a render that would not fit in the memory budget. With a shadow
    /// the frame layer and the canvas it is composited onto are alive at the
    /// same time; without one the frame is the only buffer.
    fn check_budget(&self, screen: &CapturedScreen) -> Result<()> {
        let layers = if self.shadow > 0 { 2 } else { 1 };
        check_render_budget(self.width, self.height, layers, screen)
    }
}

/// Refuse to render an image the machine should not be asked to allocate.
///
/// Retained output is unbounded in practice, so a capture can ask for an image
/// of any size; `layers` is how many full-size RGBA buffers the render holds at
/// once, so the check reflects peak memory rather than just the final PNG. The
/// error names the limit that was actually reached, and the way out: render one
/// end of the capture instead of all of it.
fn check_render_budget(
    width: u32,
    height: u32,
    layers: u64,
    screen: &CapturedScreen,
) -> Result<()> {
    let pixels = u64::from(width) * u64::from(height);
    let bytes = pixels * BYTES_PER_PIXEL * layers;
    let reason = if pixels > MAX_IMAGE_PIXELS {
        format!(
            "{} megapixels, over the {} megapixel limit",
            pixels / 1_000_000,
            MAX_IMAGE_PIXELS / 1_000_000
        )
    } else if bytes > MAX_RENDER_BYTES {
        format!(
            "about {} MB of image buffers, over the {} MB limit",
            bytes / (1024 * 1024),
            MAX_RENDER_BYTES / (1024 * 1024)
        )
    } else {
        return Ok(());
    };
    anyhow::bail!(
        "the capture would render as a {}x{} px image, needing {}: it holds {} lines of \
         output. Narrow it with --head-lines/--tail-lines (head_lines/tail_lines over MCP), \
         or use fewer columns.",
        width,
        height,
        reason,
        screen.size().0
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `data` into a single-screenful capture: no scrollback, so the
    /// capture is exactly the `rows` x `cols` viewport. That is what the
    /// geometry tests below measure; the scrollback-backed behaviour has its
    /// own tests in [`crate::capture`] and below.
    fn screen_of(data: &[u8], rows: u16, cols: u16) -> CapturedScreen {
        CapturedScreen::parse(data, rows, cols, 0)
    }

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
        let renderer = renderer_with_chrome(ChromeOptions {
            enabled: false,
            preset: "none".to_string(),
            title: None,
            timestamp: false,
            shadow: true,
            radius: 14,
            rounded: true,
            outer_padding: 18,
            title_bar_height: 34,
        });

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

        let result =
            renderer.compose_with_chrome(terminal, &theme, &renderer.default_fonts, &chrome);
        assert!(result.width() > 100);
        assert!(result.height() > 50);
    }

    #[test]
    fn timestamp_reserves_bottom_padding() {
        let renderer = renderer_with_chrome(ChromeOptions {
            enabled: false,
            preset: "none".to_string(),
            title: None,
            timestamp: false,
            shadow: false,
            radius: 14,
            rounded: true,
            outer_padding: 0,
            title_bar_height: 34,
        });
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

        let result = renderer.compose_with_chrome(
            terminal,
            &Theme::dark(),
            &renderer.default_fonts,
            &chrome,
        );
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
        let captured = screen_of(b"hello world", 3, 20);
        let screen = &captured;

        renderer.default_chrome.rounded = true;
        let rounded = renderer
            .render_to_image(
                screen,
                &theme,
                &renderer.default_fonts,
                &renderer.default_chrome,
                None,
                false,
            )
            .unwrap();
        assert_eq!(rounded.get_pixel(0, 0)[3], 0, "rounded: corner transparent");

        renderer.default_chrome.rounded = false;
        let square = renderer
            .render_to_image(
                screen,
                &theme,
                &renderer.default_fonts,
                &renderer.default_chrome,
                None,
                false,
            )
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
        let one_m = r.text_width(&r.default_fonts, "M", size);
        assert!(one_m > 0);
        assert_eq!(
            r.text_width(&r.default_fonts, "MMMMMMMMMM", size),
            one_m * 10
        );
        assert_eq!(
            r.text_width(&r.default_fonts, "iiiiiiiiii", size),
            one_m * 10
        );
        assert_eq!(r.text_width(&r.default_fonts, "Mi.lWx/9-", size), one_m * 9);
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
        r.draw_text_at(
            &mut img,
            &r.default_fonts,
            10,
            40,
            "oxy",
            Rgba([255, 255, 255, 255]),
            size,
        );

        // Bottom-most inked row per glyph cell (cells are a fixed advance wide).
        let advance = r.text_width(&r.default_fonts, "M", size) as i32;
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
            &r.default_fonts,
            right_x,
            30,
            text,
            Rgba([255, 255, 255, 255]),
            size,
        );

        let advance = r.text_width(&r.default_fonts, "M", size);
        let start_x = right_x - r.text_width(&r.default_fonts, text, size);

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

    /// The bundled JetBrains Mono with fixed 8x16 cell metrics, used as the
    /// single font chain of the test renderers below.
    fn test_fonts() -> ThemeFonts {
        ThemeFonts {
            font: Font::from_bytes(
                std::fs::read("fonts/JetBrainsMono-Regular.ttf").expect("font present"),
                FontSettings::default(),
            )
            .expect("font parse"),
            font_bold: None,
            fallback_fonts: Vec::new(),
            cell_width: 8,
            cell_height: 16,
        }
    }

    /// A renderer with one font chain and the given chrome defaults.
    fn renderer_with_chrome(default_chrome: ChromeOptions) -> Renderer {
        Renderer {
            theme_fonts: HashMap::new(),
            default_fonts: Arc::new(test_fonts()),
            font_size: 16.0,
            themes: HashMap::new(),
            default_theme: "dark".to_string(),
            default_chrome,
            padding: 16,
            max_scrollback_lines: crate::capture::DEFAULT_MAX_SCROLLBACK_LINES,
        }
    }

    fn bare_renderer() -> Renderer {
        renderer_with_chrome(ChromeOptions {
            enabled: false,
            preset: "none".to_string(),
            title: None,
            timestamp: false,
            shadow: false,
            radius: 0,
            rounded: true,
            outer_padding: 0,
            title_bar_height: 0,
        })
    }

    /// Width trimming must leave exactly the renderer's own padding to the
    /// right of the last glyph - the same gap as on the left - rather than a
    /// handful of spare cells or the whole unused terminal width.
    #[test]
    fn width_trim_leaves_the_same_padding_on_both_sides() {
        let renderer = bare_renderer();
        let theme = Theme::dark();
        let cols = 120u16;
        // Fills 92% of the terminal: wide enough that an earlier "close
        // enough to full width" shortcut would have kept all 120 columns.
        let used = 110usize;
        let captured = screen_of("x".repeat(used).as_bytes(), 4, cols);
        let screen = &captured;

        let img = renderer
            .render_screen(screen, &theme, &renderer.default_fonts, None, true)
            .unwrap();

        let cell_w = renderer.default_fonts.cell_width * RENDER_SCALE;
        let padding = renderer.padding * RENDER_SCALE;
        assert_eq!(img.width(), used as u32 * cell_w + padding * 2);

        // The last cell still carries ink, so nothing was clipped, and the
        // margins on either side of the text match to within a cell.
        assert!(
            ink_in_columns(
                &img,
                padding + (used as u32 - 1) * cell_w,
                padding + used as u32 * cell_w
            ) > 0,
            "the last column of text was trimmed away"
        );
        let bg = theme.background;
        let inked = |x: u32| (0..img.height()).any(|y| *img.get_pixel(x, y) != bg);
        let left = (0..img.width()).find(|&x| inked(x)).expect("ink");
        let right = (0..img.width()).rev().find(|&x| inked(x)).expect("ink");
        let right_margin = img.width() - 1 - right;
        assert!(
            right_margin >= 1,
            "anti-aliased glyph edge touches the right border"
        );
        assert!(
            (left as i32 - right_margin as i32).unsigned_abs() <= cell_w,
            "asymmetric margins: {left}px on the left, {right_margin}px on the right"
        );
    }

    /// The trim bound is taken from the terminal buffer, so a styled cell with
    /// no text (a colored background block) still counts as content.
    #[test]
    fn width_trim_keeps_trailing_styled_cells() {
        let renderer = bare_renderer();
        let theme = Theme::dark();
        // "hi" then a green background run of four blank cells.
        let captured = screen_of(b"hi\x1b[42m    \x1b[0m", 4, 80);
        let img = renderer
            .render_screen(&captured, &theme, &renderer.default_fonts, None, true)
            .unwrap();

        let cell_w = renderer.default_fonts.cell_width * RENDER_SCALE;
        let padding = renderer.padding * RENDER_SCALE;
        assert_eq!(img.width(), 6 * cell_w + padding * 2);
        assert_ne!(
            *img.get_pixel(padding + 5 * cell_w + cell_w / 2, padding + 2),
            theme.background,
            "the trailing styled cell was trimmed away"
        );
    }

    /// A double-width character owns a blank vt100 continuation cell; trimming
    /// must keep it so the glyph is not sliced down the middle.
    #[test]
    fn width_trim_keeps_a_wide_characters_continuation_cell() {
        let renderer = bare_renderer();
        let theme = Theme::dark();
        let captured = screen_of("ab\u{4f60}".as_bytes(), 4, 80);
        let img = renderer
            .render_screen(&captured, &theme, &renderer.default_fonts, None, true)
            .unwrap();

        let cell_w = renderer.default_fonts.cell_width * RENDER_SCALE;
        let padding = renderer.padding * RENDER_SCALE;
        assert_eq!(img.width(), 4 * cell_w + padding * 2);
    }

    /// Trimming is opt-out: `auto_crop = false` still renders the full
    /// terminal width.
    #[test]
    fn width_trim_can_be_disabled() {
        let renderer = bare_renderer();
        let theme = Theme::dark();
        let captured = screen_of(b"hi", 4, 80);
        let img = renderer
            .render_screen(&captured, &theme, &renderer.default_fonts, None, false)
            .unwrap();

        let cell_w = renderer.default_fonts.cell_width * RENDER_SCALE;
        let padding = renderer.padding * RENDER_SCALE;
        assert_eq!(img.width(), 80 * cell_w + padding * 2);
    }

    #[test]
    fn redaction_masks_sensitive_cells_in_rendered_image() {
        use crate::redaction::{RedactionConfig, RedactionEngine};

        let renderer = bare_renderer();
        let theme = Theme::dark();
        let engine = RedactionEngine::from_config(&RedactionConfig::default()).unwrap();

        let captured = screen_of(b"ip 10.20.30.40 up", 3, 40);
        let screen = &captured;

        let map = engine.redact_screen(screen, None);
        assert!(!map.is_empty(), "expected the IPv4 address to be redacted");

        let redacted = renderer
            .render_screen(screen, &theme, &renderer.default_fonts, Some(&map), true)
            .unwrap();
        let plain = renderer
            .render_screen(screen, &theme, &renderer.default_fonts, None, true)
            .unwrap();

        // "[IP]" labels columns 3-6; sample a later plain-block column so we
        // land on solid redaction red rather than a label glyph.
        let scale = 2u32;
        let cell_w = renderer.default_fonts.cell_width * scale;
        let cell_h = renderer.default_fonts.cell_height * scale;
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
                    from_screen: false,
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

    /// Read the PNG `iTXt` chunk stored under `Description`, if any.
    fn png_description(path: &std::path::Path) -> Option<String> {
        let file = std::io::BufReader::new(std::fs::File::open(path).unwrap());
        let decoder = png::Decoder::new(file);
        let mut reader = decoder.read_info().unwrap();
        // `iTXt` chunks may trail the image data, so drain the frame first.
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut buf).unwrap();
        reader
            .info()
            .utf8_text
            .iter()
            .find(|chunk| chunk.keyword == "Description")
            .map(|chunk| chunk.get_text().expect("iTXt text should decode"))
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
        // Redaction blocks survive verbatim: `iTXt` carries UTF-8, so the
        // masked span reads as the same `█` run the image draws.
        assert!(
            description.contains("\u{2588}\u{2588}\u{2588}\u{2588}"),
            "got: {:?}",
            description
        );
        std::fs::remove_file(&path).ok();
    }

    /// The embedded description must survive non-Latin-1 terminal output:
    /// `bat`/`tree` box drawing, check marks, Greek, and CJK all have to come
    /// back out of the PNG byte-for-byte, with no `?` transliteration and no
    /// U+FFFD replacement.
    #[test]
    fn embedded_description_preserves_unicode() {
        let mut renderer = bare_renderer();
        renderer.themes.insert("dark".to_string(), Theme::dark());
        let out_dir = std::path::Path::new("target/description-test-out");
        std::fs::create_dir_all(out_dir).unwrap();

        let unicode = "\u{2500} \u{2502} \u{250c} \u{2713} \u{3bb} \u{6f22}";
        let source = format!("{}\n", unicode);

        let (path, _text, _audit, _meta) = renderer
            .render_bytes(
                source.as_bytes(),
                40,
                3,
                out_dir,
                Some("description unicode"),
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
            description.contains(unicode),
            "unicode did not round-trip: {:?}",
            description
        );
        assert!(
            !description.contains('?') && !description.contains('\u{fffd}'),
            "unicode was transliterated or replaced: {:?}",
            description
        );
        std::fs::remove_file(&path).ok();
    }

    /// Control characters are dropped, newlines are kept, and the cap
    /// truncates on a character boundary (never mid-codepoint).
    #[test]
    fn description_text_chunk_sanitizes_and_truncates_on_char_boundary() {
        assert_eq!(description_text_chunk("a\u{7}b\u{1b}c\nd\n\n"), "abc\nd");

        // Every character is 3 bytes, so the cap lands mid-character unless
        // truncation is boundary-aware.
        let long = "\u{6f22}".repeat(MAX_DESCRIPTION_BYTES);
        let capped = description_text_chunk(&long);
        assert!(capped.len() <= MAX_DESCRIPTION_BYTES);
        assert!(capped.chars().all(|c| c == '\u{6f22}'));
        assert_eq!(capped.len() % '\u{6f22}'.len_utf8(), 0);
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

    // ---------------------------------------------------------------------
    // Font fallback
    // ---------------------------------------------------------------------

    /// Stand-in for a real primary font (MonoLisa and friends) that only
    /// covers printable ASCII: no box drawing, no symbols, no CJK.
    const LIMITED_PRIMARY: &str = "tests/fixtures/limited-ascii.ttf";
    /// Stand-in for a user-configured fallback font (WenQuanYi Zen Hei and
    /// friends): a single CJK character and nothing else.
    const CJK_FALLBACK: &str = "tests/fixtures/cjk-fallback.ttf";

    /// Box drawing, symbol, and Greek characters `bat`/`batcat` frames use and
    /// MonoLisa does not ship.
    const FALLBACK_CHARS: [char; 5] = ['\u{2500}', '\u{2502}', '\u{250c}', '\u{2713}', '\u{3bb}'];

    fn renderer_with(primary: Option<&str>, fallbacks: &[&str]) -> Renderer {
        let fallbacks: Vec<PathBuf> = fallbacks.iter().map(PathBuf::from).collect();
        Renderer::new(
            &FontSelection {
                font_override: primary.map(PathBuf::from),
                font_bold_override: None,
                global_font: None,
                global_fallback_fonts: fallbacks,
            },
            16.0,
            &HashMap::new(),
            "dark",
            &ChromeConfig::default(),
        )
        .expect("renderer builds")
    }

    /// Render `text` through the normal terminal path and return the image.
    fn render_line(renderer: &Renderer, text: &str, cols: u16) -> RgbaImage {
        let captured = screen_of(text.as_bytes(), 2, cols);
        renderer
            .render_screen(
                &captured,
                &Theme::dark(),
                &renderer.default_fonts,
                None,
                false,
            )
            .expect("render")
    }

    /// Count pixels that differ from the theme background, i.e. how much ink a
    /// glyph actually put on the canvas.
    fn ink(img: &RgbaImage) -> usize {
        let bg = Theme::dark().background;
        img.pixels().filter(|p| **p != bg).count()
    }

    /// Ink inside the horizontal span `[x0, x1)` of the image.
    fn ink_in_columns(img: &RgbaImage, x0: u32, x1: u32) -> usize {
        let bg = Theme::dark().background;
        let mut count = 0;
        for y in 0..img.height() {
            for x in x0..x1.min(img.width()) {
                if *img.get_pixel(x, y) != bg {
                    count += 1;
                }
            }
        }
        count
    }

    /// A primary font without box drawing glyphs must hand those characters to
    /// the embedded JetBrains Mono instead of drawing a tofu box.
    #[test]
    fn missing_box_drawing_glyphs_fall_back_to_the_embedded_font() {
        let renderer = renderer_with(Some(LIMITED_PRIMARY), &[]);
        for ch in FALLBACK_CHARS {
            assert!(
                !font_covers(&renderer.default_fonts.font, ch, 32.0),
                "fixture primary font unexpectedly covers {:?}",
                ch
            );
            let chosen = renderer.font_for_char(&renderer.default_fonts, ch, false, 32.0);
            assert!(chosen.is_fallback, "{:?} did not use a fallback font", ch);
            assert!(
                font_covers(chosen.font, ch, 32.0),
                "fallback font chosen for {:?} does not cover it",
                ch
            );
        }
        // ASCII still comes from the primary font.
        let chosen = renderer.font_for_char(&renderer.default_fonts, 'M', false, 32.0);
        assert!(!chosen.is_fallback, "ASCII must stay on the primary font");
    }

    /// The embedded JetBrains Mono is always the first fallback, even when the
    /// theme configures extra fonts.
    #[test]
    fn embedded_font_is_always_the_first_fallback() {
        let renderer = renderer_with(Some(LIMITED_PRIMARY), &[CJK_FALLBACK]);
        assert_eq!(
            renderer.default_fonts.fallback_fonts.len(),
            2,
            "expected the embedded font plus one configured fallback"
        );
        let embedded = &renderer.default_fonts.fallback_fonts[0];
        for ch in FALLBACK_CHARS {
            assert!(
                font_covers(embedded, ch, 32.0),
                "embedded JetBrains Mono should cover {:?}",
                ch
            );
            let chosen = renderer.font_for_char(&renderer.default_fonts, ch, false, 32.0);
            assert!(
                std::ptr::eq(chosen.font, embedded),
                "{:?} skipped the embedded font",
                ch
            );
        }
    }

    /// The embedded font really rasterizes the box drawing and symbol
    /// characters, and they reach the canvas through the full render path.
    #[test]
    fn embedded_font_renders_box_drawing_and_symbols() {
        let renderer = renderer_with(Some(LIMITED_PRIMARY), &[]);
        let embedded = &renderer.default_fonts.fallback_fonts[0];
        for ch in FALLBACK_CHARS {
            let (metrics, bitmap) = embedded.rasterize(ch, 32.0);
            assert!(
                !bitmap.is_empty() && metrics.width > 0 && metrics.height > 0,
                "embedded font produced no bitmap for {:?}",
                ch
            );
        }

        let drawn: String = FALLBACK_CHARS.iter().collect();
        let img = render_line(&renderer, &drawn, 10);
        assert!(ink(&img) > 0, "fallback glyphs left no ink on the canvas");
    }

    /// A character no font in the chain covers keeps the terminal's normal
    /// missing-glyph behavior: nothing is borrowed from an unrelated font, and
    /// no tofu box is invented.
    #[test]
    fn uncovered_cjk_stays_missing_without_a_configured_fallback() {
        let renderer = renderer_with(Some(LIMITED_PRIMARY), &[]);
        let chosen = renderer.font_for_char(&renderer.default_fonts, '\u{4e2d}', false, 32.0);
        assert!(
            !chosen.is_fallback,
            "no fallback should cover this CJK char"
        );
        assert!(!font_covers(chosen.font, '\u{4e2d}', 32.0));
        assert_eq!(
            ink(&render_line(&renderer, "\u{4e2d}", 10)),
            0,
            "a character no font covers must not draw a substitute glyph"
        );
    }

    /// A fallback font configured by the theme is loaded and used for the
    /// characters only it covers.
    #[test]
    fn configured_fallback_font_is_loaded_and_selected() {
        let renderer = renderer_with(Some(LIMITED_PRIMARY), &[CJK_FALLBACK]);
        let chosen = renderer.font_for_char(&renderer.default_fonts, '\u{4e2d}', false, 32.0);
        assert!(chosen.is_fallback, "configured fallback was not selected");
        assert!(std::ptr::eq(
            chosen.font,
            &renderer.default_fonts.fallback_fonts[1]
        ));
        assert!(
            ink(&render_line(&renderer, "\u{4e2d}", 10)) > 0,
            "configured fallback glyph was not drawn"
        );
    }

    /// A fallback that does not exist, or is not a font at all, is skipped
    /// with a warning: the renderer still builds and the rest of the chain
    /// keeps working.
    #[test]
    fn broken_fallback_fonts_warn_and_are_skipped() {
        let dir = Path::new("target/font-fallback-test");
        std::fs::create_dir_all(dir).unwrap();
        let junk = dir.join("not-a-font.ttf");
        std::fs::write(&junk, b"this is definitely not a font").unwrap();

        let renderer = renderer_with(
            Some(LIMITED_PRIMARY),
            &[
                "target/font-fallback-test/missing-font.ttf",
                junk.to_str().unwrap(),
                CJK_FALLBACK,
            ],
        );

        // Embedded font + the one usable configured fallback.
        assert_eq!(renderer.default_fonts.fallback_fonts.len(), 2);
        assert!(
            renderer
                .font_for_char(&renderer.default_fonts, '\u{2500}', false, 32.0)
                .is_fallback
        );
        assert!(
            renderer
                .font_for_char(&renderer.default_fonts, '\u{4e2d}', false, 32.0)
                .is_fallback
        );
    }

    /// Fallback fonts must not disturb the monospace grid: cell metrics come
    /// from the primary font alone, and a fallback glyph stays inside its own
    /// cell.
    #[test]
    fn fallback_glyphs_keep_the_primary_cell_metrics() {
        let plain = renderer_with(Some(LIMITED_PRIMARY), &[]);
        let with_fallbacks = renderer_with(Some(LIMITED_PRIMARY), &[CJK_FALLBACK]);
        assert_eq!(
            plain.default_fonts.cell_width,
            with_fallbacks.default_fonts.cell_width
        );
        assert_eq!(
            plain.default_fonts.cell_height,
            with_fallbacks.default_fonts.cell_height
        );

        let cols = 4;
        let img = render_line(&with_fallbacks, "\u{2500}", cols);
        let expected_width = cols as u32 * plain.default_fonts.cell_width * RENDER_SCALE
            + plain.padding * RENDER_SCALE * 2;
        assert_eq!(
            img.width(),
            expected_width,
            "fallback changed the grid width"
        );

        // The glyph is drawn in its own (first) cell. Box drawing glyphs
        // deliberately overhang the advance width a little so horizontal rules
        // join across cells, so the check is that the ink never reaches the
        // middle of the neighbouring cell.
        let cell_w = plain.default_fonts.cell_width * RENDER_SCALE;
        let padding = plain.padding * RENDER_SCALE;
        assert!(ink_in_columns(&img, padding, padding + cell_w) > 0);
        assert_eq!(
            ink_in_columns(&img, padding + cell_w + cell_w / 2, img.width()),
            0,
            "fallback glyph spilled into the next cell"
        );
    }

    /// A fallback glyph is positioned as cleanly in its cell as a primary
    /// glyph is in its own: it fills the cell horizontally (so rules join) and
    /// sits at the same height on the line.
    #[test]
    fn fallback_glyphs_sit_cleanly_in_the_cell() {
        // Primary = embedded JetBrains Mono: the glyph is drawn directly.
        let direct = renderer_with(None, &[]);
        // Primary = the ASCII-only fixture, whose cell is wider (0.64 em, like
        // MonoLisa): the glyph now comes from the embedded fallback.
        let via_fallback = renderer_with(Some(LIMITED_PRIMARY), &[]);

        let bbox = |img: &RgbaImage| -> (u32, u32, u32, u32) {
            let bg = Theme::dark().background;
            let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
            for (x, y, p) in img.enumerate_pixels() {
                if *p != bg {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
            (x0, y0, x1, y1)
        };

        let measure = |r: &Renderer| {
            let img = render_line(r, "\u{2500}", 4);
            let padding = r.padding * RENDER_SCALE;
            let cell_w = r.default_fonts.cell_width * RENDER_SCALE;
            let (x0, y0, x1, y1) = bbox(&img);
            // Insets from the cell edges, and the ink's vertical midpoint
            // measured from the top of the line.
            (
                x0 as i64 - padding as i64,
                (padding + cell_w) as i64 - (x1 + 1) as i64,
                ((y0 + y1) / 2) as i64 - padding as i64,
            )
        };

        let (direct_left, direct_right, direct_mid) = measure(&direct);
        let (fb_left, fb_right, fb_mid) = measure(&via_fallback);

        assert!(
            fb_left <= direct_left + 1 && fb_right <= direct_right + 1,
            "fallback rule does not reach its cell edges (insets {}/{} vs {}/{})",
            fb_left,
            fb_right,
            direct_left,
            direct_right
        );
        assert!(
            fb_mid.abs_diff(direct_mid) <= 2,
            "fallback glyph sits at a different height: {} vs {}",
            fb_mid,
            direct_mid
        );
    }

    /// A double-width fallback glyph is centered across the cell *and* its
    /// vt100 continuation cell, and does not bleed into the next character.
    #[test]
    fn wide_fallback_glyphs_respect_continuation_cells() {
        let renderer = renderer_with(Some(LIMITED_PRIMARY), &[CJK_FALLBACK]);
        let cols = 6;
        let img = render_line(&renderer, "\u{4e2d}", cols);
        let cell_w = renderer.default_fonts.cell_width * RENDER_SCALE;
        let padding = renderer.padding * RENDER_SCALE;

        assert!(
            ink_in_columns(&img, padding, padding + cell_w * 2) > 0,
            "wide glyph drew nothing in its two cells"
        );
        assert_eq!(
            ink_in_columns(&img, padding + cell_w * 2, img.width()),
            0,
            "wide glyph spilled past its continuation cell"
        );
    }

    /// A horizontal rule drawn from a fallback font must tile: the glyph is
    /// scaled to the primary font's advance, so a run of `─` comes out as one
    /// unbroken line instead of a dashed one.
    #[test]
    fn fallback_box_drawing_tiles_without_gaps() {
        // A primary font whose cell is wider than the fallback's natural
        // advance is exactly the MonoLisa + JetBrains Mono case.
        let renderer = renderer_with(Some(LIMITED_PRIMARY), &[]);
        let cols = 6;
        let img = render_line(&renderer, &"\u{2500}".repeat(4), cols);
        let bg = Theme::dark().background;
        let padding = renderer.padding * RENDER_SCALE;
        let run_end = padding + renderer.default_fonts.cell_width * RENDER_SCALE * 4;

        // The row carrying the rule must have ink in every column of the run.
        let row = (0..img.height())
            .max_by_key(|y| {
                (0..img.width())
                    .filter(|x| *img.get_pixel(*x, *y) != bg)
                    .count()
            })
            .expect("image has rows");
        let gaps: Vec<u32> = (padding..run_end)
            .filter(|x| *img.get_pixel(*x, row) == bg)
            .collect();
        assert!(
            gaps.is_empty(),
            "horizontal rule from the fallback font has gaps at columns {:?}",
            gaps
        );
    }

    /// Box drawing corners only ink part of their cell, so they must be placed
    /// by advance (side bearings preserved) rather than by centering their ink:
    /// `┌` keeps its stub on the right, where the rule it joins begins.
    #[test]
    fn fallback_box_corners_keep_their_side_bearings() {
        let renderer = renderer_with(Some(LIMITED_PRIMARY), &[]);
        let img = render_line(&renderer, "\u{250c}", 4);
        let cell_w = renderer.default_fonts.cell_width * RENDER_SCALE;
        let padding = renderer.padding * RENDER_SCALE;
        let mid = padding + cell_w / 2;

        let left = ink_in_columns(&img, padding, mid);
        let right = ink_in_columns(&img, mid, padding + cell_w);
        assert!(
            right > left,
            "'┌' should ink mostly the right half of its cell (left {} vs right {})",
            left,
            right
        );
    }

    // ---------------------------------------------------------------------
    // Per-theme font chains
    // ---------------------------------------------------------------------

    fn theme_config_with_fonts(font: Option<&str>, fallbacks: &[&str]) -> ThemeConfig {
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

    /// Every theme gets the font chain it declares, and themes that resolve to
    /// the same files share one parsed chain instead of reparsing the fonts.
    #[test]
    fn each_theme_gets_its_own_font_chain() {
        let mut themes = HashMap::new();
        themes.insert(
            "ascii-only".to_string(),
            theme_config_with_fonts(Some(LIMITED_PRIMARY), &[]),
        );
        themes.insert(
            "cjk".to_string(),
            theme_config_with_fonts(Some(LIMITED_PRIMARY), &[CJK_FALLBACK]),
        );
        themes.insert("embedded".to_string(), theme_config_with_fonts(None, &[]));
        themes.insert(
            "embedded-too".to_string(),
            theme_config_with_fonts(None, &[]),
        );

        let renderer = Renderer::new(
            &FontSelection::default(),
            16.0,
            &themes,
            "embedded",
            &ChromeConfig::default(),
        )
        .expect("renderer builds");

        // Only the theme that configured the CJK fallback can draw the CJK
        // character; the others keep the terminal's missing-glyph behavior.
        let cjk_fonts = renderer.fonts_for(Some("cjk"));
        assert!(
            renderer
                .font_for_char(cjk_fonts, '\u{4e2d}', false, 32.0)
                .is_fallback
        );
        let ascii_fonts = renderer.fonts_for(Some("ascii-only"));
        assert!(
            !renderer
                .font_for_char(ascii_fonts, '\u{4e2d}', false, 32.0)
                .is_fallback
        );

        // The fixture primary font has a wider cell than the embedded font.
        let embedded_fonts = renderer.fonts_for(Some("embedded"));
        assert!(ascii_fonts.cell_width > embedded_fonts.cell_width);

        // Themes resolving to identical files share one chain, so adding
        // themes does not multiply font parsing.
        assert!(std::ptr::eq(
            renderer.fonts_for(Some("embedded-too")),
            embedded_fonts
        ));
        // An unknown theme falls back to the default theme's chain.
        assert!(std::ptr::eq(
            renderer.fonts_for(Some("nope")),
            embedded_fonts
        ));
        assert!(std::ptr::eq(renderer.fonts_for(None), embedded_fonts));
    }

    /// An explicit `--font` override wins over every theme's own font.
    #[test]
    fn font_override_applies_to_every_theme() {
        let mut themes = HashMap::new();
        themes.insert("embedded".to_string(), theme_config_with_fonts(None, &[]));
        themes.insert(
            "ascii-only".to_string(),
            theme_config_with_fonts(Some(LIMITED_PRIMARY), &[]),
        );

        let renderer = Renderer::new(
            &FontSelection {
                font_override: Some(PathBuf::from(LIMITED_PRIMARY)),
                ..FontSelection::default()
            },
            16.0,
            &themes,
            "embedded",
            &ChromeConfig::default(),
        )
        .expect("renderer builds");

        let overridden = renderer.fonts_for(Some("embedded"));
        let declared = renderer.fonts_for(Some("ascii-only"));
        assert_eq!(overridden.cell_width, declared.cell_width);
        assert!(std::ptr::eq(overridden, declared));
    }

    // ---------------------------------------------------------------------
    // Composed image descriptions
    // ---------------------------------------------------------------------

    /// Width trimming makes panes of different widths the normal case, so
    /// composing must pad a narrower pane with its own background rather than
    /// rescale it: every source pixel has to survive unchanged.
    #[test]
    fn compose_pads_narrow_panes_instead_of_rescaling_them() {
        let dir = std::path::Path::new("target/compose-padding-test");
        std::fs::create_dir_all(dir).unwrap();

        let wide_path = dir.join("wide.png");
        let narrow_path = dir.join("narrow.png");
        let wide: RgbaImage = ImageBuffer::from_pixel(40, 10, Rgba([10, 10, 10, 255]));
        let mut narrow: RgbaImage = ImageBuffer::from_pixel(20, 10, Rgba([80, 20, 20, 255]));
        // A single bright pixel: any resample would smear it into neighbours.
        narrow.put_pixel(5, 5, Rgba([255, 255, 0, 255]));
        save_png(&wide, &wide_path, None).unwrap();
        save_png(&narrow, &narrow_path, None).unwrap();

        let composed = compose_images(
            &[wide_path, narrow_path],
            ComposeLayout::Vertical,
            0,
            Rgba([0, 0, 0, 255]),
        )
        .expect("compose");

        assert_eq!(composed.dimensions(), (40, 20));
        assert_eq!(*composed.get_pixel(5, 15), Rgba([255, 255, 0, 255]));
        assert_eq!(*composed.get_pixel(6, 15), Rgba([80, 20, 20, 255]));
        // The pad to the right of the narrow pane uses that pane's own
        // background, so the seam is invisible.
        assert_eq!(*composed.get_pixel(30, 15), Rgba([80, 20, 20, 255]));
    }

    /// A composed image's description is the panes' descriptions, in order,
    /// with a clear separator and Unicode preserved.
    #[test]
    fn composed_description_joins_panes_and_keeps_unicode() {
        let dir = std::path::Path::new("target/composed-description-test");
        std::fs::create_dir_all(dir).unwrap();
        let img: RgbaImage = ImageBuffer::from_pixel(4, 4, Rgba([0, 0, 0, 255]));

        let first = dir.join("first.png");
        let second = dir.join("second.png");
        let bare = dir.join("bare.png");
        save_png(&img, &first, Some("héllo ✓ \u{4e2d}\u{6587}")).unwrap();
        save_png(&img, &second, Some("λ ─┐ second")).unwrap();
        save_png(&img, &bare, None).unwrap();

        assert_eq!(
            read_png_description(&first).as_deref(),
            Some("héllo ✓ \u{4e2d}\u{6587}")
        );
        assert_eq!(read_png_description(&bare), None);

        let joined = composed_description(&[first.clone(), second.clone()])
            .expect("panes have descriptions");
        assert_eq!(
            joined,
            "héllo ✓ \u{4e2d}\u{6587}\n\n--- Pane 2 ---\n\nλ ─┐ second"
        );

        // A pane without a description is marked, so pane numbers still line
        // up with the image.
        let mixed =
            composed_description(&[bare.clone(), second]).expect("one pane has a description");
        assert!(mixed.starts_with("(no description)"));
        assert!(mixed.contains("--- Pane 2 ---"));

        // No descriptions at all: nothing is invented.
        assert_eq!(composed_description(&[bare.clone(), bare]), None);
    }

    // ---------------------------------------------------------------------
    // Chrome geometry: rounded frames and macOS traffic lights
    // ---------------------------------------------------------------------

    /// Every preset that draws a window frame.
    const CHROME_PRESETS: [&str; 4] = ["minimal", "gnome", "macos", "report"];

    fn preset_chrome(preset: &str, rounded: bool, shadow: bool) -> ChromeOptions {
        ChromeOptions {
            enabled: true,
            preset: preset.to_string(),
            title: Some("demo".to_string()),
            timestamp: false,
            shadow,
            radius: 14,
            rounded,
            outer_padding: 18,
            title_bar_height: 34,
        }
    }

    /// Render a terminal image inside `preset`'s window frame.
    fn framed(preset: &str, rounded: bool, shadow: bool) -> RgbaImage {
        let renderer = bare_renderer();
        let theme = Theme::dark();
        // Wide enough that the centered title cannot overlap the traffic
        // lights on the left, so masks stay comparable.
        let terminal = ImageBuffer::from_pixel(400, 80, theme.background);
        renderer.compose_with_chrome(
            terminal,
            &theme,
            &renderer.default_fonts,
            &preset_chrome(preset, rounded, shadow),
        )
    }

    /// Signed distance from a pixel center to a rounded rectangle: negative
    /// inside, positive outside.
    fn rounded_rect_distance(x: u32, y: u32, w: u32, h: u32, r: f32) -> f32 {
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        let (hw, hh) = (w as f32 / 2.0, h as f32 / 2.0);
        let qx = (px - hw).abs() - (hw - r);
        let qy = (py - hh).abs() - (hh - r);
        (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
    }

    /// Rounded corners are a property of the frame, not of one preset: every
    /// chrome preset must leave its four outer corner pixels fully transparent.
    #[test]
    fn every_chrome_preset_has_transparent_outer_corners() {
        for preset in CHROME_PRESETS {
            let img = framed(preset, true, false);
            let (w, h) = img.dimensions();
            for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
                assert_eq!(
                    img.get_pixel(x, y)[3],
                    0,
                    "{preset}: corner ({x}, {y}) is not transparent"
                );
            }
            // The corner is anti-aliased, not a hard staircase: the frame edge
            // produces partially transparent pixels.
            let partial = img.pixels().filter(|p| p[3] > 0 && p[3] < 255).count();
            assert!(
                partial > 0,
                "{preset}: rounded corners have no anti-aliased pixels"
            );
        }
    }

    /// Nothing opaque may exist outside the rounded frame - no gray, black, or
    /// theme-colored halo around the window - for any preset.
    #[test]
    fn no_opaque_pixels_outside_the_rounded_frame() {
        let radius = (14 * RENDER_SCALE) as f32;
        for preset in CHROME_PRESETS {
            let img = framed(preset, true, false);
            let (w, h) = img.dimensions();
            for y in 0..h {
                for x in 0..w {
                    let alpha = img.get_pixel(x, y)[3];
                    // Half a pixel of slack for the anti-aliased edge itself.
                    if rounded_rect_distance(x, y, w, h, radius) > 0.75 {
                        assert_eq!(
                            alpha, 0,
                            "{preset}: pixel ({x}, {y}) outside the frame is not transparent"
                        );
                    }
                }
            }
        }
    }

    /// With a shadow the area outside the frame may hold shadow pixels, but
    /// they must stay semi-transparent - never an opaque backdrop.
    #[test]
    fn shadow_outside_the_frame_is_never_opaque() {
        let radius = (14 * RENDER_SCALE) as f32;
        for preset in CHROME_PRESETS {
            let img = framed(preset, true, true);
            let (w, h) = img.dimensions();
            // `compose_with_chrome` reserves `16 * RENDER_SCALE` pixels for the
            // shadow and centers the frame in them.
            let offset = 16 * RENDER_SCALE / 2;
            let (frame_w, frame_h) = (w - offset * 2, h - offset * 2);
            let mut shadow_pixels = 0usize;
            for y in 0..h {
                for x in 0..w {
                    let alpha = img.get_pixel(x, y)[3];
                    if alpha == 0 {
                        continue;
                    }
                    let (fx, fy) = (x as i64 - offset as i64, y as i64 - offset as i64);
                    let outside = fx < 0
                        || fy < 0
                        || fx >= frame_w as i64
                        || fy >= frame_h as i64
                        || rounded_rect_distance(fx as u32, fy as u32, frame_w, frame_h, radius)
                            > 0.75;
                    if outside {
                        shadow_pixels += 1;
                        assert!(
                            alpha < 255,
                            "{preset}: opaque pixel ({x}, {y}) outside the frame"
                        );
                    }
                }
            }
            assert!(shadow_pixels > 0, "{preset}: shadow was not drawn");
        }
    }

    /// The window body is the terminal's own background for every preset, so a
    /// screenshot is never surrounded by a mismatched gray border.
    #[test]
    fn frame_padding_matches_the_terminal_background() {
        let background = Theme::dark().background;
        for preset in CHROME_PRESETS {
            let img = framed(preset, true, false);
            let (w, h) = img.dimensions();
            // Left padding (below any title bar) and bottom padding, both well
            // inside the rounded corners.
            for (x, y) in [(4, h / 2), (w / 2, h - 4)] {
                assert_eq!(
                    *img.get_pixel(x, y),
                    background,
                    "{preset}: padding at ({x}, {y}) is not the terminal background"
                );
            }
        }
    }

    /// `--no-rounded` must still produce a square, fully opaque frame: the
    /// corner mask is applied only when rounding is on.
    #[test]
    fn square_chrome_keeps_opaque_corners() {
        for preset in CHROME_PRESETS {
            let img = framed(preset, false, false);
            let (w, h) = img.dimensions();
            for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
                assert_eq!(
                    img.get_pixel(x, y)[3],
                    255,
                    "{preset}: square corner ({x}, {y}) lost its opacity"
                );
            }
        }
    }

    /// Shape statistics of the pixels exactly matching one color.
    #[derive(Debug, PartialEq, Eq)]
    struct MaskStats {
        width: u32,
        height: u32,
        count: usize,
        left: usize,
        right: usize,
        top: usize,
        bottom: usize,
        origin: (u32, u32),
    }

    /// Bounding box, pixel count, and left/right + top/bottom symmetry of the
    /// pixels exactly matching `color`.
    fn mask_stats(img: &RgbaImage, color: Rgba<u8>) -> MaskStats {
        let points: Vec<(u32, u32)> = img
            .enumerate_pixels()
            .filter(|(_, _, p)| **p == color)
            .map(|(x, y, _)| (x, y))
            .collect();
        assert!(!points.is_empty(), "no pixels of {:?} were drawn", color);
        let (x0, x1) = (
            points.iter().map(|p| p.0).min().unwrap(),
            points.iter().map(|p| p.0).max().unwrap(),
        );
        let (y0, y1) = (
            points.iter().map(|p| p.1).min().unwrap(),
            points.iter().map(|p| p.1).max().unwrap(),
        );
        let cx = (x0 + x1) as f32 / 2.0;
        let cy = (y0 + y1) as f32 / 2.0;
        MaskStats {
            width: x1 - x0 + 1,
            height: y1 - y0 + 1,
            count: points.len(),
            left: points.iter().filter(|p| (p.0 as f32) < cx).count(),
            right: points.iter().filter(|p| (p.0 as f32) > cx).count(),
            top: points.iter().filter(|p| (p.1 as f32) < cy).count(),
            bottom: points.iter().filter(|p| (p.1 as f32) > cy).count(),
            origin: (x0, y0),
        }
    }

    /// The macOS traffic lights must be three identical circles: equal
    /// diameters, square (and therefore circular) masks, mirror symmetry about
    /// both axes, and evenly spaced centers.
    #[test]
    fn macos_traffic_lights_are_equal_symmetric_circles() {
        let img = framed("macos", true, false);
        let colors = [
            Rgba([255, 95, 86, 255]),
            Rgba([255, 189, 46, 255]),
            Rgba([39, 201, 63, 255]),
        ];

        let stats: Vec<MaskStats> = colors.iter().map(|c| mask_stats(&img, *c)).collect();
        for (stat, color) in stats.iter().zip(colors) {
            assert_eq!(
                stat.width, stat.height,
                "{color:?} button is not square (so not round)"
            );
            assert_eq!(
                stat.left, stat.right,
                "{color:?} button is not left/right symmetric"
            );
            assert_eq!(
                stat.top, stat.bottom,
                "{color:?} button is not top/bottom symmetric"
            );
        }

        // All three buttons are the same disc.
        for other in &stats[1..] {
            assert_eq!(
                (other.width, other.height, other.count),
                (stats[0].width, stats[0].height, stats[0].count),
                "traffic lights differ in size"
            );
        }

        // Evenly spaced, on one horizontal line.
        let pitch = (TRAFFIC_LIGHT_PITCH * RENDER_SCALE) as i64;
        for (index, stat) in stats.iter().enumerate() {
            assert_eq!(
                stat.origin.1, stats[0].origin.1,
                "traffic lights are not on the same line"
            );
            assert_eq!(
                stat.origin.0 as i64 - stats[0].origin.0 as i64,
                pitch * index as i64,
                "traffic light {index} is misplaced"
            );
        }

        // A rounded square would fill its bounding box; a disc leaves the
        // corners of the box empty.
        let (x0, y0) = stats[0].origin;
        let side = stats[0].width - 1;
        for (dx, dy) in [(0, 0), (side, 0), (0, side), (side, side)] {
            assert_ne!(
                *img.get_pixel(x0 + dx, y0 + dy),
                colors[0],
                "the button fills its bounding box corner, so it is not a circle"
            );
        }
    }

    /// The circle helper itself: a true disc, symmetric about its center, with
    /// a one-pixel anti-aliased edge.
    #[test]
    fn draw_circle_is_a_symmetric_antialiased_disc() {
        let renderer = bare_renderer();
        let mut img: RgbaImage = ImageBuffer::from_pixel(41, 41, Rgba([0, 0, 0, 0]));
        // An even diameter centered on a pixel boundary is exactly symmetric.
        let (cx, cy, r) = (20.0f32, 20.0f32, 10.0f32);
        renderer.draw_circle(&mut img, cx, cy, r, Rgba([255, 0, 0, 255]));

        let opaque: Vec<(u32, u32)> = img
            .enumerate_pixels()
            .filter(|(_, _, p)| p[3] == 255)
            .map(|(x, y, _)| (x, y))
            .collect();
        let xs: Vec<u32> = opaque.iter().map(|p| p.0).collect();
        let ys: Vec<u32> = opaque.iter().map(|p| p.1).collect();
        let (x0, x1) = (*xs.iter().min().unwrap(), *xs.iter().max().unwrap());
        let (y0, y1) = (*ys.iter().min().unwrap(), *ys.iter().max().unwrap());
        assert_eq!(x1 - x0, y1 - y0, "the disc is not as wide as it is tall");

        // Mirror symmetry about both axes, pixel for pixel.
        for (x, y) in &opaque {
            let mirrored_x = (2.0 * cx - 1.0) as u32 - x;
            let mirrored_y = (2.0 * cy - 1.0) as u32 - y;
            assert_eq!(
                img.get_pixel(mirrored_x, *y)[3],
                255,
                "no horizontal mirror for ({x}, {y})"
            );
            assert_eq!(
                img.get_pixel(*x, mirrored_y)[3],
                255,
                "no vertical mirror for ({x}, {y})"
            );
        }

        // Round, not square: the bounding-box corners stay empty, and a point
        // just outside the radius is never lit.
        assert_eq!(img.get_pixel(x0, y0)[3], 0);
        assert_eq!(img.get_pixel(x1, y1)[3], 0);
        for (x, y) in [(cx + r + 1.0, cy), (cx, cy + r + 1.0)] {
            assert_eq!(
                img.get_pixel(x as u32, y as u32)[3],
                0,
                "ink outside the circle radius"
            );
        }

        // The edge is anti-aliased rather than a hard staircase.
        let partial = img.pixels().filter(|p| p[3] > 0 && p[3] < 255).count();
        assert!(partial > 0, "circle edge is not anti-aliased");
    }

    /// An interactive capture's text comes from the screen: colors survive,
    /// but the terminal bookkeeping in the raw stream (bracketed paste, window
    /// titles, cursor motion, and the erased trailing prompt) does not.
    #[test]
    fn screen_text_keeps_colors_and_drops_terminal_bookkeeping() {
        let captured = screen_of(
            b"\x1b[?2004h$ echo hi\r\n\x1b[?2004l\r\x1b[1;36mhi\x1b[0m\r\n$ \r\x1b[0m\x1b[J",
            6,
            40,
        );
        let text = screen_ansi_text(&captured, 40);

        assert!(
            text.contains("$ echo hi"),
            "command echo missing: {:?}",
            text
        );
        assert!(text.contains("hi"), "output missing: {:?}", text);
        assert!(text.contains('\u{1b}'), "colors were lost: {:?}", text);
        assert!(
            !text.contains("?2004"),
            "bracketed paste leaked: {:?}",
            text
        );
        assert_eq!(
            text.lines().count(),
            2,
            "the erased trailing prompt should be gone: {:?}",
            text
        );
    }

    /// The cyan attribute of a colored cell is re-emitted verbatim, and the
    /// line is closed with a reset so the style cannot bleed.
    #[test]
    fn screen_text_reemits_the_cells_own_sgr() {
        let captured = screen_of(b"\x1b[1;36mMCP\x1b[0m ok\r\n", 3, 20);
        let text = screen_ansi_text(&captured, 20);
        assert!(
            text.starts_with("\x1b[0;1;36mMCP"),
            "unexpected text: {:?}",
            text
        );
        assert!(text.ends_with("\x1b[0m"), "line not reset: {:?}", text);
    }

    /// Screen-derived text is one line per *screen row*: a soft-wrapped line
    /// becomes two, matching the image the reader is looking at.
    #[test]
    fn screen_text_is_one_line_per_row() {
        let captured = screen_of(b"abcdefghijklmno\r\n", 4, 10);
        let text = screen_ansi_text(&captured, 10);
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("abcdefghij"));
        assert!(text.contains("klmno"));
    }

    /// A screenshot shows every retained line, not the last screenful: the
    /// image is as tall as the whole capture and its `Description` metadata
    /// carries the first line as well as the last.
    #[test]
    fn render_bytes_renders_every_line_that_scrolled_off() {
        let mut renderer = bare_renderer();
        renderer.themes.insert("dark".to_string(), Theme::dark());
        let out_dir = std::path::Path::new("target/full-capture-out");
        std::fs::create_dir_all(out_dir).unwrap();

        let data: String = (1..=200).map(|i| format!("line {}\r\n", i)).collect();
        let (path, text, _, context) = renderer
            .render_bytes_with_options(
                data.as_bytes(),
                40,
                10,
                out_dir,
                Some("full capture"),
                Some("dark"),
                None,
                None,
                TextOptions {
                    strip_ansi: true,
                    embed_description: true,
                    ..TextOptions::default()
                },
                true,
                RenderOptions::default(),
            )
            .unwrap();

        assert!(!context.truncated);
        assert!(
            text.starts_with("line 1\n"),
            "text starts: {:?}",
            &text[..20]
        );
        assert!(text.trim_end().ends_with("line 200"));

        let img = image::open(&path).unwrap().to_rgba8();
        let cell_h = renderer.default_fonts.cell_height * RENDER_SCALE;
        let padding = renderer.padding * RENDER_SCALE;
        // 200 printed lines plus the row the cursor rests on.
        assert_eq!(img.height(), 201 * cell_h + padding * 2);

        let description = read_png_description(&path).expect("description");
        assert!(description.starts_with("line 1\n"));
        assert!(description.ends_with("line 200"));

        std::fs::remove_file(&path).ok();
    }

    /// Colors set on a line that has long since scrolled out of the viewport
    /// are still the colors that line is drawn and re-emitted with.
    #[test]
    fn styling_survives_from_the_oldest_scrollback_rows() {
        let renderer = bare_renderer();
        let mut data = String::from("\x1b[1;31mALERT\x1b[0m first\r\n");
        for i in 2..=100 {
            data.push_str(&format!("line {}\r\n", i));
        }
        let captured = renderer.capture(data.as_bytes(), 10, 40, LineSelection::All);

        let cell = captured.cell(0, 0).expect("first cell");
        assert_eq!(cell.contents(), "A");
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
        assert!(cell.bold());

        let text = screen_ansi_text(&captured, 40);
        assert!(
            text.starts_with("\x1b[0;1;31mALERT"),
            "scrollback style lost: {:?}",
            &text[..24]
        );
    }

    /// A selection narrows the image to the lines it kept, and the returned
    /// text follows it rather than reporting output the picture does not show.
    #[test]
    fn head_and_tail_selections_narrow_the_render() {
        let mut renderer = bare_renderer();
        renderer.themes.insert("dark".to_string(), Theme::dark());
        let out_dir = std::path::Path::new("target/selection-out");
        std::fs::create_dir_all(out_dir).unwrap();

        let data: String = (1..=200).map(|i| format!("line {}\r\n", i)).collect();
        let render = |lines| {
            renderer
                .render_bytes_with_options(
                    data.as_bytes(),
                    40,
                    10,
                    out_dir,
                    Some("selection"),
                    Some("dark"),
                    None,
                    None,
                    TextOptions {
                        strip_ansi: true,
                        ..TextOptions::default()
                    },
                    true,
                    RenderOptions { lines },
                )
                .unwrap()
        };

        let (head_path, head_text, _, head_context) = render(LineSelection::Head(10));
        assert_eq!(head_context.lines, LineSelection::Head(10));
        assert_eq!(head_text.lines().count(), 10);
        assert_eq!(head_text.lines().next().unwrap(), "line 1");
        assert_eq!(head_text.lines().last().unwrap(), "line 10");

        let (tail_path, tail_text, _, tail_context) = render(LineSelection::Tail(10));
        assert_eq!(tail_context.lines, LineSelection::Tail(10));
        assert_eq!(tail_text.lines().count(), 10);
        assert_eq!(tail_text.lines().next().unwrap(), "line 191");
        assert_eq!(tail_text.lines().last().unwrap(), "line 200");

        let cell_h = renderer.default_fonts.cell_height * RENDER_SCALE;
        let padding = renderer.padding * RENDER_SCALE;
        for path in [&head_path, &tail_path] {
            let img = image::open(path).unwrap().to_rgba8();
            assert_eq!(img.height(), 10 * cell_h + padding * 2);
            std::fs::remove_file(path).ok();
        }
    }

    /// A capture too tall to render is refused with an actionable error rather
    /// than an allocation the machine cannot satisfy.
    #[test]
    fn an_oversized_capture_is_refused_with_advice() {
        let mut renderer = bare_renderer();
        renderer.themes.insert("dark".to_string(), Theme::dark());
        let out_dir = std::path::Path::new("target/oversized-out");
        std::fs::create_dir_all(out_dir).unwrap();

        // A full 10k-line scrollback, 200 columns wide, is well past the
        // pixel ceiling.
        let data: String = (1..=12_000).map(|i| format!("line {}\r\n", i)).collect();
        let err = renderer
            .render_bytes(
                data.as_bytes(),
                200,
                10,
                out_dir,
                Some("oversized"),
                Some("dark"),
                None,
                None,
                TextOptions::default(),
                false,
            )
            .expect_err("an image this size must be refused");
        let message = err.to_string();
        assert!(
            message.contains("--head-lines") && message.contains("megapixels"),
            "unhelpful error: {message}"
        );
    }

    /// The budget is a *memory* budget, not just a pixel count: adding chrome
    /// keeps a second full-size layer alive, so the same capture that renders
    /// bare can be over the line once it is framed and shadowed.
    #[test]
    fn the_render_budget_counts_every_simultaneous_buffer() {
        let screen = screen_of(b"hi", 4, 80);

        // One buffer at the pixel ceiling is allowed.
        let side = (MAX_IMAGE_PIXELS as f64).sqrt() as u32;
        assert!(check_render_budget(side, side, 1, &screen).is_ok());
        // The same image with a second layer alive is over the byte budget.
        let err = check_render_budget(side, side, 2, &screen)
            .expect_err("two buffers of a 64 MP image exceed the budget");
        assert!(
            err.to_string().contains("MB of image buffers"),
            "the error should name the memory cost: {err}"
        );
        // And one pixel too many is refused outright.
        assert!(check_render_budget(side + 1, side + 1, 1, &screen).is_err());
    }

    /// A capture the machine cannot render is refused *before* anything is
    /// allocated, whether the size comes from the number of rows or from the
    /// width of the terminal.
    #[test]
    fn huge_geometry_is_refused_before_allocation() {
        let mut renderer = bare_renderer();
        renderer.themes.insert("dark".to_string(), Theme::dark());
        let out_dir = std::path::Path::new("target/huge-geometry-out");
        std::fs::create_dir_all(out_dir).unwrap();

        let render = |cols: u16, rows: u16, data: &str, chrome: Option<&ChromeOptions>| {
            renderer.render_bytes(
                data.as_bytes(),
                cols,
                rows,
                out_dir,
                Some("huge"),
                Some("dark"),
                chrome,
                None,
                TextOptions::default(),
                false,
            )
        };

        // Too many rows: a full scrollback of a narrow terminal.
        let tall: String = (1..=12_000).map(|i| format!("line {}\r\n", i)).collect();
        let err = render(200, 10, &tall, None).expect_err("a 12,000-line image must be refused");
        assert!(err.to_string().contains("--head-lines"), "{err}");

        // Too many columns: the widest terminal the CLI accepts, filled.
        let wide = format!("{}\r\n", "x".repeat(500)).repeat(2_000);
        let err = render(500, 10, &wide, None).expect_err("a 500-column wall must be refused");
        assert!(err.to_string().contains("--head-lines"), "{err}");

        // Selecting one end of the same capture renders fine, which is what
        // the error tells the caller to do.
        let (path, _, _, _) = renderer
            .render_bytes_with_options(
                tall.as_bytes(),
                200,
                10,
                out_dir,
                Some("huge-head"),
                Some("dark"),
                None,
                None,
                TextOptions::default(),
                false,
                RenderOptions::default().with_lines(LineSelection::Head(20)),
            )
            .expect("a twenty-line selection is renderable");
        std::fs::remove_file(&path).ok();
    }

    /// Head selection is the first lines of the *output*, even when far more
    /// output was produced than the scrollback could hold - which is what makes
    /// it usable as an escape hatch from the render budget.
    #[test]
    fn head_selection_survives_a_capture_larger_than_the_scrollback() {
        let mut renderer = bare_renderer();
        renderer.max_scrollback_lines = 100;
        renderer.themes.insert("dark".to_string(), Theme::dark());
        let out_dir = std::path::Path::new("target/head-overflow-out");
        std::fs::create_dir_all(out_dir).unwrap();

        let data: String = (1..=20_000).map(|i| format!("line {}\r\n", i)).collect();
        let (path, text, _, context) = renderer
            .render_bytes_with_options(
                data.as_bytes(),
                80,
                24,
                out_dir,
                Some("head-overflow"),
                Some("dark"),
                None,
                None,
                TextOptions {
                    strip_ansi: true,
                    ..TextOptions::default()
                },
                true,
                RenderOptions::default().with_lines(LineSelection::Head(10)),
            )
            .unwrap();

        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "line 1");
        assert_eq!(lines[9], "line 10");
        assert!(
            !context.truncated,
            "the head is complete, so nothing it shows was dropped"
        );
        std::fs::remove_file(&path).ok();
    }

    /// The configured scrollback is a *tail*-retention setting, so a head
    /// render must not be at its mercy: a one-line capacity still answers with
    /// the first ten lines of a thousand-line run.
    #[test]
    fn head_selection_ignores_the_configured_scrollback_capacity() {
        let mut renderer = bare_renderer();
        renderer.max_scrollback_lines = 1;
        renderer.themes.insert("dark".to_string(), Theme::dark());
        let out_dir = std::path::Path::new("target/head-tiny-capacity-out");
        std::fs::create_dir_all(out_dir).unwrap();

        let data: String = (1..=1_000).map(|i| format!("line {}\r\n", i)).collect();
        let (path, text, _, context) = renderer
            .render_bytes_with_options(
                data.as_bytes(),
                80,
                40,
                out_dir,
                Some("head-tiny-capacity"),
                Some("dark"),
                None,
                None,
                TextOptions {
                    strip_ansi: true,
                    ..TextOptions::default()
                },
                true,
                RenderOptions::default().with_lines(LineSelection::Head(10)),
            )
            .unwrap();

        let lines: Vec<&str> = text.lines().collect();
        let expected: Vec<String> = (1..=10).map(|i| format!("line {}", i)).collect();
        assert_eq!(lines, expected);
        assert!(!context.truncated);
        std::fs::remove_file(&path).ok();
    }

    /// A capture that lost output says so, and one that filled its scrollback
    /// to the last row does not.
    #[test]
    fn render_meta_reports_truncation_exactly() {
        let mut renderer = bare_renderer();
        renderer.max_scrollback_lines = 10;
        renderer.themes.insert("dark".to_string(), Theme::dark());
        let out_dir = std::path::Path::new("target/truncation-meta-out");
        std::fs::create_dir_all(out_dir).unwrap();

        let render = |lines: usize| {
            let data: String = (1..=lines).map(|i| format!("line {}\r\n", i)).collect();
            renderer
                .render_bytes_with_options(
                    data.as_bytes(),
                    40,
                    5,
                    out_dir,
                    Some("truncation"),
                    Some("dark"),
                    None,
                    None,
                    TextOptions::default(),
                    true,
                    RenderOptions::default(),
                )
                .unwrap()
        };

        // 14 lines through a 5-row viewport scroll exactly 10 rows off.
        let (path, _, _, context) = render(14);
        assert!(!context.truncated);
        std::fs::remove_file(&path).ok();

        let (path, _, _, context) = render(15);
        assert!(context.truncated);
        std::fs::remove_file(&path).ok();
    }

    /// The renderer reports the capacity that actually applied, which the
    /// retained-cell budget can push below the configured one.
    #[test]
    fn effective_scrollback_follows_the_terminal_width() {
        let renderer = bare_renderer();
        assert_eq!(
            renderer.effective_scrollback_lines(40, 120),
            DEFAULT_MAX_SCROLLBACK_LINES
        );
        assert!(renderer.effective_scrollback_lines(40, 500) < DEFAULT_MAX_SCROLLBACK_LINES);
    }

    /// Drawing the terminal straight into the window frame saves a full-size
    /// buffer; it must not change a single pixel of the result. Both presets
    /// and both shadow settings are checked against the copy-then-compose path
    /// they replaced.
    #[test]
    fn chrome_composition_is_pixel_identical_with_one_fewer_buffer() {
        let renderer = bare_renderer();
        let theme = Theme::dark();
        let captured = screen_of(b"\x1b[1;32mok\x1b[0m composed", 4, 40);

        for (preset, shadow, rounded) in [
            ("gnome", true, true),
            ("macos", false, true),
            ("minimal", true, false),
            ("report", false, false),
        ] {
            let chrome = ChromeOptions {
                enabled: true,
                preset: preset.to_string(),
                title: Some("demo".to_string()),
                // A timestamp would differ between the two renders for reasons
                // that have nothing to do with buffering.
                timestamp: false,
                shadow,
                radius: 14,
                rounded,
                outer_padding: 18,
                title_bar_height: 34,
            };

            let one_layer = renderer
                .render_to_image(
                    &captured,
                    &theme,
                    &renderer.default_fonts,
                    &chrome,
                    None,
                    true,
                )
                .unwrap();

            let terminal = renderer
                .render_screen(&captured, &theme, &renderer.default_fonts, None, true)
                .unwrap();
            let copied =
                renderer.compose_with_chrome(terminal, &theme, &renderer.default_fonts, &chrome);

            assert_eq!(
                one_layer.dimensions(),
                copied.dimensions(),
                "{preset} changed size"
            );
            assert!(
                one_layer.as_raw() == copied.as_raw(),
                "{preset} (shadow={shadow}) is no longer pixel identical"
            );
        }
    }
}
