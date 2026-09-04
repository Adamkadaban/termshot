//! Shaped text rendering for the terminal grid.
//!
//! The renderer's original path rasterizes one Unicode scalar at a time with
//! [`fontdue`], which is exactly right for the overwhelming majority of
//! terminal output and is what every existing screenshot was produced with.
//! It cannot, however, place a combining mark, join an Arabic word, reorder a
//! Devanagari cluster, or draw a color emoji.
//!
//! This module adds a second path for precisely those cells. It is built on
//! `cosmic-text` (pure Rust throughout: `fontdb` for discovery, `harfrust` for
//! shaping, `swash` for rasterization including color glyphs), so the static
//! musl release keeps building with no Pango, Cairo, or `FreeType` anywhere
//! near it.
//!
//! # What this module is not allowed to do
//!
//! The terminal grid stays authoritative. Nothing here decides where a line
//! wraps, how tall a row is, or how wide a cell is: the renderer hands over a
//! run of cells vt100 already laid out, together with their pixel geometry,
//! and gets back glyph bitmaps positioned *inside that run*. The renderer
//! clips every one of them to the run's own rectangle, so a shaped cluster can
//! never paint into the cell after the run - which is what keeps redaction
//! blocks and style boundaries honest.
//!
//! # Font precedence
//!
//! Explicit fonts always win. A run is shaped with the first font that covers
//! it out of, in order: the theme's (or CLI's) primary face, its bold face,
//! the embedded JetBrains Mono, and then each configured fallback font in the
//! order it was listed. Only when none of them covers the run does automatic
//! system font fallback get a say, and that is `cosmic-text`'s platform
//! fallback list running over the fonts `fontdb` found. Font collections
//! (`.ttc`) are loaded face by face, so a fallback that names a collection
//! keeps the face index `fontdb` discovered instead of silently collapsing to
//! face 0.
//!
//! Clusters that ask for emoji presentation are shaped with a color emoji font
//! when one can be found, preferring an explicitly configured one.
//!
//! # Projecting shaped text back onto the grid
//!
//! Two modes, chosen by the run's script:
//!
//! * **Per cluster.** The default. Glyphs are grouped by the cells they
//!   actually came from - a ligature or an emoji sequence merges the cells it
//!   spans - and each group is fitted to the columns *it* needs, anchored at
//!   the start of its span. That is what keeps a two-cell emoji in its two
//!   columns even when vt100 handed the run six.
//! * **Contiguous.** For joining and reordering scripts (Arabic, Devanagari,
//!   and friends), where centering each letter in its own cell would break
//!   every connecting stroke and strand every reordered mark. The whole run is
//!   laid out as one piece and fitted to the columns it needs, anchored at the
//!   start of the run - the end, for a right-to-left one. A long word
//!   therefore does not sit one code point per column; it sits inside its own
//!   columns, which is the guarantee that matters.
//!
//! Either way the fit only ever scales within [`MIN_FIT_SCALE`] and
//! [`MAX_FIT_SCALE`], the same clamp the `fontdue` fallback path uses, and the
//! renderer clips the result to the run's rectangle.

