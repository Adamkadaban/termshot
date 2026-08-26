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

The bootstrapped `config.toml` selects `dark`, which uses the bundled JetBrains
Mono. termshot also writes an `adamkadaban` *example* user theme; it points at
MonoLisa (a commercial font), so edit or remove its `font` keys before selecting
it.

Select a theme per run with `--theme <name>` (CLI) or the `theme` parameter
(MCP), or set `default_theme` in the config file. List everything available,
with each marked `built-in` or `user` and the active default flagged:

```bash
termshot themes
```

## User themes

User themes live in `~/.config/termshot/themes/`, one `.toml` per theme. A theme
with the same name as a built-in overrides it. You can also define themes inline
in `config.toml` under `[themes.<name>]` (same fields as a theme file).

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
```

## Fonts

The JetBrains Mono font is compiled into the binary, so no external font files
are needed. To use a different font, in order of precedence:

- Per run: `--font <path>` and `--font-bold <path>` on the CLI.
- Per theme: the `font` and `font_bold` keys in a theme definition. Relative
  paths are resolved against the theme file's directory.
- Globally: `font_path` in `config.toml`, or the `TERMSHOT_FONT_PATH`
  environment variable.

When no bold font is supplied, bold text is rendered as faux bold (the regular
glyph drawn twice with a one-pixel offset) and standard palette colors use their
bright variants, matching how most terminal emulators show bold.

### Glyph coverage

There is no font fallback chain: every glyph comes from the one configured font.
Codepoints it does not cover - CJK, emoji, and some box-drawing or Powerline
glyphs used by tools like `bat`, `btop`, and `eza` - render as `.notdef` boxes.
If you capture such output, point `font` / `font_bold` (per theme) or
`font_path` (globally) at a face with wider coverage, such as a Nerd Font
build of your favorite monospace family.

Adjust the text size with the top-level `font_size` config key (default `16.0`)
or `TERMSHOT_FONT_SIZE`.
