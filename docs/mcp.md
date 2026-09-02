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
- `cwd`: set the child shell's working directory before its first prompt.
- `head_lines` / `tail_lines` (mutually exclusive): show only the first or last
  N lines. Otherwise every line is shown, including what scrolled out of view.
- `show_prompt` (default `true`): login + interactive shell, so the user's
  real prompt, aliases, and `PATH` apply. A prompt appears before each command;
  termshot always removes the final prompt after the last command.
- `theme`, `chrome`, `title`, `timestamp`, `rounded`, `auto_crop`.
- `output_name` (for example `finding-01-sqli`), `strip_ansi`.
- `redact`, `redaction_rules`, `redact_text`, `show_labels`, `redactions` - see
  [redaction.md](./redaction.md#manual-redaction-cli-and-mcp).

```json
{ "command": "nmap -F target", "cwd": "~/engagement", "chrome": "gnome", "redact": true,
  "output_name": "finding-01-portscan" }
```

Use `cwd` when the directory is useful context. If a long source path is
incidental, stage the required files in a short directory or set
`show_prompt: false`. Never print or manually style a prompt in `command`; with
`show_prompt: false`, prompt-looking text is ordinary output and is not removed.
There is no trailing-prompt option because a real trailing prompt is already
removed automatically.
New clients should pass either `command` or `commands`. For compatibility with
the original schema, `commands` takes precedence if both are present.

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
