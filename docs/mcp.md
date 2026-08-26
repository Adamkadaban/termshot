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

```json
{ "command": "nmap -F target", "chrome": "gnome", "redact": true,
  "output_name": "finding-01-portscan" }
```

### `render_ansi`

Renders a file of raw ANSI output to a PNG without executing anything. Takes
`input_path` plus the same rendering and redaction parameters as
`execute_and_screenshot`.

```json
{ "input_path": "/var/log/scan.ansi", "theme": "dracula", "redact": true }
```

### `redact_screenshot`

Re-renders a screenshot produced by the running server with selective
redactions, overwriting the PNG. Each entry in `redactions` is either a regex
`pattern` (optional `replacement` for the on-image label, `keep_prefix`,
`keep_suffix`) or an explicit cell range (`row`, `col_start`, `col_end`,
optional `label`). `show_labels: false` draws plain blocks.

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
