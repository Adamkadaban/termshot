# Themes

A theme sets the terminal foreground, background, and 16-color ANSI palette
used when rendering, and may point at its own regular and bold fonts.

## Built-in themes

Eleven themes are compiled into the binary:

```
dark, catppuccin-mocha, catppuccin-latte, catppuccin-frappe,
catppuccin-macchiato, solarized-dark, solarized-light, dracula, nord,
gruvbox-dark, tokyo-night
```

They all use the bundled JetBrains Mono, so they work with no fonts installed.
The shipped default is `dark`. No theme files are installed for you: the
`themes/` directory is created empty on first run and is yours to fill.

Select a theme per run with `--theme <name>` (CLI) or the `theme` parameter
(MCP), or set `default_theme` in the config file. List everything available,
with each marked `built-in` or `user` and the active default flagged:

```bash
termshot themes
```

## User themes

Create any number of your own themes in `~/.config/termshot/themes/`, one
`.toml` per theme - the file name (without the extension) is the theme name, and
each may point at its own fonts. They are picked up automatically and listed by
`termshot themes` as `user`. A theme with the same name as a built-in overrides
it. You can also define themes inline in `config.toml` under `[themes.<name>]`
(same fields as a theme file).

## Theme format

Each theme sets a foreground, a background, and a 16-color palette. Colors are
`#RRGGBB` hex strings.

```toml
# ~/.config/termshot/themes/mytheme.toml
foreground = "#e6edf3"
background = "#0d1117"
# ANSI 16-color palette
# [0]  black    [1]  red      [2]  green    [3]  yellow
# [4]  blue     [5]  magenta  [6]  cyan     [7]  white
# [8]  bright black  [9]  bright red    [10] bright green  [11] bright yellow
# [12] bright blue   [13] bright magenta [14] bright cyan   [15] bright white
palette = [
  "#0d1117", "#ff7b72", "#3fb950", "#d29922",
  "#58a6ff", "#bc8cff", "#39c5cf", "#b1bac4",
  "#484f58", "#ffa198", "#56d364", "#e3b341",
  "#79c0ff", "#d2a8ff", "#56d4dd", "#f0f6fc",
]
# Optional per-theme fonts:
# font = "/path/to/Regular.ttf"
# font_bold = "/path/to/Bold.ttf"
# Extra faces searched for glyphs the fonts above lack:
# fallback_fonts = ["/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc"]
```

## Fonts

The JetBrains Mono font is compiled into the binary, so no external font files
are needed. To use a different font, in order of precedence:

- Per run: `--font <path>` and `--font-bold <path>` on the CLI.
- Per theme: the `font` and `font_bold` keys in a theme definition. Relative
  paths are resolved against the theme file's directory.
- Globally: `font_path` in `config.toml`, or the `TERMSHOT_FONT_PATH`
  environment variable.

A theme's fonts follow the theme wherever it is selected: the CLI's `--theme`
and the MCP tools' `theme` parameter both render with that theme's `font`,
`font_bold`, and `fallback_fonts`. The renderer builds one font chain per theme
up front, so switching themes between screenshots costs nothing.

When no bold font is supplied, bold text is rendered as faux bold (the regular
glyph drawn twice with a one-pixel offset) and standard palette colors use their
bright variants, matching how most terminal emulators show bold.

### Glyph coverage and font fallback

Every character is drawn by the first font that actually has a glyph for it:

1. the theme's `font` (or `font_bold` for a bold cell),
2. the **bundled JetBrains Mono**, always tried next and never configured by
   hand,
3. each file listed in the theme's `fallback_fonts`, in order.

A character mapped to `.notdef` does not count as coverage, so a font that would
draw a tofu box never wins the lookup. If no font in the chain has the glyph,
the character keeps the primary font's normal missing-glyph rendering.

Step 2 is what makes a font that ships no box drawing glyphs usable for
capturing `bat`, `btop`, or `eza`: the frames come from JetBrains Mono while the
text stays in your font. Fallback glyphs are scaled to the primary
font's advance and drawn on the primary baseline, so the monospace grid is
untouched and box drawing runs still tile without gaps. Double-width characters
keep their continuation cell.

For scripts neither font covers - CJK and Powerline/Nerd Font glyphs - list
a covering face in the theme:

```toml
font = "~/.local/share/fonts/MyMono-Regular.otf"
font_bold = "~/.local/share/fonts/MyMono-Bold.otf"
fallback_fonts = [
  "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
]
```

`fallback_fonts` entries are resolved exactly like `font` and `font_bold`: `~`
expands to your home directory and relative paths resolve against the theme
file's directory. An entry that is missing, or is not a font at all, is skipped
with a warning. Fallback fonts must contain outline glyphs that `fontdue` can
rasterize. Color-bitmap emoji fonts such as Noto Color Emoji and Apple Color
Emoji are not supported; monochrome outline emoji fonts may work. Rendering
continues with the rest of the chain when a configured fallback is skipped.

Adjust the text size with the top-level `font_size` config key (default `16.0`)
or `TERMSHOT_FONT_SIZE`.
