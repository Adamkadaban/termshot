# Configuration

On first run termshot bootstraps `~/.config/termshot/` with a starter
`config.toml` plus empty `rules/` (redaction rules) and `themes/` directories;
`--config <path>` selects a different file. A commented reference ships as
`termshot.example.toml`, and packages never write a user config, so upgrades do
not overwrite your settings.

## Top-level keys

| Key | Default | Description |
| --- | --- | --- |
| `output_dir` | `/tmp/termshot` | Directory where screenshots are written. |
| `font_size` | `16.0` | Text size in pixels. |
| `cols` | `120` | Default terminal width in columns. |
| `rows` | `40` | Default terminal viewport height in rows. |
| `max_scrollback_lines` | `10000` | Scrolled-off lines each capture keeps (1 to 60,000), so a screenshot can show output that never fit in the viewport. |
| `timeout_secs` | `30` | Default command timeout in seconds. |
| `default_theme` | `dark` | Theme used when `--theme` is not given. See [themes.md](./themes.md). |
| `font_path` | unset | Monospace font overriding the embedded JetBrains Mono. |
| `shell` | `$SHELL` | Shell used to execute commands. `exec` runs it as a login + interactive shell (`-l -i`), so your profile, prompt, aliases, and `PATH` apply; `--no-prompt` runs it non-interactively (`-c`). |
| `embed_description` | `true` | Embed the terminal text (redacted, when redaction ran) in each PNG's UTF-8 `Description` metadata for screen readers. Disable per run with `--no-description`. |

`rows` and `cols` size the terminal a command *runs in*: they decide where long
lines wrap, not what the screenshot shows. Output that scrolls off the top is
retained and rendered, bounded by `max_scrollback_lines` and a per-capture cell
budget, so a very wide terminal keeps fewer lines than a narrow one; overrunning
either warns rather than truncating silently. Use `--head-lines N` /
`--tail-lines N` (`head_lines` / `tail_lines` over MCP, mutually exclusive) to
render one end only. Full-screen programs such as `vim` and `htop` repaint the
viewport, so only their active screen is captured.

## `[chrome]`

| Key | Default | Description |
| --- | --- | --- |
| `enabled` | `false` | Draw chrome by default. |
| `preset` | `none` | `none` (raw terminal), `minimal` (padding, no title bar), `gnome`, `macos`, or `report`. |
| `title` | unset | Default title bar text (the first command is used when unset). |
| `timestamp` | `false` | Draw a UTC timestamp watermark below the content. |
| `shadow` | `false` | Draw a drop shadow behind the frame. |
| `radius` | `14` | Corner radius in pixels. |
| `rounded` | `true` | Soft rounded corners. Without chrome, the terminal image gets rounded corners on a transparent background. |
| `outer_padding` | `0` | Extra padding between frame and terminal content. |
| `title_bar_height` | `34` | Title bar height in pixels (ignored by `minimal`). |

## `[redaction]`

| Key | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Master switch. `false` disables all redaction, and an explicit `--redact` / `redact: true` then fails with an error. |
| `auto` | `false` | Redact every screenshot's image without an explicit flag. |
| `color` | `#d41919` | Default block color as `#RRGGBB`. |
| `label_color` | black | Label text color as `#RRGGBB`. |
| `rules_path` | unset | Extra directory of `.toml`/`.yaml` rule files, layered on `~/.config/termshot/rules/`. |
| `[[redaction.rules]]` | none | Inline rule overrides and additions. |

See [redaction.md](./redaction.md) for rule format and manual redactions.

## `[themes.<name>]`

Inline theme definitions using the same fields as a user theme file: see
[themes.md](./themes.md).

## Example

```toml
output_dir = "~/screenshots"
cols = 120
rows = 40
default_theme = "catppuccin-mocha"

[chrome]
enabled = true
preset = "gnome"
shadow = true

[redaction]
auto = true            # redact every screenshot

[[redaction.rules]]
name = "aws_key"       # override a built-in: show AKIA, mask the rest
keep_prefix = 4
```

## Environment variables

`TERMSHOT_OUTPUT_DIR`, `TERMSHOT_FONT_PATH`, `TERMSHOT_FONT_SIZE`,
`TERMSHOT_COLS`, `TERMSHOT_ROWS`, `TERMSHOT_MAX_SCROLLBACK_LINES`,
`TERMSHOT_TIMEOUT`, `TERMSHOT_SHELL`, `TERMSHOT_THEME`, and `TERMSHOT_CHROME`
override the matching config keys. The pre-rename `SCREENSHOT_MCP_*` spellings
are still read with a deprecation warning.

## Output filenames

On the CLI, `-o` sets the path; over MCP, `output_name` takes a short slug such
as `finding-01-sqli`. Without either, a name is derived as
`{working-dir}-{first-command-word}` (`cargo test` in `~/Desktop/webapp` yields
`webapp-cargo.png`). Names are lowercased, symbol runs collapse to single
hyphens, and a numeric suffix (`-2`, `-3`, ...) avoids overwriting a file.
