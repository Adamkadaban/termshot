# Configuration

On first run termshot bootstraps `~/.config/termshot/` with a starter
`config.toml` and empty `themes/` and `rules/` directories:

```
~/.config/termshot/
├── config.toml
├── rules/     # every .toml / .yaml file here is loaded as redaction rules
└── themes/    # every .toml file here is a theme, named after the file
```

Point at a different file with `--config <path>`. A commented example ships as
`termshot.example.toml`. Packages do not write a user config, so upgrades never
overwrite your settings.

## Top-level keys

| Key | Default | Description |
| --- | --- | --- |
| `output_dir` | `/tmp/termshot` | Directory where screenshots are written. |
| `font_size` | `16.0` | Text size in pixels. |
| `cols` | `120` | Default terminal width in columns. |
| `rows` | `40` | Default terminal *viewport* height in rows. See [Viewport rows vs retained output](#viewport-rows-vs-retained-output). |
| `max_scrollback_lines` | `10000` | How many scrolled-off lines each capture keeps, so a screenshot can show output that never fit in the viewport. At least 1, capped at 60,000, and capped again per capture by the terminal's width (see below). |
| `timeout_secs` | `30` | Default command timeout in seconds. |
| `default_theme` | `dark` | Theme used when `--theme` is not given. See [themes.md](./themes.md). |
| `font_path` | unset | Monospace font overriding the embedded JetBrains Mono. |
| `shell` | `$SHELL` | Shell used to execute commands. `exec` runs it as a login + interactive shell (`-l -i`), so your profile, prompt, aliases, and `PATH` apply; `--no-prompt` runs it non-interactively (`-c`). |
| `embed_description` | `true` | Embed the terminal text (redacted, when redaction ran) in each PNG's UTF-8 `Description` metadata for screen readers. Disable per run with `--no-description`. Not exposed over MCP. |

### Viewport rows vs retained output

`rows` and `cols` are the size of the terminal a command *runs in*: they decide
where long lines wrap and what a full-screen program is told the window is. They
are not a limit on what a screenshot shows.

Everything that scrolls off the top is retained and rendered, so
`termshot exec --rows 10 'seq 1 200'` produces an image with all 200 lines.
`max_scrollback_lines` bounds that retention so a runaway command cannot grow
the buffer without limit; when a command overruns it, termshot prints a warning
(and the MCP result carries one) rather than quietly dropping the oldest output.

A line count is not a memory limit on its own - the same 10,000 lines cost four
times more in a 500-column terminal than in a 120-column one - so each capture
also holds to a budget of 2,000,000 retained cells. At the default 120 columns
that is over 16,000 lines, well past `max_scrollback_lines`; at 500 columns it
caps retention at about 3,900 lines, and the truncation warning reports the
figure that actually applied.

Use `--head-lines N` / `--tail-lines N` (`head_lines` / `tail_lines` over MCP)
to render only one end. The two are mutually exclusive.

`--head-lines N` always means the first N lines of the output, even when the
command went on to print far more than the scrollback can hold: those lines are
captured as they scroll past rather than filtered out of what survived. It is
therefore both the cheapest and the most reliable way to look at the start of a
very long run.

`max_scrollback_lines` does not apply to it. That setting decides how much of
the *end* of a run is retained once the terminal starts evicting rows, which is
the opposite of what a head selection wants, so the head is streamed into a
staging area bounded only by the 2,000,000-cell budget. `--head-lines 10` on a
1,000-line command returns lines 1-10 with `max_scrollback_lines = 1` exactly as
it does with the default 10,000.

Very tall captures also hit a rendering ceiling: 64 megapixels for the image,
and 320 MB of image buffers held at once - window chrome with a drop shadow
needs two of them, so it reaches the ceiling sooner than a bare screenshot.
Roughly, that is several hundred lines at 120 columns. Past it, termshot fails
with an error pointing at `--head-lines` / `--tail-lines` instead of attempting
an allocation the machine cannot satisfy.

Full-screen (alternate-screen) programs such as `vim`, `htop` and `less`
repaint the whole viewport, so only their active screen is captured - the
scrollback from before they started is not what they are showing. `--head-lines
N` is the exception: when the command printed N lines before the program
started, those lines are the head, and they are returned instead of the screen
it painted - even when the last of them and the program's startup arrived
together. A shorter prefix falls back to the active screen.

## `[chrome]`

| Key | Default | Description |
| --- | --- | --- |
| `enabled` | `false` | Draw chrome by default. |
| `preset` | `none` | `none`, `minimal`, `gnome`, `macos`, or `report`. |
| `title` | unset | Default title bar text (the first command is used when unset). |
| `timestamp` | `false` | Draw a UTC timestamp watermark below the content. |
| `shadow` | `false` | Draw a drop shadow behind the frame. |
| `radius` | `14` | Corner radius in pixels. |
| `rounded` | `true` | Soft rounded corners. With chrome the window frame is rounded; without chrome the terminal image gets rounded corners on a transparent background. |
| `outer_padding` | `0` | Extra padding between frame and terminal content. |
| `title_bar_height` | `34` | Title bar height in pixels (ignored by `minimal`). |

Presets: `none` is the raw terminal image, `minimal` adds padding with no title
bar, `gnome` and `macos` mimic those window title bars, and `report` is a clean
framed style for documents.

## `[redaction]`

| Key | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Master switch. `false` disables all redaction: no rule runs, and an explicit `--redact` / `redact: true` fails with an error. |
| `auto` | `false` | Redact every screenshot's image without an explicit flag. |
| `color` | `#d41919` | Default block color as `#RRGGBB`. |
| `label_color` | black | Label text color as `#RRGGBB`. |
| `rules_path` | unset | Additional directory of `.toml`/`.yaml` rule files, layered on top of `~/.config/termshot/rules/`. |
| `[[redaction.rules]]` | none | Inline rule overrides and additions. |

See [redaction.md](./redaction.md) for the rule format and partial redaction.

## `[themes.<name>]`

Inline theme definitions using the same fields as a user theme file
(`foreground`, `background`, `palette`, optional `font` / `font_bold` /
`fallback_fonts`). See [themes.md](./themes.md).

## Example

```toml
output_dir = "/tmp/termshot"
font_size = 16.0
cols = 120
rows = 40
max_scrollback_lines = 10000
timeout_secs = 30
default_theme = "dark"
embed_description = true

[chrome]
enabled = false
preset = "none"
shadow = false
radius = 14
rounded = true
outer_padding = 0
title_bar_height = 34

[redaction]
# Master switch: false disables all redaction, everywhere.
enabled = true
auto = false
```

## Environment variables

`TERMSHOT_OUTPUT_DIR`, `TERMSHOT_FONT_PATH`, `TERMSHOT_FONT_SIZE`,
`TERMSHOT_COLS`, `TERMSHOT_ROWS`, `TERMSHOT_MAX_SCROLLBACK_LINES`,
`TERMSHOT_TIMEOUT`, `TERMSHOT_SHELL`, `TERMSHOT_THEME`, and `TERMSHOT_CHROME`
override the matching config keys. The
pre-rename `SCREENSHOT_MCP_*` spellings are still read with a deprecation
warning and will be removed in a future release.

## Output filenames

On the CLI, `-o` sets the path. Over MCP, `output_name` is the preferred way to
name a screenshot: pick a short descriptive slug such as `finding-01-sqli`.
Without a name, one is derived as `{working-dir}-{first-command-word}` (running
`cargo test` in `~/Desktop/webapp` yields `webapp-cargo.png`). Names are
lowercased, symbol runs collapse to single hyphens, and a numeric suffix (`-2`,
`-3`, ...) avoids overwriting an existing file.
