# Themes

A theme sets the terminal foreground, background, and 16-color ANSI palette
used when rendering, and may point at its own fonts.

## Built-in themes

Eleven themes are compiled into the binary:

```
dark, catppuccin-mocha, catppuccin-latte, catppuccin-frappe,
catppuccin-macchiato, solarized-dark, solarized-light, dracula, nord,
gruvbox-dark, tokyo-night
```

They all use the bundled JetBrains Mono, so they work with no fonts installed.
The shipped default is `dark`.

Select a theme with `--theme <name>` (CLI) or the `theme` parameter (MCP), or
set `default_theme` in the config. `termshot themes` lists everything available,
marking each `built-in` or `user` and flagging the active default.

## User themes

No theme files are installed for you: `~/.config/termshot/themes/` is created
empty on first run and is yours to fill. Each `.toml` file there is one theme,
named after the file, picked up automatically, and listed as `user`. A user
theme with the same name as a built-in overrides it. Themes can also be defined
inline in `config.toml` under `[themes.<name>]`, using the same fields.

## Theme format

Colors are `#RRGGBB` hex strings.

```toml
# ~/.config/termshot/themes/mytheme.toml
foreground = "#e6edf3"
background = "#0d1117"
# ANSI 16-color palette
# [0]  black    [1]  red      [2]  green    [3]  yellow
# [4]  blue     [5]  magenta  [6]  cyan     [7]  white
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
# Extra faces searched for glyphs the fonts above lack:
fallback_fonts = ["/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc"]
```

## Fonts

JetBrains Mono is compiled into the binary, so no external font files are
needed. To use a different font, in order of precedence:

- Per run: `--font <path>` and `--font-bold <path>`.
- Per theme: the `font` and `font_bold` keys.
- Globally: `font_path` in `config.toml`, or `TERMSHOT_FONT_PATH`.

A theme's fonts follow the theme wherever it is selected, on the CLI and over
MCP. Set the text size with the `font_size` config key (default `16.0`) or
`TERMSHOT_FONT_SIZE`.

With no bold font, bold text is rendered as faux bold and standard palette
colors use their bright variants, matching most terminal emulators.

Font paths in `font`, `font_bold`, and `fallback_fonts` all expand `~` and
resolve relative entries against the theme file's directory. An entry that is
missing or is not a usable font is skipped with a warning. Fonts must contain
outline glyphs: color-bitmap emoji fonts such as Noto Color Emoji and Apple
Color Emoji are not supported, though monochrome outline emoji fonts may work.

### Glyph coverage and font fallback

Every character is drawn by the first font in the chain that has a real glyph
for it:

1. the theme's `font` (or `font_bold` for a bold cell),
2. the bundled JetBrains Mono, always tried next and never configured by hand,
3. each file in the theme's `fallback_fonts`, in order.

Step 2 is what makes a font with no box drawing glyphs usable for capturing
`bat`, `btop`, or `eza`: the frames come from JetBrains Mono while the text
stays in your font. Fallback glyphs are scaled to the primary font's advance, so
the monospace grid is untouched.

For scripts neither font covers, such as CJK and Powerline or Nerd Font glyphs,
list a covering face in the theme:

```toml
fallback_fonts = [
  "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
]
```