use crate::unicode;
use cosmic_text::fontdb;
use cosmic_text::{
    Attrs, Buffer, CacheKey, CacheKeyFlags, Family, FontSystem, LayoutGlyph, Metrics, Shaping,
    SwashCache, SwashContent, Weight, Wrap,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Largest number of cells shaped as one run.
///
/// A capture's rows are as wide as the user asked for, so a run is chopped
/// into pieces this long before it reaches the shaper. The break is invisible
/// in practice - no script needs a 256-cell context - and it bounds both the
/// shaping work and the size of a shape-cache key.
const MAX_RUN_CELLS: usize = 256;

/// Largest contiguous joining/reordering run shaped as one piece.
///
/// The CLI and MCP interfaces cap terminal width at 500 columns. Keeping one
/// such row intact is necessary for paragraph-level RTL word order; splitting
/// it into independently right-aligned chunks would reverse the chunks. The
/// separate ordinary-run limit above remains smaller.
const MAX_CONTIGUOUS_RUN_CELLS: usize = 512;

fn run_chunk_limit(cells: &[RunCell]) -> usize {
    if cells
        .iter()
        .any(|cell| unicode::cluster_forces_contiguous_run(&cell.text))
    {
        MAX_CONTIGUOUS_RUN_CELLS
    } else {
        MAX_RUN_CELLS
    }
}

/// Largest glyph bitmap, in pixels, that will be kept and drawn. A broken or
/// hostile font can report an enormous bitmap for one glyph; past this it is
/// dropped rather than allocated.
const MAX_GLYPH_PIXELS: usize = 1 << 20;

/// Bytes of rasterized glyphs held before the glyph cache is dropped. Color
/// bitmaps are four bytes a pixel, so this is a few thousand emoji.
const MAX_GLYPH_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Entries held in the glyph cache, including glyphs that could not be
/// rasterized. Failed glyphs carry no bitmap bytes, so the byte ceiling alone
/// would not bound an adversarial stream of distinct missing glyphs.
const MAX_GLYPH_CACHE_ENTRIES: usize = 32 * 1024;

fn glyph_cache_full(entries: usize, bytes: usize) -> bool {
    entries >= MAX_GLYPH_CACHE_ENTRIES || bytes >= MAX_GLYPH_CACHE_BYTES
}

/// Shaped runs held before the shape cache is dropped.
const MAX_SHAPE_CACHE_ENTRIES: usize = 4096;

/// Faces loaded from explicitly configured font files. A font collection can
/// carry dozens of faces and a user can list many fallbacks; past this the
/// remaining faces are ignored with a warning.
const MAX_EXPLICIT_FACES: usize = 64;

fn explicit_faces_fit(current: usize, additional: usize) -> bool {
    current
        .checked_add(additional)
        .is_some_and(|total| total <= MAX_EXPLICIT_FACES)
}

/// Smallest and largest factor a shaped cluster may be scaled by to fit the
/// cells vt100 gave it. Mirrors the clamp the `fontdue` fallback path uses, so
/// an unusual face cannot blow a glyph up or shrink it away.
const MIN_FIT_SCALE: f32 = 0.5;
/// See [`MIN_FIT_SCALE`].
const MAX_FIT_SCALE: f32 = 1.5;

/// Color emoji families tried, in order, before falling back to "any family
/// whose name mentions emoji". Only families that really carry a color table
/// are considered, so a monochrome font cannot win on its name alone.
const KNOWN_EMOJI_FAMILIES: &[&str] = &[
    "Noto Color Emoji",
    "Apple Color Emoji",
    "Segoe UI Emoji",
    "Twemoji Mozilla",
    "Twitter Color Emoji",
    "JoyPixels",
    "OpenMoji Color",
    "EmojiOne Color",
];

/// How the shaped path is set up for a renderer.
///
/// A new type rather than a field on [`crate::renderer::FontSelection`] or
/// [`crate::renderer::RendererOptions`], both of which are published structs
/// that external code builds with exhaustive literals. It is
/// `#[non_exhaustive]` so it can keep growing without ever repeating that
/// mistake: build one with [`ShapingOptions::default`] and adjust it through
/// the builder methods.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ShapingOptions {
    /// Master switch. With shaping off the renderer behaves exactly like
    /// 1.1.5: every cell goes through the `fontdue` path, and characters no
    /// configured font covers are drawn the way they always were.
    pub enabled: bool,
    /// Whether fonts installed on the machine may be used for automatic
    /// fallback, after every explicitly configured font has been tried.
    ///
    /// Turning this off makes rendering depend only on the fonts the
    /// configuration names, which is what reproducible pipelines - and the
    /// deterministic tests - want.
    pub system_fonts: bool,
}

impl Default for ShapingOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            system_fonts: true,
        }
    }
}

impl ShapingOptions {
    /// Options that use only explicitly configured fonts: no system font
    /// discovery, so the same input renders the same way on any machine.
    pub fn deterministic() -> Self {
        Self {
            enabled: true,
            system_fonts: false,
        }
    }

    /// Options that disable the shaped path entirely.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            system_fonts: false,
        }
    }

    /// Enable or disable the shaped path.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Enable or disable automatic system font fallback.
    pub fn with_system_fonts(mut self, system_fonts: bool) -> Self {
        self.system_fonts = system_fonts;
        self
    }

    /// Options taken from the environment.
    ///
    /// `TERMSHOT_UNICODE_SHAPING=0` turns the shaped path off;
    /// `TERMSHOT_SYSTEM_FONTS=0` keeps shaping but restricts it to the fonts
    /// the configuration names. Both accept `0`/`false`/`off`/`no`.
    pub fn from_env() -> Self {
        let mut options = Self::default();
        if env_flag("UNICODE_SHAPING") == Some(false) {
            options.enabled = false;
        }
        if env_flag("SYSTEM_FONTS") == Some(false) {
            options.system_fonts = false;
        }
        options
    }

    fn locale(&self) -> String {
        if self.system_fonts {
            sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string())
        } else {
            // Deterministic rendering cannot inherit locale-dependent Han
            // fallback choices from the machine running it.
            "en-US".to_string()
        }
    }
}

