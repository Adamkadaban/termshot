# MCP server

The termshot binary is also an MCP server over stdio (`termshot mcp`).

## Client config

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

Use an absolute path if the binary is not on `PATH`. Global flags apply to the
server too: `--config <path>` selects a `config.toml` ([config.md](./config.md))
and `--rules-path <dir>` loads extra redaction rules
([redaction.md](./redaction.md)).

## Tools

| Tool | Purpose |
| --- | --- |
| `execute_and_screenshot` | Run a command in a PTY and save a PNG of its output. |
| `render_ansi` | Render an existing ANSI log to a PNG without executing anything. |
| `redact_screenshot` | Re-render a screenshot from this server with extra redactions. |
| `compose_screenshots` | Combine screenshots side by side or stacked into one image. |

### `execute_and_screenshot`

Returns the screenshot path, exit status, an optional audit, and the text. Every
tool schema rejects unknown parameters.

- `command` (or `commands` for several), `cols`, `rows`, `timeout_secs`.
- `head_lines` / `tail_lines` (mutually exclusive): show only the first or last
  N lines. Otherwise every line is shown, including what scrolled out of view.
- `show_prompt` (default `true`): login + interactive shell, so the user's
  prompt, aliases, and `PATH` apply.
- `theme`, `chrome`, `title`, `timestamp`, `rounded`, `auto_crop`.
- `output_name` (for example `finding-01-sqli`), `strip_ansi`.
- `redact`, `redaction_rules`, `redact_text`, `show_labels`, `redactions` - see
  [redaction.md](./redaction.md#manual-redaction-cli-and-mcp).

```json
{ "command": "nmap -F target", "chrome": "gnome", "redact": true,
  "output_name": "finding-01-portscan" }
```

### `render_ansi`

Takes `input_path` plus the same rendering, redaction, and `head_lines` /
`tail_lines` parameters. A log taller than `rows` is rendered whole.

```json
{ "input_path": "/var/log/scan.ansi", "theme": "dracula", "redact": true }
```

### `redact_screenshot`

Overwrites a PNG from the running server with additional redactions, in the
shared format documented in
[redaction.md](./redaction.md#manual-redaction-cli-and-mcp). Coordinates are
counted in the image as rendered (`row: 0` is the first visible line);
out-of-range values are an `invalid_params` error. Patterns match the whole
capture, including lines that scrolled out of view.

```json
{ "screenshot_path": "/tmp/termshot/finding-01-portscan.png",
  "redactions": [{ "pattern": "AKIA[0-9A-Z]{16}", "keep_prefix": 4 }] }
```

### `compose_screenshots`

`layout` is `"horizontal"` or `"vertical"`, `divider` sets the divider thickness
in pixels, and `chrome` / `title` frame the composite. Use raw captures as input.

```json
{ "paths": ["/tmp/termshot/before.png", "/tmp/termshot/after.png"],
  "layout": "horizontal", "chrome": "gnome", "output": "/tmp/composed.png" }
```

## Workflow: capture, inspect, redact

1. Capture with `execute_and_screenshot`. With `redact: true` (or `auto = true`
   in config) the PNG is masked for known secrets; the returned text keeps the
   original content either way.
2. Inspect that text and decide what else is sensitive.
3. Call `redact_screenshot` with the screenshot path and your redactions.

When you already know what to mask, skip the round trip: pass the same
specifications as `redactions` on `execute_and_screenshot` / `render_ansi`, and
the image is never written unmasked. `redact_screenshot` works only on
screenshots from the current server process.

Each PNG carries its terminal text in a UTF-8 `Description` chunk for screen
readers. That is a config key, not an MCP parameter (`embed_description`, see
[config.md](./config.md)).
