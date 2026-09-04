# Themes

A theme sets the terminal foreground, background, and 16-color ANSI palette,
and may point at its own fonts.

## Built-in themes

```
dark, catppuccin-mocha, catppuccin-latte, catppuccin-frappe,
catppuccin-macchiato, solarized-dark, solarized-light, dracula, nord,
gruvbox-dark, tokyo-night
```

All eleven use the bundled JetBrains Mono, so they work with no fonts
installed. The shipped default is `dark`.

Select one with `--theme <name>` (CLI) or `theme` (MCP), or set `default_theme`
in the config. `termshot themes` lists everything available, marking each
`built-in` or `user` and flagging the active default.

## User themes

No theme files are installed for you: `~/.config/termshot/themes/` is created
empty on first run. Each `.toml` file there is one theme, named after the file
and listed as `user`; a user theme may override a built-in of the same name.
Themes can also be defined inline in `config.toml` under `[themes.<name>]`,
using the same fields.

```toml
# ~/.config/termshot/themes/mytheme.toml
foreground = "#e6edf3"
background = "#0d1117"
# ANSI 16-color palette, all values #RRGGBB
# [0]  black         [1]  red            [2]  green         [3]  yellow
# [4]  blue          [5]  magenta        [6]  cyan          [7]  white
# [8]  bright black  [9]  bright red     [10] bright green  [11] bright yellow
# [12] bright blue   [13] bright magenta [14] bright cyan   [15] bright white
palette = [
  "#0d1117", "#ff7b72", "#3fb950", "#d29922",
  "#58a6ff", "#bc8cff", "#39c5cf", "#b1bac4",
  "#484f58", "#ffa198", "#56d364", "#e3b341",
  "#79c0ff", "#d2a8ff", "#56d4dd", "#f0f6fc",
]
# Optional per-theme fonts:
font = "~/.local/share/fonts/MyMono-Regular.otf"
font_bold = "~/.local/share/fonts/MyMono-Bold.otf"
# Extra faces searched for glyphs the fonts above lack (CJK, Powerline, Nerd):
fallback_fonts = ["/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc"]
```

## Font resolution

A font is chosen in this order of precedence:

1. `--font` / `--font-bold` for one run,
2. the theme's `font` / `font_bold`,
3. `font_path` in `config.toml`, or `TERMSHOT_FONT_PATH`,
4. the bundled JetBrains Mono.

Set the size with the `font_size` config key (default `16.0`) or
`TERMSHOT_FONT_SIZE`. With no bold font, bold text is rendered as faux bold and
standard palette colors use their bright variants, as in most terminals.

Paths in `font`, `font_bold`, and `fallback_fonts` expand `~` and resolve
relative entries against the theme file's directory. An entry that is missing or
unusable is skipped with a warning. Font collections (`.ttc`) are supported:
every face in the collection is available to the shaped text path below, at the
index the file declares.

## Glyph fallback

Ordinary output - ASCII, box drawing, Latin, Greek, Cyrillic, and anything else
that is correct one character at a time - is drawn by the first font in this
chain that has a real glyph:

1. the theme's `font` (or `font_bold` for a bold cell),
2. the bundled JetBrains Mono,
3. each file in `fallback_fonts`, in order.

Step 2 is why a font with no box drawing glyphs still captures `bat`, `btop`, or
`eza` cleanly. Fallback glyphs are scaled to the primary font's advance, so the
monospace grid is untouched.

Cells that cannot be drawn one character at a time take a second, shaped path:
combining marks, emoji (including variation selectors, keycaps, skin tones, and
ZWJ sequences), joining scripts such as Arabic, reordering scripts such as
Devanagari, and any character no font above covers. Those runs are shaped with
the same chain in the same order, and only if none of those fonts covers the run
are the machine's installed fonts searched. A cluster asking for emoji
presentation prefers a color emoji font, so **color-bitmap and COLR emoji fonts
are now supported** - configure one in `fallback_fonts` to pin it, or let system
discovery find the usual ones (Noto Color Emoji, Apple Color Emoji, Segoe UI
Emoji).

The terminal grid stays in charge either way: shaped glyphs are fitted to the
cells the terminal allocated and clipped to them, so nothing is drawn across a
style change or a redaction block. Shaping never invents a substitute glyph
either - a character it cannot find anywhere falls back to the per-character
path, which draws the primary font's `.notdef` box as it always did. Add a
covering face to `fallback_fonts` to fix one for good.

Set `TERMSHOT_SYSTEM_FONTS=0` to keep rendering to the fonts your configuration
names, which makes screenshots reproducible across machines;
`TERMSHOT_UNICODE_SHAPING=0` turns the shaped path off completely.

Font files are binary parser inputs. MCP requests cannot provide font paths,
but an operator-controlled theme, fontconfig file, or system font directory
can. Hardened multi-user deployments should set `TERMSHOT_SYSTEM_FONTS=0` and
configure only immutable, deployment-owned font files.

### Known limitation: emoji sequence width

The terminal core stores one Unicode *scalar cluster* per cell, not one
grapheme cluster, so a ZWJ sequence (`👨‍👩‍👧`) or an emoji plus a skin tone
modifier (`👍🏽`) is split across several cells and given several cells' worth of
width. termshot reassembles the sequence and draws the one correct picture at
the width it actually needs, so the rest of the line is unaffected - but the
extra columns the terminal reserved are left blank. Flags (`🇺🇸`) come out at the
right width by accident. Fixing the width properly needs grapheme-cluster
support in the terminal core.
