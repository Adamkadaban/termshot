# Configuration

On first run termshot bootstraps `~/.config/termshot/` with a starter
`config.toml`, `themes/` and `rules/` directories, and an `adamkadaban` example user theme
(not selected by default: it points at a commercial font). Point at a different
file with `--config <path>`. A commented example ships as
`termshot.example.toml`.

The Debian/Ubuntu package does not write a user config. On first run any command
that touches config (for example `termshot themes`) bootstraps
`~/.config/termshot/`, so your settings are never overwritten by upgrades.

## Top-level keys

| Key | Default | Description |
| --- | --- | --- |
| `output_dir` | `/tmp/termshot` | Directory where screenshots are written. |
| `font_size` | `16.0` | Text size in pixels. |
| `cols` | `120` | Default terminal width in columns. |
| `rows` | `40` | Default terminal height in rows. |
| `timeout_secs` | `30` | Default command timeout in seconds. |
| `default_theme` | `dark` | Theme used when `--theme` is not given. |
| `font_path` | unset | Path to a monospace font, overriding the embedded JetBrains Mono. |
| `shell` | `$SHELL` | Shell used to execute commands. `exec` runs it as a login + interactive shell (`-l -i`), so your profile, prompt, aliases, and `PATH` apply; `--no-prompt` runs it non-interactively (`-c`). |
| `embed_description` | `true` | Embed the terminal text (redacted, when redaction ran) in each PNG's `Description` metadata for screen readers. Disable per run with `--no-description`. |

## `[chrome]`

Window chrome frames the terminal like a real window.

| Key | Default | Description |
| --- | --- | --- |
| `enabled` | `false` | Draw chrome by default. |
| `preset` | `none` | `none`, `minimal`, `gnome`, `macos`, or `report`. |
| `title` | unset | Default title bar text (the first command is used when unset). |
| `timestamp` | `false` | Draw a UTC timestamp watermark below the content. |
| `shadow` | `false` | Draw a drop shadow behind the frame. |
| `radius` | `14` | Corner radius in pixels. |
| `rounded` | `true` | Draw soft rounded corners. Independent of chrome: with chrome the window frame is rounded; without chrome the terminal image itself gets rounded corners on a transparent background. Set `false` for square corners. |
| `outer_padding` | `18` | Padding between the frame edge and the terminal content. |
| `title_bar_height` | `34` | Title bar height in pixels (ignored by `minimal`). |

Presets:

- `none`: no chrome (raw terminal image).
- `minimal`: framed with padding, no title bar.
- `gnome`: GNOME-style title bar with a control pill.
- `macos`: macOS-style title bar with red, yellow, green controls.
- `report`: a clean framed style suited to documents and reports.

## `[redaction]`

| Key | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | **Master switch.** Set to `false` to disable *all* redaction globally: no rule ever runs, and an explicit `--redact` / `redact: true` fails with an error instead of silently producing an unredacted image. |
| `auto` | `false` | Redact every screenshot's image without an explicit flag. Off by default: a false positive silently mangles ordinary output, which is worse than a capture you chose not to redact. |
| `color` | red (`#d41919`) | Default block color as `#RRGGBB`. |
| `label_color` | black | Label text color as `#RRGGBB`. |
| `rules_path` | unset | *Additional* directory of `.toml`/`.yaml` rule files. `~/.config/termshot/rules/` is always scanned; rules from `rules_path` (or `--rules-path`) are layered on top and win on name conflicts. |
| `[[redaction.rules]]` | none | Inline rule overrides / additions. |

See [redaction.md](./redaction.md) for the rule format and partial redaction.

## `[themes.<name>]`

Inline theme definitions, using the same fields as a user theme file
(`foreground`, `background`, `palette`, optional `font` / `font_bold`). See
[themes.md](./themes.md).

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
outer_padding = 18
title_bar_height = 34

[redaction]
# Master switch: false disables all redaction, everywhere.
enabled = true
auto = false
```

## Rule and theme directories

Bootstrapped on first run and scanned automatically:

```
~/.config/termshot/
├── config.toml
├── rules/     # every .toml / .yaml file here is loaded as redaction rules
└── themes/    # every .toml file here is a theme, named after the file
```

## Environment variables

Several settings can be overridden by environment variables:
`TERMSHOT_OUTPUT_DIR`, `TERMSHOT_FONT_PATH`, `TERMSHOT_FONT_SIZE`,
`TERMSHOT_COLS`, `TERMSHOT_ROWS`, `TERMSHOT_TIMEOUT`, `TERMSHOT_SHELL`,
`TERMSHOT_THEME`, and `TERMSHOT_CHROME`. The default output directory is
`/tmp/termshot`.

The pre-rename `SCREENSHOT_MCP_*` spellings are still read (with a deprecation
warning) and will be removed in a future release.

## Output filenames

On the CLI, use `-o` to set the path. Over MCP, `output_name` is the preferred
way to name a screenshot; pick a short descriptive slug such as
`finding-01-sqli` or `before-fix`. When no name is given, a fallback is derived
as `{working-dir}-{first-command-word}` (for example running `cargo test` in
`~/Desktop/webapp` yields `webapp-cargo.png`). Names are lowercased, symbol runs
collapse to single hyphens, and a numeric suffix (`-2`, `-3`, and so on) is
added to avoid overwriting an existing file.
