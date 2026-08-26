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
unusable is skipped with a warning. Fonts must contain outline glyphs:
color-bitmap emoji fonts (Noto Color Emoji, Apple Color Emoji) are not
supported.

## Glyph fallback

Each character is drawn by the first font in this chain that has a real glyph:

1. the theme's `font` (or `font_bold` for a bold cell),
2. the bundled JetBrains Mono,
3. each file in `fallback_fonts`, in order.

Step 2 is why a font with no box drawing glyphs still captures `bat`, `btop`, or
`eza` cleanly. Fallback glyphs are scaled to the primary font's advance, so the
monospace grid is untouched. Characters no font in the chain covers are drawn as
`.notdef` boxes; add a covering face to `fallback_fonts`.
