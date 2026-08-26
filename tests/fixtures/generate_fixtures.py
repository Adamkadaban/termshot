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
"""

from pathlib import Path

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.subset import Subsetter
from fontTools.ttLib import TTFont

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


if __name__ == "__main__":
    build_limited_ascii()
    build_cjk_fallback()
    print(f"wrote fixtures to {FIXTURES}")
