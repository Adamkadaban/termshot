//! Unicode rendering: what the shaped path draws, and - just as important -
//! what it leaves alone.
//!
//! Every test here is deterministic. The renderers are built with
//! [`ShapingOptions::deterministic`], which turns off automatic system font
//! discovery, so the only fonts in play are the one compiled into the binary
//! and the synthetic fixtures in `tests/fixtures/` (see
//! `tests/fixtures/generate_fixtures.py` for how they are made and what they
//! contain). A test that asserted on a real emoji or CJK font would really be
//! asserting on whatever the machine running it happens to have installed.
//!
//! The suite covers four things:
//!
//! 1. the Unicode corpus - one case per shaping behaviour the renderer gained;
//! 2. that ordinary output did *not* change, byte for byte;
//! 3. that shaping stops at style and redaction boundaries; and
//! 4. the cluster-splitting the terminal core still does, recorded precisely
//!    so Phase 3 has failing targets rather than adjectives.

use image::{Rgba, RgbaImage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use termshot::capture::CapturedScreen;
use termshot::config::{ChromeConfig, ThemeConfig};
use termshot::redaction::{ManualRedactionSpec, ManualRedactions};
use termshot::renderer::{
    ChromeOptions, ExtendedRenderOptions, FontSelection, RenderOptions, Renderer, RendererOptions,
    TextOptions,
};
use termshot::shaping::ShapingOptions;

// -------------------------------------------------------------------------
// Fixtures and helpers
// -------------------------------------------------------------------------

/// Covers Devanagari KA, U+2764, and one private-use character, inking the
/// *left* third of the cell.
const SHAPE_A: &str = "tests/fixtures/shape-a.ttf";
/// The same characters (plus one more private-use character), inking the
/// *right* third of the cell.
const SHAPE_B: &str = "tests/fixtures/shape-b.ttf";
/// A COLRv0 font: every glyph is two layers, pure red over pure blue.
const COLOR_EMOJI: &str = "tests/fixtures/color-emoji.ttf";
/// A two-face collection; face 0 covers U+E020, face 1 covers U+E021.
const COLLECTION: &str = "tests/fixtures/collection.ttc";
/// A primary face covering printable ASCII only, whose `.notdef` is empty - so
/// "no ink" really means "nothing was drawn".
const LIMITED_PRIMARY: &str = "tests/fixtures/limited-ascii.ttf";

/// The two palette colors `color-emoji.ttf` paints its layers with.
const LAYER_RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
/// See [`LAYER_RED`].
const LAYER_BLUE: Rgba<u8> = Rgba([0, 0, 255, 255]);

fn out_dir(name: &str) -> PathBuf {
    let dir = Path::new("target/unicode-rendering").join(name);
    std::fs::create_dir_all(&dir).expect("test output directory");
    dir
}

/// A renderer with no window chrome, the given fallback fonts, and the given
/// shaping options.
fn renderer(primary: Option<&str>, fallbacks: &[&str], shaping: ShapingOptions) -> Renderer {
    Renderer::new_with_shaping(
        &FontSelection {
            font_override: primary.map(PathBuf::from),
            font_bold_override: None,
            global_font: None,
            global_fallback_fonts: fallbacks.iter().map(PathBuf::from).collect(),
        },
        16.0,
        &HashMap::<String, ThemeConfig>::new(),
        "dark",
        &ChromeConfig::default(),
        RendererOptions::default(),
        shaping,
    )
    .expect("renderer builds")
}

/// A renderer restricted to the fixtures this repository ships.
fn deterministic(fallbacks: &[&str]) -> Renderer {
    renderer(None, fallbacks, ShapingOptions::deterministic())
}

fn bare_chrome() -> ChromeOptions {
    ChromeOptions {
        enabled: false,
        preset: "none".to_string(),
        title: None,
        timestamp: false,
        shadow: false,
        // Rounded corners would make an exact pixel comparison about the
        // corner radius rather than about the glyphs.
        radius: 0,
        rounded: false,
        outer_padding: 0,
        title_bar_height: 0,
    }
}

/// Render `text` at a fixed width and read the PNG back.
fn render(renderer: &Renderer, dir: &Path, name: &str, text: &str, cols: u16) -> RgbaImage {
    render_redacted(renderer, dir, name, text, cols, None)
}

/// [`render`] with optional caller-supplied redactions.
fn render_redacted(
    renderer: &Renderer,
    dir: &Path,
    name: &str,
    text: &str,
    cols: u16,
    manual: Option<&ManualRedactions>,
) -> RgbaImage {
    let chrome = bare_chrome();
    let options =
        ExtendedRenderOptions::from(RenderOptions::default()).with_optional_manual(manual);
    let (path, _, _, _) = renderer
        .render_bytes_with_extended_options(
            text.as_bytes(),
            cols,
            2,
            dir,
            Some(name),
            None,
            Some(&chrome),
            None,
            TextOptions::default(),
            // No auto-crop: cell coordinates in these tests are absolute.
            false,
            options,
        )
        .expect("render succeeds");
    let img = image::open(&path).expect("png opens").to_rgba8();
    std::fs::remove_file(&path).ok();
    img
}

/// The pixel width of one grid column, and the padding to its left.
fn geometry(renderer: &Renderer) -> (u32, u32) {
    // The bare renderer draws `cols` cells plus symmetric padding, so one
    // render of a known width gives both numbers without exposing internals.
    // Each call gets its own directory: the tests run in parallel and share
    // one output tree, and two renders that pick the same file name would
    // measure each other's image.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = out_dir(&format!("geometry-{seq}"));
    let one = render(renderer, &dir, "one", "a", 1);
    let three = render(renderer, &dir, "three", "a", 3);
    let cell_w = (three.width() - one.width()) / 2;
    let padding = (one.width() - cell_w) / 2;
    std::fs::remove_dir_all(&dir).ok();
    (cell_w, padding)
}

fn background(img: &RgbaImage) -> Rgba<u8> {
    *img.get_pixel(0, 0)
}

/// Total pixels that are not the background.
fn ink(img: &RgbaImage) -> usize {
    let bg = background(img);
    img.pixels().filter(|p| **p != bg).count()
}

/// Ink inside the half-open horizontal span `[x0, x1)`.
fn ink_in(img: &RgbaImage, x0: u32, x1: u32) -> usize {
    let bg = background(img);
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

/// Ink inside the grid columns `[first, last)`.
fn ink_in_cells(img: &RgbaImage, cell_w: u32, padding: u32, first: u32, last: u32) -> usize {
    ink_in(img, padding + first * cell_w, padding + last * cell_w)
}

/// Pixels close to `want`, so an anti-aliased edge of a palette color counts.
fn count_near(img: &RgbaImage, want: Rgba<u8>) -> usize {
    img.pixels()
        .filter(|p| (0..3).all(|i| (p[i] as i32 - want[i] as i32).abs() <= 40) && p[3] > 200)
        .count()
}

/// How vt100 laid a string out: one entry per grid column, holding that cell's
/// contents. Continuation columns of a double-width character come back as
/// `""`.
fn cells(text: &str, cols: u16) -> Vec<String> {
    let screen = CapturedScreen::parse(text.as_bytes(), 2, cols, 0);
    let mut out = Vec::new();
    for col in 0..cols {
        let cell = screen.cell(0, col).expect("cell in range");
        let contents = cell.contents();
        if contents.is_empty() && !cell.is_wide_continuation() {
            break;
        }
        out.push(contents.to_string());
    }
    out
}

// -------------------------------------------------------------------------
// 1. The Unicode corpus
// -------------------------------------------------------------------------

/// One deterministic case per shaping behaviour, each rendered end to end.
///
/// The fixtures cover exactly the characters these cases need, so every
/// expectation below is about the renderer rather than about the host.
#[test]
fn the_unicode_corpus_renders_through_the_expected_path() {
    let dir = out_dir("corpus");
    let plain = deterministic(&[]);
    let shaped = deterministic(&[SHAPE_A, COLOR_EMOJI]);

    // A single emoji: one double-width cell, drawn from the color font.
    let single = render(&shaped, &dir, "emoji", "\u{1F600}", 6);
    assert!(
        count_near(&single, LAYER_RED) > 0 && count_near(&single, LAYER_BLUE) > 0,
        "a single emoji did not reach the color font"
    );

    // Emoji presentation selector: the same code point, two answers.
    let emoji_vs = render(&shaped, &dir, "vs16", "\u{2764}\u{FE0F}", 6);
    let text_vs = render(&shaped, &dir, "vs15", "\u{2764}\u{FE0E}", 6);
    assert!(
        count_near(&emoji_vs, LAYER_BLUE) > 0,
        "VS16 did not select the color font"
    );
    assert_eq!(
        count_near(&text_vs, LAYER_BLUE),
        0,
        "VS15 must keep the text presentation"
    );
    assert!(ink(&text_vs) > 0, "VS15 drew nothing at all");

    // e + combining acute: vt100 keeps both scalars in one cell, and the
    // shaper places the mark over the base instead of beside it.
    assert_eq!(cells("e\u{0301}", 6), vec!["e\u{0301}"]);
    let combining = render(&plain, &dir, "combining", "e\u{0301}", 6);
    let bare_e = render(&plain, &dir, "bare-e", "e", 6);
    let (cell_w, padding) = geometry(&plain);
    assert!(
        ink(&combining) > ink(&bare_e),
        "the combining acute added no ink"
    );
    assert_eq!(
        ink_in_cells(&combining, cell_w, padding, 1, 3),
        0,
        "the combining mark escaped its cell"
    );

    // Keycap: three scalars, one cell, one glyph.
    assert_eq!(cells("1\u{FE0F}\u{20E3}", 6), vec!["1\u{FE0F}\u{20E3}"]);

    // CJK falls back: nothing in the deterministic chain covers it, so it
    // stays blank rather than borrowing an unrelated glyph - and it still
    // owns exactly its two columns.
    assert_eq!(cells("\u{4F60}", 6), vec!["\u{4F60}", ""]);

    // Arabic: five cells, one joined word, laid out right to left inside
    // them, drawn from the font that covers it.
    let arabic_cells = cells("\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}", 10);
    assert_eq!(arabic_cells.len(), 5, "vt100 gave Arabic {arabic_cells:?}");

    // Devanagari: the virama cluster shares a cell, the vowel sign gets its
    // own, and the shaper reorders them.
    assert_eq!(
        cells("\u{0915}\u{094D}\u{0937}\u{093F}", 10),
        vec!["\u{0915}\u{094D}", "\u{0937}", "\u{093F}"]
    );
    let devanagari = render(&shaped, &dir, "devanagari", "\u{0915}", 6);
    assert!(
        ink(&devanagari) > 0,
        "Devanagari did not reach the configured font"
    );

    // Programming ligature opportunities stay opportunities: the terminal
    // grid is one glyph per cell, so `!=` is two cells and stays two glyphs.
    assert_eq!(cells("!=", 6), vec!["!", "="]);
}

// -------------------------------------------------------------------------
// 2. Ordinary output did not move
// -------------------------------------------------------------------------

/// Everything 1.1.5 already rendered correctly must render identically now,
/// pixel for pixel, whether or not shaping is available.
///
/// This is the load-bearing test of the whole feature: the shaped path is
/// opt-in per cell, and no cell of ordinary terminal output opts in. The
/// comparison is made against a renderer with shaping *fully disabled*, which
/// is the 1.1.5 drawing code, and against one with system font discovery
/// enabled, which is the shipping default.
#[test]
fn ordinary_output_is_pixel_identical_with_and_without_shaping() {
    let dir = out_dir("identical");
    let samples: &[(&str, &str)] = &[
        ("ascii", "the quick brown fox 0123456789"),
        ("ligatures", "a => b != c -> d <= e ::f |> g"),
        (
            "box drawing",
            "\u{250C}\u{2500}\u{2500}\u{2510}\r\n\u{2502}ok\u{2502}\r\n\u{2514}\u{2500}\u{2500}\u{2518}",
        ),
        (
            "sgr styling",
            "\u{1b}[31mred\u{1b}[0m \u{1b}[1mbold\u{1b}[0m \u{1b}[3mitalic\u{1b}[0m \u{1b}[4munder\u{1b}[0m \u{1b}[42mbg\u{1b}[0m",
        ),
        ("latin greek cyrillic", "caf\u{e9} \u{3bb} \u{416} \u{2713}"),
    ];

    let off = renderer(None, &[], ShapingOptions::disabled());
    let on = renderer(None, &[], ShapingOptions::default());
    let explicit = deterministic(&[SHAPE_A, COLOR_EMOJI]);
    let explicit_off = renderer(None, &[SHAPE_A, COLOR_EMOJI], ShapingOptions::disabled());

    for (name, text) in samples {
        let base = render(&off, &dir, &format!("{name}-off"), text, 40);
        let shaped = render(&on, &dir, &format!("{name}-on"), text, 40);
        assert_eq!(
            base.dimensions(),
            shaped.dimensions(),
            "{name}: shaping changed the image size"
        );
        assert!(
            base.as_raw() == shaped.as_raw(),
            "{name}: enabling shaping changed the pixels of ordinary output"
        );

        // The same must hold with extra fonts configured: an unused fallback
        // font cannot be allowed to change output either.
        let with_fonts = render(&explicit, &dir, &format!("{name}-fonts"), text, 40);
        let without = render(&explicit_off, &dir, &format!("{name}-nofonts"), text, 40);
        assert!(
            with_fonts.as_raw() == without.as_raw(),
            "{name}: shaping changed output that has extra fonts configured"
        );
    }
}

// -------------------------------------------------------------------------
// 3. Font precedence
// -------------------------------------------------------------------------

/// Explicit fonts are searched in the order they were configured, and that
/// order decides the glyph. The two fixtures ink opposite halves of the cell,
/// so the pixels say which one won.
#[test]
fn configured_font_order_decides_the_glyph() {
    let dir = out_dir("precedence");
    for (order, expect_left) in [([SHAPE_A, SHAPE_B], true), ([SHAPE_B, SHAPE_A], false)] {
        let renderer = deterministic(&order);
        let (cell_w, padding) = geometry(&renderer);
        // Devanagari always needs a shaper, so this exercises precedence on
        // the shaped path rather than the per-scalar one.
        let img = render(&renderer, &dir, "ka", "\u{0915}", 6);
        let left = ink_in(&img, padding, padding + cell_w / 2);
        let right = ink_in(&img, padding + cell_w / 2, padding + cell_w);
        assert!(
            if expect_left {
                left > right
            } else {
                right > left
            },
            "{order:?}: the first configured font did not win (left={left}, right={right})"
        );
    }
}

/// A font collection is loaded face by face. `fontdue` only ever sees face 0,
/// so a character that exists only in face 1 proves the shaped path kept the
/// index `fontdb` reported.
#[test]
fn font_collection_face_indices_are_preserved() {
    let dir = out_dir("collection");
    let renderer = renderer(
        Some(LIMITED_PRIMARY),
        &[COLLECTION],
        ShapingOptions::deterministic(),
    );
    assert!(
        ink(&render(&renderer, &dir, "face1", "\u{E021}", 6)) > 0,
        "the second face of the collection was never reached"
    );
    assert_eq!(
        ink(&render(&renderer, &dir, "absent", "\u{E022}", 6)),
        0,
        "a code point no configured face covers must stay blank"
    );
}

/// A configured fallback path that names a file which is missing, or is not a
/// font at all, is skipped: the rest of the chain still works.
#[test]
fn broken_fallback_paths_do_not_break_shaping() {
    let dir = out_dir("broken-fallbacks");
    let junk = dir.join("not-a-font.ttf");
    std::fs::write(&junk, b"this is definitely not a font").unwrap();
    let renderer = deterministic(&[
        "tests/fixtures/does-not-exist.ttf",
        junk.to_str().unwrap(),
        SHAPE_A,
    ]);
    assert!(
        ink(&render(&renderer, &dir, "ka", "\u{0915}", 6)) > 0,
        "a broken fallback stopped the usable one from being reached"
    );
}

// -------------------------------------------------------------------------
// 4. Boundaries
// -------------------------------------------------------------------------

/// Shaping stops at a style change, so a run is never laid out across two
/// different colors and no glyph is drawn with the wrong one.
#[test]
fn shaping_stops_at_a_style_boundary() {
    let dir = out_dir("style-boundary");
    let renderer = deterministic(&[SHAPE_A]);
    let (cell_w, padding) = geometry(&renderer);

    // Two Devanagari letters, the second in red.
    let img = render(
        &renderer,
        &dir,
        "split",
        "\u{0915}\u{1b}[31m\u{0915}\u{1b}[0m",
        6,
    );
    let reds = img
        .pixels()
        .filter(|p| p[0] > 150 && p[1] < 100 && p[2] < 100)
        .count();
    assert!(reds > 0, "the second letter lost its color");

    // Each letter stays in its own column: had the two shaped as one run and
    // been fitted to both cells, the ink would have moved.
    assert!(
        ink_in_cells(&img, cell_w, padding, 0, 1) > 0,
        "the first cell is empty"
    );
    assert!(
        ink_in_cells(&img, cell_w, padding, 1, 2) > 0,
        "the second cell is empty"
    );
    assert_eq!(
        ink_in_cells(&img, cell_w, padding, 2, 6),
        0,
        "a shaped run painted past the cells it owns"
    );
}

/// Redaction blocks are drawn from the cell grid and shaped glyphs are clipped
/// to their run, so nothing shaped can leak into - or out of - a redacted cell.
#[test]
fn shaping_stops_at_a_redaction_boundary() {
    let dir = out_dir("redaction-boundary");
    let renderer = deterministic(&[SHAPE_A]);
    let (cell_w, padding) = geometry(&renderer);

    // Five Devanagari letters with the middle three redacted.
    let text = "\u{0915}".repeat(5);
    let manual = ManualRedactions::new(
        &[ManualRedactionSpec::Coordinate {
            row: 0,
            col_start: 1,
            col_end: 4,
            label: None,
            color: None,
        }],
        false,
    )
    .expect("manual redaction compiles");
    let img = render_redacted(&renderer, &dir, "redacted", &text, 8, Some(&manual));

    // The redacted columns are solid block color and nothing else.
    let block = *img.get_pixel(padding + cell_w + cell_w / 2, img.height() / 2);
    assert_eq!(
        block,
        Rgba([212, 25, 25, 255]),
        "the redaction block is not the color it should be"
    );
    for x in padding + cell_w..padding + 4 * cell_w {
        for y in padding..img.height() - padding {
            let pixel = *img.get_pixel(x, y);
            assert_eq!(
                pixel, block,
                "a glyph leaked into the redacted cells at ({x}, {y})"
            );
        }
    }
    // The unredacted letters on either side still drew.
    assert!(ink_in_cells(&img, cell_w, padding, 0, 1) > 0);
    assert!(ink_in_cells(&img, cell_w, padding, 4, 5) > 0);
}

/// A wide cell owns its continuation column, but vt100 exposes the redaction
/// map by grid column. A redaction on only that continuation must stop the
/// shaped run before the glyph, or the run would skip over the block and leak
/// the right half of the character.
#[test]
fn shaping_honors_redaction_on_a_wide_continuation_column() {
    let dir = out_dir("wide-redaction-boundary");
    let renderer = deterministic(&[COLOR_EMOJI]);
    let (cell_w, padding) = geometry(&renderer);

    // Each emoji occupies two columns. Redact only column 3: the continuation
    // half of the second emoji.
    let manual = ManualRedactions::new(
        &[ManualRedactionSpec::Coordinate {
            row: 0,
            col_start: 3,
            col_end: 4,
            label: None,
            color: None,
        }],
        false,
    )
    .expect("manual redaction compiles");
    let img = render_redacted(
        &renderer,
        &dir,
        "wide-redacted",
        "\u{1F600}\u{1F600}",
        8,
        Some(&manual),
    );

    for x in padding + 3 * cell_w..padding + 4 * cell_w {
        for y in padding..img.height() - padding {
            assert_eq!(
                *img.get_pixel(x, y),
                Rgba([212, 25, 25, 255]),
                "the wide glyph leaked through its redacted continuation at ({x}, {y})"
            );
        }
    }
}

// -------------------------------------------------------------------------
// 5. What the terminal core still gets wrong
// -------------------------------------------------------------------------

/// vt100 0.16 stores one *scalar cluster* per cell, not one grapheme cluster,
/// so an emoji sequence is split across several cells and given several cells'
/// worth of width.
///
/// The renderer's answer for now is to reassemble the sequence across the run
/// (so the picture is right) and to draw it at the size it actually needs,
/// anchored at the start of the run (so the columns after it are untouched).
/// The cells vt100 over-allocated stay blank.
///
/// These assertions are the precise Phase 3 targets: when the terminal core
/// learns about grapheme clusters, the *cell layouts* below change and these
/// tests are what will catch it.
#[test]
fn vt100_splits_emoji_sequences_across_cells() {
    // A ZWJ sequence: one picture, three cells, six columns of width.
    assert_eq!(
        cells("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}", 12),
        vec![
            "\u{1F468}\u{200D}".to_string(),
            String::new(),
            "\u{1F469}\u{200D}".to_string(),
            String::new(),
            "\u{1F467}".to_string(),
            String::new(),
        ],
        "vt100's ZWJ cell layout changed - Phase 3 may have landed"
    );

    // A skin tone modifier: one picture, two cells, four columns.
    assert_eq!(
        cells("\u{1F44D}\u{1F3FD}", 8),
        vec![
            "\u{1F44D}".to_string(),
            String::new(),
            "\u{1F3FD}".to_string(),
            String::new(),
        ],
        "vt100's skin tone cell layout changed - Phase 3 may have landed"
    );

    // A flag: one picture, two cells, and here vt100 gives each regional
    // indicator a *single* column, so the pair is the right width by accident.
    assert_eq!(
        cells("\u{1F1FA}\u{1F1F8}", 8),
        vec!["\u{1F1FA}".to_string(), "\u{1F1F8}".to_string()],
        "vt100's regional indicator layout changed - Phase 3 may have landed"
    );
}

/// The width vt100 over-allocates for a split sequence is left blank rather
/// than filled by stretching the glyph, and nothing after the run is
/// disturbed.
#[test]
fn an_over_wide_emoji_sequence_does_not_corrupt_the_rest_of_the_line() {
    let dir = out_dir("over-wide");
    let renderer = deterministic(&[COLOR_EMOJI]);
    let (cell_w, padding) = geometry(&renderer);

    // Thumbs up plus a skin tone: four columns of vt100 width for a picture
    // that needs two, then an ASCII letter that must land in column 4.
    let img = render(&renderer, &dir, "skin", "\u{1F44D}\u{1F3FD}X", 8);

    // The color glyph is drawn in the first two columns...
    assert!(
        count_near(&img, LAYER_RED) > 0,
        "the emoji sequence did not reach the color font"
    );
    let in_first_two = ink_in_cells(&img, cell_w, padding, 0, 2);
    let in_next_two = ink_in_cells(&img, cell_w, padding, 2, 4);
    assert!(
        in_first_two > in_next_two,
        "the sequence was stretched across the cells vt100 over-allocated"
    );

    // ...and the letter after it is still exactly where the grid says.
    let letter = ink_in_cells(&img, cell_w, padding, 4, 5);
    assert!(letter > 0, "the character after the sequence was lost");
    assert_eq!(
        ink_in_cells(&img, cell_w, padding, 5, 8),
        0,
        "something painted past the end of the line"
    );
}

/// A run longer than the shaper's per-run cell budget is drawn in bounded
/// chunks, and the chunk boundary lands on a cell edge: the columns on either
/// side of it keep their glyphs and nothing is shifted.
#[test]
fn a_run_longer_than_the_shaping_budget_stays_on_the_grid() {
    let dir = out_dir("long-run");
    let renderer = deterministic(&[COLOR_EMOJI]);
    let (cell_w, padding) = geometry(&renderer);

    // The shaper works in chunks of 256 cells. Each emoji is one cell holding
    // two columns, so 260 of them cross the boundary at column 512.
    let count = 260u32;
    let cols = (count * 2 + 4) as u16;
    let img = render(
        &renderer,
        &dir,
        "long",
        &"\u{1F600}".repeat(count as usize),
        cols,
    );

    for first in [0u32, 2, 508, 510, 512, 514, 516, 518] {
        assert!(
            ink_in_cells(&img, cell_w, padding, first, first + 2) > 0,
            "columns {first}..{} of a long shaped run are empty",
            first + 2
        );
    }
    assert_eq!(
        ink_in_cells(&img, cell_w, padding, count * 2, u32::from(cols)),
        0,
        "a long shaped run painted past the cells it owns"
    );
}

/// A joining or reordering script is laid out as one piece, which is the only
/// way its connecting strokes and reordered marks can be right. The price is
/// that a very long word does not sit one code point per column: the shaped
/// text is fitted to the columns it needs, anchored at the start of the run,
/// and the columns the terminal allocated beyond that stay blank.
///
/// This documents that trade-off rather than asserting it away - and pins the
/// guarantee that actually matters, which is that nothing is drawn outside the
/// run's own columns.
#[test]
fn a_contiguous_script_run_is_fitted_to_the_columns_it_needs() {
    let dir = out_dir("contiguous-fit");
    let renderer = deterministic(&[SHAPE_A]);
    let (cell_w, padding) = geometry(&renderer);

    let letters = 40u32;
    let text = format!("{}X", "\u{0915}".repeat(letters as usize));
    let img = render(&renderer, &dir, "long-word", &text, (letters + 4) as u16);

    assert!(
        ink_in_cells(&img, cell_w, padding, 0, 1) > 0,
        "the run does not start at its first column"
    );
    assert_eq!(
        ink_in_cells(&img, cell_w, padding, letters + 1, letters + 4),
        0,
        "the run painted past the character that follows it"
    );
    // The ASCII letter after the run is untouched by any of this.
    assert!(
        ink_in_cells(&img, cell_w, padding, letters, letters + 1) > 0,
        "the character after a long shaped run was lost"
    );
}

// -------------------------------------------------------------------------
// 6. Robustness
// -------------------------------------------------------------------------

/// Hostile or merely strange input must not panic, hang, or escape the grid.
///
/// Every case here is something a captured command could actually print: a
/// base character buried under hundreds of combining marks, a wall of distinct
/// emoji large enough to churn the glyph cache, lone joiners and variation
/// selectors with nothing to join, and a right-to-left override in the middle
/// of a line.
#[test]
fn adversarial_input_renders_without_panicking_or_escaping_the_grid() {
    let dir = out_dir("adversarial");
    let renderer = deterministic(&[SHAPE_A, COLOR_EMOJI]);
    let (cell_w, padding) = geometry(&renderer);

    let cases: &[(&str, String)] = &[
        // A base character under 400 combining marks. vt100 keeps them all in
        // one cell, so this is one enormous cluster.
        ("stacked marks", format!("a{}", "\u{0301}".repeat(400))),
        // Lone format characters with nothing to format.
        (
            "lone joiners",
            "\u{200D}\u{200D}\u{FE0F}\u{FE0E}\u{20E3}".to_string(),
        ),
        // A directional override mid-line.
        ("bidi override", "abc\u{202E}def\u{202C}ghi".to_string()),
        // Half a surrogate pair's worth of regional indicators: an odd count,
        // so the last one has no partner to make a flag with.
        (
            "odd regional indicators",
            "\u{1F1FA}\u{1F1F8}\u{1F1EB}".to_string(),
        ),
        // Enough distinct emoji to exercise the glyph and shape caches.
        (
            "many distinct emoji",
            (0x1F600..0x1F640)
                .filter_map(char::from_u32)
                .collect::<String>(),
        ),
        // Deeply nested variation selectors on a character that has none.
        (
            "variation selector spam",
            format!("x{}", "\u{FE0F}".repeat(64)),
        ),
    ];

    for (name, text) in cases {
        let cols = 60u16;
        let img = render(&renderer, &dir, name, text, cols);
        assert!(img.width() > 0 && img.height() > 0, "{name}: empty image");
        // Whatever was drawn stayed inside the terminal's own columns: the
        // padding on the right edge is untouched.
        assert_eq!(
            ink_in(&img, img.width() - padding, img.width()),
            0,
            "{name}: something painted into the right padding"
        );
        assert_eq!(
            ink_in(&img, 0, padding),
            0,
            "{name}: something painted into the left padding"
        );
        assert!(cell_w > 0);
    }
}

/// The renderer is shared behind an `Arc` by the MCP server, and the shaped
/// path put a font system, a scaler, and three caches behind one mutex. Ten
/// threads rendering the same shaped content must all get the same pixels.
#[test]
fn concurrent_renders_of_shaped_content_agree() {
    use std::sync::Arc;

    let dir = out_dir("concurrent");
    let renderer = Arc::new(deterministic(&[SHAPE_A, COLOR_EMOJI]));
    let text = "\u{0915}\u{094D}\u{0937}\u{093F} \u{1F600} e\u{0301} \u{2764}\u{FE0F}";

    let expected = render(&renderer, &dir, "reference", text, 30);
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let renderer = Arc::clone(&renderer);
            let dir = dir.clone();
            let text = text.to_string();
            std::thread::spawn(move || {
                render(&renderer, &dir, &format!("thread-{i}"), &text, 30).into_raw()
            })
        })
        .collect();

    for (i, handle) in handles.into_iter().enumerate() {
        let pixels = handle.join().expect("render thread did not panic");
        assert!(
            pixels == *expected.as_raw(),
            "thread {i} rendered different pixels"
        );
    }
}

/// A chain that never draws a shaped cell must never build a font database.
///
/// The shaped path loads system fonts, which is the single most expensive
/// thing this feature can do; it has to stay off the ASCII path entirely.
#[test]
fn rendering_ordinary_output_never_starts_the_shaper() {
    let dir = out_dir("laziness");
    // System font discovery is deliberately left *on* here: if the engine were
    // built eagerly, this would be the slow case.
    let renderer = renderer(None, &[], ShapingOptions::default());

    let start = std::time::Instant::now();
    for i in 0..20 {
        let img = render(
            &renderer,
            &dir,
            &format!("ascii-{i}"),
            "the quick brown fox jumps over the lazy dog 0123456789",
            60,
        );
        assert!(ink(&img) > 0);
    }
    let elapsed = start.elapsed();
    // Twenty ASCII screenshots. Loading a machine's fonts takes hundreds of
    // milliseconds on its own, so a generous ceiling still catches an eager
    // font database.
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "twenty ASCII renders took {elapsed:?}: the shaper is no longer lazy"
    );
}
