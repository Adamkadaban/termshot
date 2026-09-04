#!/usr/bin/env python3
"""Regenerate the font fixtures used by the font-fallback tests.

Run from the repository root with fontTools installed:

    python3 tests/fixtures/generate_fixtures.py

Two fixtures are produced, both committed to the repo so tests need no
network access or system fonts:

* ``limited-ascii.ttf`` - the bundled JetBrains Mono subset down to printable
  ASCII, renamed, and re-spaced to a 0.64 em advance. It stands in for a
  real-world primary font such as MonoLisa, which has no box drawing, arrow, or
  symbol glyphs *and* a wider cell than JetBrains Mono (0.64 em against
  0.6 em), so tests exercise both the fallback chain and the metric mismatch
  without shipping a commercial font.
* ``cjk-fallback.ttf`` - a synthetic font built from scratch that maps a single
  CJK character (U+4E2D) to a plain filled rectangle. It stands in for a
  user-configured fallback font such as WenQuanYi Zen Hei.
* ``shape-a.ttf`` / ``shape-b.ttf`` - two synthetic fonts covering the same
  characters with visibly different outlines (a bar in the left half of the
  cell against one in the right half), so the shaped-text tests can prove which
  explicitly configured font was actually used.
* ``color-emoji.ttf`` - a synthetic COLRv0 font: two emoji code points, each
  drawn as two layers in two palette colors, so color glyph rendering can be
  tested without depending on whatever emoji font the host happens to have.
* ``collection.ttc`` - a two-face font collection whose faces cover different
  code points, so the tests can prove `fontdb` face indices survive.

All four synthetic fonts are original outlines authored for this repository and
carry no third-party font data; they are covered by the repository's own MIT
license.
"""

from pathlib import Path

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.subset import Subsetter
from fontTools.ttLib import TTCollection, TTFont

FIXTURES = Path(__file__).resolve().parent
REPO = FIXTURES.parent.parent

# Printable ASCII only: deliberately no box drawing (U+2500..), no check mark
# (U+2713), no Greek (U+03BB), no CJK.
ASCII_TEXT = "".join(chr(c) for c in range(0x20, 0x7F))


def build_limited_ascii() -> None:
    font = TTFont(REPO / "fonts" / "JetBrainsMono-Regular.ttf")
    subsetter = Subsetter()
    subsetter.populate(text=ASCII_TEXT)
    subsetter.subset(font)

    # Rename so the fixture is never mistaken for real JetBrains Mono.
    name = font["name"]
    for record in name.names:
        if record.nameID in (1, 3, 4, 6, 16, 21):
            value = "Termshot Test ASCII"
            if record.nameID == 6:
                value = "TermshotTestASCII"
            record.string = value.encode(
                "utf_16_be" if record.platformID == 3 else "latin-1"
            )
    # Widen the cell to MonoLisa's 0.64 em advance while leaving the outlines
    # alone, so the fixture reproduces the metric mismatch that makes an
    # unscaled fallback glyph too narrow for the cell.
    upm = font["head"].unitsPerEm
    wide_advance = round(upm * 0.64)
    hmtx = font["hmtx"]
    for name in font.getGlyphOrder():
        _, lsb = hmtx[name]
        hmtx[name] = (wide_advance, lsb)

    font.save(FIXTURES / "limited-ascii.ttf")


def build_cjk_fallback() -> None:
    upm = 1000
    glyph_order = [".notdef", "cjk_zhong"]

    pen = TTGlyphPen(None)
    pen.moveTo((100, 0))
    pen.lineTo((100, 700))
    pen.lineTo((900, 700))
    pen.lineTo((900, 0))
    pen.closePath()
    filled = pen.glyph()

    empty = TTGlyphPen(None).glyph()

    builder = FontBuilder(upm, isTTF=True)
    builder.setupGlyphOrder(glyph_order)
    builder.setupCharacterMap({0x4E2D: "cjk_zhong"})
    builder.setupGlyf({".notdef": empty, "cjk_zhong": filled})
    builder.setupHorizontalMetrics({".notdef": (upm, 0), "cjk_zhong": (upm, 100)})
    builder.setupHorizontalHeader(ascent=800, descent=-200)
    builder.setupNameTable(
        {
            "familyName": "Termshot Test CJK",
            "styleName": "Regular",
            "psName": "TermshotTestCJK-Regular",
            "fullName": "Termshot Test CJK Regular",
            "version": "1.0",
        }
    )
    builder.setupOS2(sTypoAscender=800, sTypoDescender=-200, usWinAscent=800, usWinDescent=200)
    builder.setupPost()
    builder.save(FIXTURES / "cjk-fallback.ttf")


UPM = 1000
# Every synthetic fixture uses the same 0.6 em advance as JetBrains Mono, so a
# glyph lands in its cell without the fit-to-cell scaling having to do anything
# interesting - the tests are about which font was used, not about metrics.
ADVANCE = 600
# A double-width cell is two columns wide, so emoji get a 1.2 em advance.
WIDE_ADVANCE = 1200


def _bar(x0: int, x1: int, y0: int = 0, y1: int = 700):
    """A filled rectangle, the only outline any synthetic fixture needs."""
    pen = TTGlyphPen(None)
    pen.moveTo((x0, y0))
    pen.lineTo((x0, y1))
    pen.lineTo((x1, y1))
    pen.lineTo((x1, y0))
    pen.closePath()
    return pen.glyph()