/// Read a boolean `TERMSHOT_<name>` override, honouring the deprecated
/// `SCREENSHOT_MCP_` prefix the rest of the crate still accepts.
fn env_flag(name: &str) -> Option<bool> {
    let current = format!("TERMSHOT_{name}");
    let legacy = format!("SCREENSHOT_MCP_{name}");
    let raw = match std::env::var(&current) {
        Ok(value) => value,
        Err(_) => match std::env::var(&legacy) {
            Ok(value) => {
                tracing::warn!("{legacy} is deprecated, use {current} instead");
                value
            }
            Err(_) => return None,
        },
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "0" | "false" | "off" | "no" => Some(false),
        "1" | "true" | "on" | "yes" => Some(true),
        _ => {
            tracing::warn!(
                "ignoring invalid {current} value {:?}; use 0/1, false/true, off/on, or no/yes",
                raw
            );
            None
        }
    }
}

/// One font the shaped path may use, in precedence order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum FontSource {
    /// The JetBrains Mono compiled into the binary.
    Embedded(&'static [u8]),
    /// A font file the configuration named. May be a `.ttc` collection, in
    /// which case every face in it is loaded with its own index.
    File(PathBuf),
}

/// A single cell handed to the shaper.
#[derive(Debug, Clone)]
pub(crate) struct RunCell {
    /// The cell's contents, exactly as vt100 stored them.
    pub text: String,
    /// How many grid columns the cell owns: two for a double-width character
    /// (the cell plus its vt100 continuation), one otherwise.
    pub cols: u32,
}

/// Everything the shaper needs to draw one run of cells.
#[derive(Debug, Clone)]
pub(crate) struct RunRequest<'a> {
    pub cells: &'a [RunCell],
    /// Pixel width of one grid column.
    pub cell_w: u32,
    /// Font size, in device pixels, the run is shaped at.
    pub font_size: f32,
    /// Baseline offset from the top of the row, taken from the *primary* face
    /// so shaped glyphs sit on the same baseline as `fontdue` ones.
    pub ascent: f32,
    /// Whether a real bold face should be asked for. The renderer passes
    /// `false` when it means to draw the run twice for faux bold, exactly as
    /// the `fontdue` path does when no bold face is configured.
    pub bold: bool,
    pub italic: bool,
}

/// A rasterized glyph ready to be blended into the image.
#[derive(Debug)]
pub(crate) struct GlyphBitmap {
    pub width: u32,
    pub height: u32,
    /// Straight (non-premultiplied) RGBA when the glyph came from a color
    /// font, otherwise coverage to be modulated by the cell's foreground.
    pub color: bool,
    pub data: Vec<u8>,
}

/// A glyph placed relative to the top-left corner of its run: `x` grows to the
/// right from the run's first column, `y` down from the top of the row.
#[derive(Debug, Clone)]
pub(crate) struct PlacedGlyph {
    pub x: i32,
    pub y: i32,
    pub bitmap: Arc<GlyphBitmap>,
}

/// Key for the shaped-run cache. Two cells with the same text and style shape
/// identically, so a screen full of repeated content shapes once.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShapeKey {
    text: String,
    size_bits: u32,
    bold: bool,
    italic: bool,
    emoji: bool,
}

/// The result of shaping one run: glyphs in run-local layout coordinates, plus
/// whether the shaper laid them out right-to-left.
#[derive(Debug)]
struct ShapedRun {
    glyphs: Vec<LayoutGlyph>,
    rtl: bool,
}

/// The lazily built, lock-protected half of the engine.
struct ShapingState {
    font_system: FontSystem,
    swash: SwashCache,
    /// Explicit font families in precedence order, deduplicated.
    families: Vec<String>,
    /// Family of the preferred color emoji font, if one was found.
    emoji_family: Option<String>,
    shape_cache: HashMap<ShapeKey, Arc<ShapedRun>>,
    glyph_cache: HashMap<CacheKey, Option<Arc<RasterGlyph>>>,
    glyph_bytes: usize,
}

/// A rasterized glyph plus the offsets swash reported for it.
#[derive(Debug)]
struct RasterGlyph {
    bitmap: Arc<GlyphBitmap>,
    /// Left side bearing of the bitmap relative to the pen position.
    left: i32,
    /// Distance from the baseline up to the top row of the bitmap.
    top: i32,
}

/// The shaped glyph source for one font chain.
///
/// Construction is deliberately cheap: nothing is parsed and no system font is
/// looked at until a cell actually needs shaping, so a renderer that only ever
/// draws ASCII pays nothing at all. Everything mutable lives behind one
/// [`Mutex`] that is held for shaping and rasterization only - never across a
/// whole image - so the renderer's `&self` methods stay shareable behind an
/// `Arc`.
pub(crate) struct ShapingEngine {
    options: ShapingOptions,
    sources: Vec<FontSource>,
    state: OnceLock<Option<Mutex<ShapingState>>>,
}

impl std::fmt::Debug for ShapingEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShapingEngine")
            .field("options", &self.options)
            .field("sources", &self.sources)
            .field("initialized", &self.state.get().is_some())
            .finish()
    }
}

