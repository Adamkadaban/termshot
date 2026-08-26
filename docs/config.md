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
| `rows` | `40` | Default terminal height in rows. |
| `timeout_secs` | `30` | Default command timeout in seconds. |
| `default_theme` | `dark` | Theme used when `--theme` is not given. See [themes.md](./themes.md). |
| `font_path` | unset | Monospace font overriding the embedded JetBrains Mono. |
| `shell` | `$SHELL` | Shell used to execute commands. `exec` runs it as a login + interactive shell (`-l -i`), so your profile, prompt, aliases, and `PATH` apply; `--no-prompt` runs it non-interactively (`-c`). |
| `embed_description` | `true` | Embed the terminal text (redacted, when redaction ran) in each PNG's UTF-8 `Description` metadata for screen readers. Disable per run with `--no-description`. Not exposed over MCP. |

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
`TERMSHOT_COLS`, `TERMSHOT_ROWS`, `TERMSHOT_TIMEOUT`, `TERMSHOT_SHELL`,
`TERMSHOT_THEME`, and `TERMSHOT_CHROME` override the matching config keys. The
pre-rename `SCREENSHOT_MCP_*` spellings are still read with a deprecation
warning and will be removed in a future release.

## Output filenames

On the CLI, `-o` sets the path. Over MCP, `output_name` is the preferred way to
name a screenshot: pick a short descriptive slug such as `finding-01-sqli`.
Without a name, one is derived as `{working-dir}-{first-command-word}` (running
`cargo test` in `~/Desktop/webapp` yields `webapp-cargo.png`). Names are
lowercased, symbol runs collapse to single hyphens, and a numeric suffix (`-2`,
`-3`, ...) avoids overwriting an existing file.
