# MCP server

termshot ships as a single binary that is both a CLI and an MCP server over
stdio. Run `termshot mcp` to serve, and register it in your MCP client config.

## Setup

A typical client entry looks like this:

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

If the binary is not on your PATH, use its absolute path (for example
`/home/you/termshot/target/release/termshot`).

Global config flags apply to the server too: pass `--config <path>` to point at
a specific `config.toml`, and `--rules-path <dir>` to load extra redaction rule
files. See [config.md](./config.md) and [redaction.md](./redaction.md).

## Tools

Commands run in a PTY through the server's shell as a login + interactive shell
(`$SHELL -l -i`), so captures inherit the user's environment: profile files are
sourced and the prompt, aliases, functions, and `PATH` all apply. Pass
`show_prompt: false` to run the command non-interactively (`$SHELL -c`) with no
prompt in the image.

- `execute_and_screenshot` runs a command in a PTY and returns the screenshot
  path, exit status, an optional redaction audit, and the terminal text. Set
  `redact: true` (or configure `[redaction] auto = true`) to mask secrets in the
  image; the returned text keeps the original content so an agent can inspect
  it, unless `redact_text: true`. Prefer `output_name` to name the file
  descriptively (for example `finding-01-sqli`).
- `render_ansi` renders a file of raw ANSI output to a PNG. It honors the same
  redaction parameters (`redact`, `redaction_rules`, `redact_text`,
  `show_labels`) and the same `[redaction] auto` config as
  `execute_and_screenshot`.
- `redact_screenshot` re-renders a screenshot from this session with selective
  redactions (regex patterns and/or explicit cell ranges), overwriting the PNG.
  The raw output and metadata are kept in memory, so this works only for
  screenshots produced by the running server instance.
- `compose_screenshots` places two or more screenshots side by side or stacked
  into a single image, like tmux split panes, and can wrap the composed result
  in one outer window frame.

Screenshot metadata is not an MCP parameter. Every PNG carries its terminal
text - redacted, if redaction ran - in a UTF-8 `Description` chunk (PNG
`iTXt`) whenever `embed_description` is on in the server config, which it is by
default. The document or app embedding a screenshot owns its alt text, so tool
callers cannot change or omit it per call; flip `embed_description = false` in
the config (or use the CLI's `--no-description`) to turn it off. A composed
image is described too: `compose_screenshots` reads each source PNG's
`Description` and joins them, separated by `--- Pane N ---` markers.

Unknown parameters are rejected. Every tool's schema forbids extra properties,
so a call that passes a field the tool does not define (for example a legacy
`embed_description`) fails with an error instead of being silently ignored.

`execute_and_screenshot` and `render_ansi` accept an optional `rounded`
(boolean, default `true`): with `chrome` the window frame is rounded, and
without chrome the terminal image itself gets soft rounded corners on a
transparent background. Set `rounded: false` for square corners.

## Agent workflow: selective redaction

The redaction flow is designed so an agent can see the real output, decide what
is sensitive, and mask exactly that.

1. Capture with `execute_and_screenshot`. With `redact: true` (or `auto = true`
   in config) the PNG is masked for known secrets; the returned text is the
   original content either way.
2. Inspect the returned text and decide what else is sensitive.
3. Call `redact_screenshot` with the returned screenshot path and a list of
   redactions, each either a regex `pattern` (with optional `replacement`, used
   as the on-image label - omit it for a plain block with no label) or an
   explicit cell range (`row`, `col_start`, `col_end`, optional `label`). The
   image is re-rendered in place from the in-memory record. Pass
   `show_labels: false` (on `execute_and_screenshot` or `redact_screenshot`) to
   draw plain solid blocks with no text overlay.

To reveal only part of a secret, set `keep_prefix` and/or `keep_suffix`
(character counts) on a `redact_screenshot` pattern. See
[redaction.md](./redaction.md#partial-redaction).

## Agent workflow: composition

Use `compose_screenshots` to combine related captures into one image for
before/after comparisons or theme galleries:

```json
{
  "paths": ["/tmp/termshot/before.png", "/tmp/termshot/after.png"],
  "layout": "horizontal",
  "divider": 2,
  "chrome": "gnome",
  "title": "before / after",
  "output": "/tmp/composed.png"
}
```

Compose works best on raw (chrome-less) captures: the panes are joined with a
thin divider, and the optional `chrome` field wraps the whole result in a
single outer frame instead of giving each pane its own title bar. Omit `chrome`
for a frameless composite. `layout` is `"horizontal"` or `"vertical"`, and
`divider` is the divider thickness in pixels (0 for none).