impl ShapingEngine {
    /// A shaping engine over `sources`, listed in precedence order.
    pub(crate) fn new(sources: Vec<FontSource>, options: ShapingOptions) -> Self {
        Self {
            options,
            sources,
            state: OnceLock::new(),
        }
    }

    /// Whether the shaped path may be used at all.
    pub(crate) fn enabled(&self) -> bool {
        self.options.enabled
    }

    fn state(&self) -> Option<&Mutex<ShapingState>> {
        self.state
            .get_or_init(|| {
                if !self.options.enabled {
                    return None;
                }
                Some(Mutex::new(ShapingState::build(
                    &self.sources,
                    &self.options,
                )))
            })
            .as_ref()
    }

    /// Shape and rasterize one run of cells.
    ///
    /// Returns glyphs positioned relative to the run's top-left corner. An
    /// empty result means the run could not be shaped at all, and the caller
    /// keeps whatever fallback behaviour it had.
    pub(crate) fn place_run(&self, request: &RunRequest<'_>) -> Vec<PlacedGlyph> {
        if !self.options.enabled || request.cells.is_empty() || request.cell_w == 0 {
            return Vec::new();
        }
        let Some(state) = self.state() else {
            return Vec::new();
        };
        let Ok(mut state) = state.lock() else {
            tracing::warn!("shaping state was poisoned; falling back to unshaped rendering");
            return Vec::new();
        };
        state.place_run(request)
    }