def _build(path: Path, family: str, glyphs: dict, cmap: dict, advances: dict,
           colr=None, palettes=None, save=True):
    """Assemble one synthetic TrueType font from filled rectangles.

    ``glyphs`` maps a glyph name to ``(outline, x_min)``. The left side bearing
    has to match the outline's own ``xMin``: rasterizers shift a glyph by the
    difference, so a font that reports ``lsb = 0`` for a bar starting at 360
    units draws it at the left edge of the cell instead - which would make the
    two shape fixtures indistinguishable.
    """
    order = [".notdef", *glyphs]
    builder = FontBuilder(UPM, isTTF=True)
    builder.setupGlyphOrder(order)
    builder.setupCharacterMap(cmap)
    builder.setupGlyf({".notdef": TTGlyphPen(None).glyph(), **{n: g for n, (g, _) in glyphs.items()}})
    builder.setupHorizontalMetrics(
        {
            ".notdef": (ADVANCE, 0),
            **{n: (advances.get(n, ADVANCE), x_min) for n, (_, x_min) in glyphs.items()},
        }
    )
    builder.setupHorizontalHeader(ascent=800, descent=-200)
    builder.setupNameTable(
        {
            "familyName": family,
            "styleName": "Regular",
            "psName": family.replace(" ", "") + "-Regular",
            "fullName": f"{family} Regular",
            "version": "1.0",
        }
    )
    builder.setupOS2(
        sTypoAscender=800, sTypoDescender=-200, usWinAscent=800, usWinDescent=200
    )
    builder.setupPost()
    if palettes is not None:
        builder.setupCPAL(palettes)
    if colr is not None:
        builder.setupCOLR(colr)
    if save:
        builder.save(path)
    return builder.font


def build_shape_pair() -> None:
    """Two fonts covering the same characters with distinguishable outlines.

    Both cover Devanagari KA (a script that always needs a shaper, so the
    shaped path picks the font rather than fontdue) and one private-use
    character (which fontdue covers, so the per-scalar path picks it). ``A``
    inks the left third of the cell, ``B`` the right third, so a test can tell
    from the pixels alone which font won.
    """
    for family, path, x0, x1 in (
        ("Termshot Shape A", "shape-a.ttf", 60, 240),
        ("Termshot Shape B", "shape-b.ttf", 360, 540),
    ):
        glyphs = {name: (_bar(x0, x1), x0) for name in ("ka", "pua", "heart")}
        # U+2764 is the *text* presentation heart: a monochrome font covering
        # it is what proves a VS15 sequence stays off the color emoji path.
        cmap = {0x0915: "ka", 0xE010: "pua", 0x2764: "heart"}
        if family.endswith("B"):
            # Only B covers this one, so precedence tests have a control.
            glyphs["puab"] = (_bar(x0, x1), x0)
            cmap[0xE011] = "puab"
        _build(FIXTURES / path, family, glyphs, cmap, {})


def build_color_emoji() -> None:
    """A COLRv0 font: each emoji is two layers in two palette colors.

    The base glyphs are also real outlines, so a monochrome renderer draws
    *something* - which is exactly what lets a test tell the color path apart
    from the fallback one.
    """
    glyphs = {}
    for base in ("grin", "thumb", "heart"):
        glyphs[base] = (_bar(100, 1100, 0, 700), 100)
        glyphs[f"{base}.l0"] = (_bar(100, 1100, 0, 700), 100)
        glyphs[f"{base}.l1"] = (_bar(300, 900, 150, 550), 300)
    # U+2764 defaults to *text* presentation, so it only reaches this font when
    # a variation selector asks for emoji - which is exactly what makes it a
    # useful test of presentation selection.
    cmap = {0x1F600: "grin", 0x1F44D: "thumb", 0x2764: "heart"}
    advances = {name: WIDE_ADVANCE for name in glyphs}
    colr = {
        "grin": [("grin.l0", 0), ("grin.l1", 1)],
        "thumb": [("thumb.l0", 0), ("thumb.l1", 1)],
        "heart": [("heart.l0", 0), ("heart.l1", 1)],
    }
    # Palette entries are (red, green, blue, alpha) floats in 0..1. Two loud,
    # unmistakable colors that no theme in the repo uses.
    palettes = [[(1.0, 0.0, 0.0, 1.0), (0.0, 0.0, 1.0, 1.0)]]
    _build(
        FIXTURES / "color-emoji.ttf",
        "Termshot Color Emoji",
        glyphs,
        cmap,
        advances,
        colr=colr,
        palettes=palettes,
    )


def build_collection() -> None:
    """A two-face collection whose faces cover different code points.

    fontdue only ever loads face 0 of a collection, so a character that exists
    only in face 1 proves the shaped path kept the face index fontdb reported.
    """
    face0 = _build(
        FIXTURES / "unused-face0.ttf",
        "Termshot Collection Zero",
        {"zero": (_bar(60, 240), 60)},
        {0xE020: "zero"},
        {},
        save=False,
    )
    face1 = _build(
        FIXTURES / "unused-face1.ttf",
        "Termshot Collection One",
        {"one": (_bar(360, 540), 360)},
        {0xE021: "one"},
        {},
        save=False,
    )
    collection = TTCollection()
    collection.fonts = [face0, face1]
    collection.save(str(FIXTURES / "collection.ttc"))


if __name__ == "__main__":
    build_limited_ascii()
    build_cjk_fallback()
    build_shape_pair()
    build_color_emoji()
    build_collection()
    print(f"wrote fixtures to {FIXTURES}")
