# MCP server

The termshot binary is both a CLI and an MCP server over stdio. Run
`termshot mcp` to serve, and register it in your MCP client config.

## Setup

```json
{
  "mcpServers": {
    "termshot": {
      "command": "termshot",
      "args": ["mcp"]
    }
  }
}
```

Use an absolute path if the binary is not on your `PATH`. Global flags apply to
the server too: `--config <path>` selects a `config.toml`, `--rules-path <dir>`
loads extra redaction rules. See [config.md](./config.md) and
[redaction.md](./redaction.md).

Every tool schema rejects unknown parameters.

## Tools

### `execute_and_screenshot`

Runs a command in a PTY and returns the screenshot path, exit status, an
optional redaction audit, and the terminal text.

- `command` (or `commands` for several, each with its own prompt), `cols`,
  `rows`, `timeout_secs`.
- `head_lines` / `tail_lines` (mutually exclusive): show only the first or last
  N lines. By default **every** line the command produced is shown, including
  everything that scrolled out of the viewport - see
  [Viewport rows vs retained output](#viewport-rows-vs-retained-output).
- `show_prompt` (default `true`): run through a login + interactive shell so the
  user's profile, prompt, aliases, and `PATH` apply. `false` runs the command
  non-interactively with no prompt in the image.
- `theme`, `chrome`, `title`, `timestamp`, `rounded` (default `true`),
  `auto_crop` (default `true`).
- `redact`, `redaction_rules`, `redact_text`, `show_labels`: with `redact: true`
  (or `[redaction] auto = true`) the image is masked; the returned text keeps
  the original content unless `redact_text: true`.
- `output_name`: preferred way to name the file, for example `finding-01-sqli`.
- `strip_ansi`: return text without ANSI color codes.

With `show_prompt: true` the returned text is the terminal screen with its
colors, so it matches the image line for line - the shell's own redraws, window
titles, and the trailing prompt are not part of it.

```json
{ "command": "nmap -F target", "chrome": "gnome", "redact": true,
  "output_name": "finding-01-portscan" }
```

```json
{ "command": "cargo test", "tail_lines": 25,
  "output_name": "test-summary-after-fix" }
```

#### Viewport rows vs retained output

`rows` and `cols` describe the terminal the command *runs in*: they decide where
long lines wrap and what a full-screen program is told the window size is. They
do **not** cap what the screenshot shows. Output that scrolls off the top is
retained and rendered, so `{"command": "seq 1 200", "rows": 10}` produces an
image with all 200 lines, and the `Description` metadata and returned text match
it.

Use `head_lines` / `tail_lines` when only one end matters; passing both is an
error. `head_lines` is always the first N lines the command printed, even when
it went on to print far more than the scrollback can hold, and at any
`max_scrollback_lines` setting: that setting bounds tail retention, and the head
is streamed as it scrolls past rather than selected out of what survived.

Retention is bounded by `max_scrollback_lines` in the server config (10,000
lines by default) and by a budget of 2,000,000 retained cells, so a very wide
terminal keeps fewer lines than a narrow one. If a command overruns whichever
limit applied, the result carries a warning naming it rather than silently
dropping the oldest output. A capture too tall to render (over 64 megapixels, or
over 320 MB of image buffers once chrome is added) is refused with an error
suggesting `head_lines` / `tail_lines`.

Full-screen (alternate-screen) programs such as `vim`, `htop` or `less` are the
exception: they repaint the whole viewport, so only their active screen is
captured. `head_lines` still wins where it can: output printed before the
program started is the head of the run, so N lines of it are returned rather
than the screen the program painted. A shorter prefix falls back to the active
screen.

### `render_ansi`

Renders a file of raw ANSI output to a PNG without executing anything. Takes
`input_path` plus the same rendering, redaction, and `head_lines` / `tail_lines`
parameters as `execute_and_screenshot`. A log taller than `rows` is rendered
whole, not clipped to its last screenful.

```json
{ "input_path": "/var/log/scan.ansi", "theme": "dracula", "redact": true }
```

### `redact_screenshot`

Re-renders a screenshot produced by the running server with selective
redactions, overwriting the PNG. Each entry in `redactions` is either a regex
`pattern` (optional `replacement` - also accepted as `label` - for the on-image
tag, plus `keep_prefix`, `keep_suffix`, and a `#RRGGBB` `color`) or an explicit
cell range (`row`, `col_start`, `col_end`, optional `label` and `color`).
`show_labels: false` draws plain blocks. An unknown field, an invalid regex, or
an unparseable color is rejected with an `invalid_params` error.

These are the same specifications the CLI takes through its repeatable
`--redaction '<JSON>'` option on `exec` and `render`, decoded by the same code,
so a redaction behaves identically from either entry point. The schema
`tools/list` publishes says exactly that: two mutually exclusive variants
(`oneOf`), each naming every field it accepts and refusing anything else
(`additionalProperties: false`). See
[redaction.md](./redaction.md#manual-redaction-cli-and-mcp).

Rows are counted in the image the screenshot actually shows: the original
`head_lines` / `tail_lines` selection is reproduced, so `row: 0` is the first
line you can see in the PNG. Patterns are matched across the whole capture, so a
secret that scrolled out of the viewport is masked just like one still on
screen.

Cell ranges are checked against what that image actually renders. Trailing blank
rows are trimmed away, and with `auto_crop` (the default) so are the empty
columns to the right of the output, so the drawn grid is usually smaller than
the retained capture. A `row` at or past the last rendered row, a `col_start` at
or past the last rendered column, a `col_end` past the right edge, or a
`col_start` that is not before `col_end` is rejected with an `invalid_params`
error naming the requested coordinates and the rendered dimensions - rather than
covering part of the range, or none of it, while still reporting a redaction.
Render with `auto_crop: false` if you need to address the full terminal width.
Patterns are unaffected: they are matched against the capture, so a match on the
rightmost content cell is masked as before.

```json
{
  "screenshot_path": "/tmp/termshot/finding-01-portscan.png",
  "redactions": [{ "pattern": "AKIA[0-9A-Z]{16}", "keep_prefix": 4 }]
}
```

### `compose_screenshots`

Places two or more screenshots side by side or stacked, like tmux split panes,
and can wrap the result in one outer window frame. `layout` is `"horizontal"` or
`"vertical"`, `divider` is the divider thickness in pixels (0 for none), and
`chrome` / `title` frame the whole composite. Compose works best on raw
(chrome-less) captures.

```json
{
  "paths": ["/tmp/termshot/before.png", "/tmp/termshot/after.png"],
  "layout": "horizontal",
  "chrome": "gnome",
  "title": "before / after",
  "output": "/tmp/composed.png"
}
```

## Workflow: capture, inspect, selectively redact

1. Capture with `execute_and_screenshot`. With `redact: true` (or `auto = true`
   in config) the PNG is masked for known secrets; the returned text is the
   original content either way.
2. Inspect the returned text and decide what else is sensitive.
3. Call `redact_screenshot` with the returned screenshot path and the
   redactions you want. Use `keep_prefix` / `keep_suffix` to reveal only part of
   a value (see [redaction.md](./redaction.md#partial-redaction)).

`redact_screenshot` works only on screenshots from the current server process.

## Screenshot descriptions

Each PNG carries its terminal text (redacted, when redaction ran) in a UTF-8
`Description` chunk for screen readers whenever `embed_description` is on in the
server config, which is the default. `compose_screenshots` joins the source
descriptions with `--- Pane N ---` markers. This is not an MCP parameter: the
document embedding a screenshot owns its alt text, so change it with
`embed_description = false` in the config.