    /// Font families the engine will try, in precedence order.
    #[cfg(test)]
    pub(crate) fn families(&self) -> Vec<String> {
        match self.state() {
            Some(state) => state.lock().map(|s| s.families.clone()).unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// The color emoji family the engine settled on, if any.
    #[cfg(test)]
    pub(crate) fn emoji_family(&self) -> Option<String> {
        match self.state() {
            Some(state) => state.lock().ok().and_then(|s| s.emoji_family.clone()),
            None => None,
        }
    }

    /// Every `(family, face index)` pair the font database holds, in database
    /// order, so a caller can check that a font collection kept its indices.
    #[cfg(test)]
    pub(crate) fn loaded_faces(&self) -> Vec<(String, u32)> {
        let Some(state) = self.state() else {
            return Vec::new();
        };
        let Ok(state) = state.lock() else {
            return Vec::new();
        };
        let db = state.font_system.db();
        db.faces()
            .filter_map(|face| {
                let (_, index) = db.face_source(face.id)?;
                let family = face.families.first()?.0.clone();
                Some((family, index))
            })
            .collect()
    }
}

impl ShapingState {
    fn build(sources: &[FontSource], options: &ShapingOptions) -> Self {
        let mut db = fontdb::Database::new();
        let mut families: Vec<String> = Vec::new();
        let mut explicit_ids: Vec<fontdb::ID> = Vec::new();

        for source in sources {
            if explicit_ids.len() >= MAX_EXPLICIT_FACES {
                tracing::warn!(
                    "ignoring font {:?}: more than {} explicit faces were configured",
                    source,
                    MAX_EXPLICIT_FACES
                );
                break;
            }
            let make_source = || match source {
                FontSource::Embedded(bytes) => Some(fontdb::Source::Binary(Arc::new(*bytes))),
                FontSource::File(path) => {
                    if !path.is_file() {
                        tracing::warn!("ignoring unreadable shaping font {:?}", path);
                        return None;
                    }
                    Some(fontdb::Source::File(path.clone()))
                }
            };
            let Some(probe_source) = make_source() else {
                continue;
            };
            let mut probe = fontdb::Database::new();
            let face_count = probe.load_font_source(probe_source).len();
            if !explicit_faces_fit(explicit_ids.len(), face_count) {
                let remaining = MAX_EXPLICIT_FACES - explicit_ids.len();
                tracing::warn!(
                    "ignoring font {:?}: its {} faces exceed the remaining explicit-face \
                     limit of {}",
                    source,
                    face_count,
                    remaining
                );
                continue;
            }
            let Some(load_source) = make_source() else {
                continue;
            };
            let loaded = db.load_font_source(load_source);
            for id in loaded {
                explicit_ids.push(id);
                if let Some(face) = db.face(id)
                    && let Some((family, _)) = face.families.first()
                    && !families.iter().any(|known| known == family)
                {
                    families.push(family.clone());
                }
            }
        }

        // Resolved while the database still holds nothing but the configured
        // fonts, so an explicitly configured color font always wins over
        // whatever the machine happens to have installed.
        let explicit_emoji = first_color_family(&db, explicit_ids.iter().copied());

        if options.system_fonts {
            db.load_system_fonts();
        }
        // The renderer always names a concrete family, so the generic families
        // only matter to cosmic-text's own platform fallback list.
        if let Some(primary) = families.first() {
            db.set_monospace_family(primary.clone());
        }

        let emoji_family = explicit_emoji.or_else(|| discover_emoji_family(&db));

        let locale = options.locale();
        let font_system = FontSystem::new_with_locale_and_db(locale, db);

        Self {
            font_system,
            swash: SwashCache::new(),
            families,
            emoji_family,
            shape_cache: HashMap::new(),
            glyph_cache: HashMap::new(),
            glyph_bytes: 0,
        }
    }

    fn place_run(&mut self, request: &RunRequest<'_>) -> Vec<PlacedGlyph> {
        let mut placed = Vec::new();
        let mut origin_cols = 0u32;
        let chunk_cells = run_chunk_limit(request.cells);
        for chunk in request.cells.chunks(chunk_cells) {
            let chunk_request = RunRequest {
                cells: chunk,
                ..request.clone()
            };
            let origin_x = i32::try_from(origin_cols * request.cell_w).unwrap_or(i32::MAX);
            self.place_chunk(&chunk_request, origin_x, &mut placed);
            origin_cols += chunk.iter().map(|cell| cell.cols).sum::<u32>();
        }
        placed
    }

    fn place_chunk(&mut self, request: &RunRequest<'_>, origin_x: i32, out: &mut Vec<PlacedGlyph>) {
        // Assemble the run's text, remembering the byte range and the pixel
        // span each cell contributed.
        let mut text = String::new();
        let mut ranges: Vec<std::ops::Range<usize>> = Vec::with_capacity(request.cells.len());
        let mut lefts: Vec<u32> = Vec::with_capacity(request.cells.len());
        let mut widths: Vec<u32> = Vec::with_capacity(request.cells.len());
        let mut pen = 0u32;
        for cell in request.cells {
            let start = text.len();
            text.push_str(&cell.text);
            ranges.push(start..text.len());
            lefts.push(pen);
            widths.push(cell.cols * request.cell_w);
            pen += cell.cols * request.cell_w;
        }
        if text.is_empty() {
            return;
        }

        let emoji = unicode::cluster_is_emoji(&text);
        let contiguous = unicode::cluster_forces_contiguous_run(&text);
        let shaped = self.shape(&text, request, emoji);
        if shaped.glyphs.is_empty() {
            return;
        }

        // vt100 0.16 stores one scalar cluster per cell, so an emoji sequence
        // arrives here in pieces - and with a piece's worth of width each. The
        // pieces are reassembled above (the run's text is the whole sequence),
        // but the extra columns are the terminal core's to fix; all this can
        // do is refuse to stretch the glyph across them. Logged rather than
        // worked around, so the limitation is visible while it lasts.
        if request
            .cells
            .iter()
            .any(|cell| unicode::is_split_emoji_fragment(&cell.text))
        {
            tracing::debug!(
                "terminal core split {:?} across {} cells; the sequence is drawn at its own \
                 width and the extra columns are left blank",
                text,
                request.cells.len()
            );
        }

        // Work out which cells the shaper actually tied together. A glyph that
        // covers bytes from more than one cell - a ligature, a reordered
        // cluster, an emoji sequence vt100 split up - merges them, so the
        // group is projected into the cells it really came from rather than
        // being sliced down the middle.
        let mut starts_group = vec![true; ranges.len()];
        if contiguous {
            // Joining and reordering scripts are laid out as one piece:
            // centering each Arabic letter in its own cell would break every
            // connecting stroke.
            starts_group.iter_mut().skip(1).for_each(|s| *s = false);
        } else {
            for glyph in &shaped.glyphs {
                let first = cell_of_byte(&ranges, glyph.start);
                let last = cell_of_byte(&ranges, glyph.end.saturating_sub(1)).max(first);
                for slot in starts_group.iter_mut().take(last + 1).skip(first + 1) {
                    *slot = false;
                }
            }
        }

        let mut start = 0usize;
        while start < ranges.len() {
            let mut end = start + 1;
            while end < ranges.len() && !starts_group[end] {
                end += 1;
            }
            let bytes = ranges[start].start..ranges[end - 1].end;
            let members: Vec<&LayoutGlyph> = shaped
                .glyphs
                .iter()
                // A glyph nothing covered maps to `.notdef`, which most faces
                // draw as a tofu box. The fontdue path has always refused to
                // count that as coverage, and the shaped path refuses too: a
                // character no font has stays blank rather than becoming a
                // box that was never there before.
                .filter(|glyph| glyph.glyph_id != 0 && bytes.contains(&glyph.start))
                .collect();
            if !members.is_empty() {
                let target_x = lefts[start];
                let target_w: u32 = widths[start..end].iter().sum();
                self.place_group(
                    &members, request, origin_x, target_x, target_w, shaped.rtl, out,
                );
            }
            start = end;
        }
    }

    /// Project one group of glyphs into the cells it belongs to.
    #[allow(clippy::too_many_arguments)]
    fn place_group(
        &mut self,
        members: &[&LayoutGlyph],
        request: &RunRequest<'_>,
        origin_x: i32,
        target_x: u32,
        target_w: u32,
        rtl: bool,
        out: &mut Vec<PlacedGlyph>,
    ) {
        let min_x = members.iter().fold(f32::INFINITY, |acc, g| acc.min(g.x));
        let max_x = members
            .iter()
            .fold(f32::NEG_INFINITY, |acc, g| acc.max(g.x + g.w));
        let natural = (max_x - min_x).max(0.0);
        let cell_w = request.cell_w.max(1);

        // Fit the cluster to the cells it actually needs, never to every cell
        // vt100 handed the run. The two differ whenever the terminal core
        // over-allocates - a ZWJ emoji sequence gets one double-width cell per
        // fragment - and stretching the glyph across all of them would draw it
        // half again as large as the text beside it. Growing a cluster to fill
        // the cells it does need is the same thing the fontdue fallback path
        // does, with the same clamp, so a CJK glyph still tiles its two cells.
        let natural_cells = ((natural / cell_w as f32).ceil() as u32).max(1);
        let fit_w = (natural_cells * cell_w).min(target_w);
        let scale = if natural > 0.0 && fit_w > 0 {
            (fit_w as f32 / natural).clamp(MIN_FIT_SCALE, MAX_FIT_SCALE)
        } else {
            1.0
        };
        let scaled = natural * scale;

        // A cluster narrower than its cells is centered in as many whole cells
        // as it actually needs, anchored at the start of the span (the end,
        // for a right-to-left run). That keeps a two-cell emoji where the
        // column says it is instead of drifting into the middle of the six
        // cells vt100 hands out for a ZWJ sequence.
        let used = if scaled <= 0.0 {
            target_w
        } else {
            (((scaled / cell_w as f32).ceil() as u32).max(1) * cell_w).min(target_w)
        };
        let lead = ((used as f32 - scaled) / 2.0).round();
        let anchor = if rtl {
            target_x as f32 + (target_w - used) as f32 + lead
        } else {
            target_x as f32 + lead
        };
        let offset = (anchor - min_x * scale, request.ascent);

        for glyph in members {
            let physical = glyph.physical(offset, scale);
            let Some(raster) = self.rasterize(physical.cache_key) else {
                continue;
            };
            out.push(PlacedGlyph {
                x: origin_x + physical.x + raster.left,
                y: physical.y - raster.top,
                bitmap: Arc::clone(&raster.bitmap),
            });
        }
    }

    /// Shape one run, caching the result.
    fn shape(&mut self, text: &str, request: &RunRequest<'_>, emoji: bool) -> Arc<ShapedRun> {
        let key = ShapeKey {
            text: text.to_string(),
            size_bits: request.font_size.to_bits(),
            bold: request.bold,
            italic: request.italic,
            emoji,
        };
        if let Some(cached) = self.shape_cache.get(&key) {
            return Arc::clone(cached);
        }
        if self.shape_cache.len() >= MAX_SHAPE_CACHE_ENTRIES {
            self.shape_cache.clear();
        }

        let family = self.family_for(text, emoji, request.bold);
        let weight = if request.bold {
            Weight::BOLD
        } else {
            Weight::NORMAL
        };
        let mut attrs = Attrs::new().family(Family::Name(&family)).weight(weight);
        if request.italic {
            // The fontdue path never picks a real italic face either; it
            // shears the regular one. Asking swash for the same synthetic
            // slant keeps the two paths looking like one renderer.
            attrs = attrs.cache_key_flags(CacheKeyFlags::FAKE_ITALIC);
        }

        let metrics = Metrics::new(request.font_size, request.font_size.max(1.0));
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        {
            let mut buffer = buffer.borrow_with(&mut self.font_system);
            // The terminal already decided where this text ends. Nothing here
            // is allowed to wrap it.
            buffer.set_wrap(Wrap::None);
            buffer.set_size(None, None);
            buffer.set_text(text, &attrs, Shaping::Advanced, None);
            buffer.shape_until_scroll(false);
        }

        let mut glyphs = Vec::new();
        let mut rtl = false;
        for run in buffer.layout_runs() {
            rtl |= run.rtl;
            glyphs.extend(run.glyphs.iter().cloned());
        }
        let shaped = Arc::new(ShapedRun { glyphs, rtl });
        self.shape_cache.insert(key, Arc::clone(&shaped));
        shaped
    }

    /// The family a run is shaped with: a color emoji face for emoji
    /// presentation, otherwise the first explicitly configured family that
    /// covers the whole run, otherwise the primary family - which is what
    /// hands the missing glyphs to cosmic-text's automatic fallback.
    fn family_for(&mut self, text: &str, emoji: bool, bold: bool) -> String {
        if emoji && let Some(family) = self.emoji_family.clone() {
            return family;
        }
        let weight = if bold { Weight::BOLD } else { Weight::NORMAL };
        // Joiners and variation selectors are format characters no font has to
        // carry, so they do not count against a font's coverage.
        let wanted: Vec<char> = text
            .chars()
            .filter(|c| !unicode::is_variation_selector(*c) && *c != unicode::ZWJ)
            .collect();
        if !wanted.is_empty() {
            for family in self.families.clone() {
                let ids: Vec<fontdb::ID> = self
                    .font_system
                    .db()
                    .faces()
                    .filter(|face| face.families.iter().any(|(name, _)| *name == family))
                    .map(|face| face.id)
                    .collect();
                for id in ids {
                    if self.face_covers(id, weight, &wanted) {
                        return family;
                    }
                }
            }
        }
        self.families.first().cloned().unwrap_or_default()
    }

    /// Whether one face has a glyph for every character in `wanted`.
    ///
    /// Asked of the face's own character map rather than of `fontdb`'s face
    /// info, so a font collection answers for the exact face index and a
    /// character that only maps to `.notdef` does not count as covered.
    fn face_covers(&mut self, id: fontdb::ID, weight: Weight, wanted: &[char]) -> bool {
        let Some(font) = self.font_system.get_font(id, weight) else {
            return false;
        };
        let charmap = font.as_swash().charmap();
        wanted.iter().all(|c| charmap.map(*c) != 0)
    }

    /// Rasterize one glyph, caching the bitmap.
    fn rasterize(&mut self, key: CacheKey) -> Option<Arc<RasterGlyph>> {
        if let Some(cached) = self.glyph_cache.get(&key) {
            return cached.clone();
        }
        if glyph_cache_full(self.glyph_cache.len(), self.glyph_bytes) {
            self.glyph_cache.clear();
            self.swash.image_cache.clear();
            self.glyph_bytes = 0;
        }
        let raster = self
            .swash
            .get_image_uncached(&mut self.font_system, key)
            .and_then(|image| {
                let width = image.placement.width;
                let height = image.placement.height;
                if width == 0 || height == 0 {
                    return None;
                }
                let pixels = (width as usize).saturating_mul(height as usize);
                if pixels > MAX_GLYPH_PIXELS {
                    tracing::warn!("skipping a {width}x{height} glyph bitmap: over the size limit");
                    return None;
                }
                let color = matches!(image.content, SwashContent::Color);
                let expected = if color { pixels * 4 } else { pixels };
                if image.data.len() < expected {
                    return None;
                }
                Some(Arc::new(RasterGlyph {
                    bitmap: Arc::new(GlyphBitmap {
                        width,
                        height,
                        color,
                        data: image.data[..expected].to_vec(),
                    }),
                    left: image.placement.left,
                    top: image.placement.top,
                }))
            });
        if let Some(raster) = &raster {
            self.glyph_bytes = self.glyph_bytes.saturating_add(raster.bitmap.data.len());
        }
        self.glyph_cache.insert(key, raster.clone());
        raster
    }
}

/// Index of the cell whose byte range holds `byte`, clamped to the last cell.
fn cell_of_byte(ranges: &[std::ops::Range<usize>], byte: usize) -> usize {
    match ranges.iter().position(|range| range.contains(&byte)) {
        Some(index) => index,
        None => ranges.len().saturating_sub(1),
    }
}

/// The first family among `ids` whose face carries a color table.
fn first_color_family(
    db: &fontdb::Database,
    ids: impl Iterator<Item = fontdb::ID>,
) -> Option<String> {
    for id in ids {
        let has_color = db.with_face_data(id, has_color_table).unwrap_or(false);
        if has_color
            && let Some(face) = db.face(id)
            && let Some((family, _)) = face.families.first()
        {
            return Some(family.clone());
        }
    }
    None
}

/// Find a color emoji family among everything the database holds.
fn discover_emoji_family(db: &fontdb::Database) -> Option<String> {
    for wanted in KNOWN_EMOJI_FAMILIES {
        let ids = db
            .faces()
            .filter(|face| face.families.iter().any(|(name, _)| name == wanted))
            .map(|face| face.id);
        if let Some(family) = first_color_family(db, ids) {
            return Some(family);
        }
    }
    // Anything else that calls itself an emoji font and really is one, sorted
    // so two machines with the same fonts settle on the same face.
    let mut candidates: Vec<(String, fontdb::ID)> = db
        .faces()
        .filter_map(|face| {
            let (family, _) = face.families.first()?;
            family
                .to_ascii_lowercase()
                .contains("emoji")
                .then(|| (family.clone(), face.id))
        })
        .collect();
    candidates.sort();
    for (_, id) in candidates {
        if let Some(family) = first_color_family(db, std::iter::once(id)) {
            return Some(family);
        }
    }
    None
}

/// Whether the face at `index` of an sfnt file or font collection carries
/// color glyphs.
///
/// Reads the table directory directly rather than pulling in another font
/// parser: `COLR` (layered outlines), `CBDT`/`sbix` (color bitmaps), and `SVG`
/// are the four tables that make a face a color font. Every read is bounds
/// checked, because this runs over font files the user pointed at.
fn has_color_table(data: &[u8], index: u32) -> bool {
    const COLOR_TABLES: [&[u8]; 4] = [b"COLR", b"CBDT", b"sbix", b"SVG "];
    let read_u16 = |at: usize| -> Option<u16> {
        let bytes = data.get(at..at.checked_add(2)?)?;
        Some(u16::from_be_bytes([bytes[0], bytes[1]]))
    };
    let read_u32 = |at: usize| -> Option<u32> {
        let bytes = data.get(at..at.checked_add(4)?)?;
        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };

    let mut directory = 0usize;
    if data.get(0..4) == Some(b"ttcf".as_slice()) {
        let Some(count) = read_u32(8) else {
            return false;
        };
        if index >= count {
            return false;
        }
        let Some(offset) = read_u32(12 + index as usize * 4) else {
            return false;
        };
        directory = offset as usize;
    }

    let Some(num_tables) = read_u16(directory + 4) else {
        return false;
    };
    (0..num_tables as usize).any(|table| {
        let record = directory + 12 + table * 16;
        data.get(record..record + 4)
            .is_some_and(|tag| COLOR_TABLES.contains(&tag))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_presets_say_what_they_mean() {
        let options = ShapingOptions::default();
        assert!(options.enabled);
        assert!(options.system_fonts);
        assert!(!ShapingOptions::disabled().enabled);
        assert!(ShapingOptions::deterministic().enabled);
        assert!(!ShapingOptions::deterministic().system_fonts);
        assert!(!ShapingOptions::default().with_enabled(false).enabled);
        assert!(
            !ShapingOptions::default()
                .with_system_fonts(false)
                .system_fonts
        );
        assert_eq!(ShapingOptions::deterministic().locale(), "en-US");
    }

    #[test]
    fn explicit_face_limit_rejects_oversized_collections() {
        assert!(explicit_faces_fit(0, MAX_EXPLICIT_FACES));
        assert!(explicit_faces_fit(MAX_EXPLICIT_FACES - 1, 1));
        assert!(!explicit_faces_fit(MAX_EXPLICIT_FACES, 1));
        assert!(!explicit_faces_fit(1, MAX_EXPLICIT_FACES));
        assert!(!explicit_faces_fit(usize::MAX, 1));
    }

    #[test]
    fn cache_limits_cover_failed_glyphs_as_well_as_bitmap_bytes() {
        assert!(!glyph_cache_full(0, 0));
        assert!(glyph_cache_full(MAX_GLYPH_CACHE_ENTRIES, 0));
        assert!(glyph_cache_full(0, MAX_GLYPH_CACHE_BYTES));
    }

    #[test]
    fn supported_width_contiguous_runs_are_never_split() {
        let arabic = vec![
            RunCell {
                text: "\u{0645}".to_string(),
                cols: 1,
            };
            500
        ];
        let emoji = vec![
            RunCell {
                text: "\u{1F600}".to_string(),
                cols: 2,
            };
            500
        ];
        assert_eq!(run_chunk_limit(&arabic), 512);
        assert_eq!(run_chunk_limit(&emoji), 256);
    }

    #[test]
    fn a_plain_font_has_no_color_table() {
        let data = std::fs::read("fonts/JetBrainsMono-Regular.ttf").expect("embedded font");
        assert!(!has_color_table(&data, 0));
    }

    #[test]
    fn malformed_font_data_is_rejected_rather_than_panicking() {
        assert!(!has_color_table(&[], 0));
        assert!(!has_color_table(b"ttcf", 0));
        assert!(!has_color_table(b"ttcf\x00\x01\x00\x00\x00\x00\x00\x01", 0));
        assert!(!has_color_table(b"\x00\x01\x00\x00\xff\xff", 0));
        assert!(!has_color_table(b"\x00\x01\x00\x00\x00\x01\x00\x00", 7));
    }

    #[test]
    fn a_byte_maps_to_the_cell_that_contributed_it() {
        let ranges = vec![0..1, 1..4, 4..5];
        assert_eq!(cell_of_byte(&ranges, 0), 0);
        assert_eq!(cell_of_byte(&ranges, 2), 1);
        assert_eq!(cell_of_byte(&ranges, 4), 2);
        // Past the end clamps rather than panicking.
        assert_eq!(cell_of_byte(&ranges, 99), 2);
    }
}
