# termshot

`termshot` is a Rust CLI and MCP server for capturing terminal screenshots with ANSI escape sequences fully rendered into PNGs.

It is built for workflows like pentesting, operator runbooks, and agent tooling where plain text is not enough and you want the terminal output to look like a terminal.

## What It Does

- Executes commands in a PTY so shell prompts, colors, cursor movement, and formatting are preserved
- Renders terminal state to PNG using a bundled monospace font
- Works as both:
  - a standalone CLI
  - an MCP server over stdio
- Supports multiple themes, including a built-in `adamkadaban` theme captured from the local GNOME Terminal profile
- Preserves intermediate prompts for multi-command sessions while stripping the trailing prompt
- Supports optional terminal chrome presets such as `gnome`, `macos`, and `report`

## Status

Core functionality is working:

- CLI command execution
- MCP server mode
- ANSI rendering to PNG
- PS1 prompt capture
- multiple command support
- title-sequence stripping (`ESC k ... ESC \\`, OSC title updates)

Stretch goals like redaction and terminal chrome are planned and documented locally in `REDACTION.md` and `TERMINAL_CHROME.md`.

## Install

### Local development

```bash
cargo build
```

Run directly:

```bash
cargo run -- themes
cargo run -- exec "ls -la"
cargo run -- mcp
```

### Install as a CLI

```bash
cargo install --path .
```

This installs the binary as `termshot`.

## CLI Usage

### Execute a command and capture a screenshot

```bash
termshot exec "ls -la"
```

Prints the screenshot path to `stdout` and the textual output to `stderr`.

### Multiple commands, each with its own prompt

```bash
termshot exec "pwd" "whoami" "echo hello"
```

This renders:

```text
PS1$ pwd
/path/to/repo
PS1$ whoami
adam
PS1$ echo hello
hello
```

The trailing prompt is stripped.

### Run without the prompt

```bash
termshot exec --no-prompt -- ls --color=always /tmp
```

### Pick a theme

```bash
termshot exec --theme adamkadaban "ls -la"
termshot exec --theme catppuccin-mocha "git status"
```

### Add terminal chrome

```bash
termshot exec --chrome gnome --title "nmap scan" "nmap -sV 10.0.0.1"
termshot exec --chrome macos --title "operator shell" "whoami"
```

### Write to a specific file

```bash
termshot exec -o screenshot.png "nmap -sV 10.0.0.1"
```

### Render an ANSI file directly

```bash
termshot render session.ansi
termshot render --theme dracula -o render.png session.ansi
```

### List built-in themes

```bash
termshot themes
```

## MCP Usage

Start the MCP server on stdio:

```bash
termshot mcp
```

### MCP tools

#### `execute_and_screenshot`

Runs a command in a PTY and returns:

- screenshot path
- status / exit code
- plain text terminal output

If `commands` is provided, each command is executed on its own line and gets its own PS1 prompt.

Parameters:

```json
{
  "command": "ls -la",
  "commands": ["pwd", "whoami"],
  "cols": 120,
  "rows": 40,
  "timeout_secs": 30,
  "show_prompt": true,
  "theme": "adamkadaban",
  "chrome": "gnome",
  "title": "operator shell"
}
```

#### `render_ansi`

Renders a file containing raw ANSI terminal output:

```json
{
  "input_path": "/tmp/captured.ansi",
  "cols": 120,
  "rows": 40,
  "theme": "dracula",
  "chrome": "report",
  "title": "captured session"
}
```

## Config File

Default lookup order:

1. `--config <path>`
2. `~/.config/termshot/config.toml`
3. `./termshot.toml`

Example config:

```toml
output_dir = "/tmp/screenshot-mcp"
font_size = 16.0
cols = 120
rows = 40
timeout_secs = 30
default_theme = "adamkadaban"

[chrome]
enabled = false
preset = "none"
title = "termshot"
shadow = true
radius = 14
outer_padding = 18
title_bar_height = 34

[themes.my-theme]
foreground = "#e0e0e0"
background = "#10141a"
palette = [
  "#10141a", "#ff6b6b", "#51cf66", "#fcc419",
  "#4dabf7", "#b197fc", "#22b8cf", "#ced4da",
  "#495057", "#ff8787", "#69db7c", "#ffd43b",
  "#74c0fc", "#d0bfff", "#66d9e8", "#f8f9fa",
]
```

See `termshot.example.toml` for a starting point.

### Environment variables

- `SCREENSHOT_MCP_OUTPUT_DIR`
- `SCREENSHOT_MCP_FONT_PATH`
- `SCREENSHOT_MCP_FONT_SIZE`
- `SCREENSHOT_MCP_COLS`
- `SCREENSHOT_MCP_ROWS`
- `SCREENSHOT_MCP_TIMEOUT`
- `SCREENSHOT_MCP_SHELL`
- `SCREENSHOT_MCP_THEME`
- `SCREENSHOT_MCP_CHROME`

## Built-in Themes

- `adamkadaban`
- `dark`
- `catppuccin-mocha`
- `catppuccin-latte`
- `catppuccin-frappe`
- `catppuccin-macchiato`
- `dracula`
- `nord`
- `gruvbox-dark`
- `solarized-dark`
- `solarized-light`
- `tokyo-night`

## Current Limitations

- Redaction is documented but not implemented yet
- Chrome is intentionally lightweight today and not yet a pixel-perfect GNOME Terminal clone
- `--no-prompt` mode shells out via `-c`, so shell-specific quoting/escaping behavior still applies
- MCP still prefers `command` for backward compatibility, even though `commands` is now supported

## Testing

Run unit tests:

```bash
cargo test
```

Current tests cover:

- stripping GNU Screen and OSC title sequences
- preserving row boundaries when rebuilding ANSI from the vt100 screen
- preserving intermediate prompts while removing trailing sentinel/prompt rows
- chrome-related code paths compile and are exercised by CLI smoke checks

## Release Workflow

GitHub Actions builds release binaries for:

- Linux x86_64
- Linux aarch64
- macOS x86_64
- macOS aarch64

Create a tag like `v0.1.0` to trigger a release.

## Roadmap

### Terminal chrome

Planned in `TERMINAL_CHROME.md`.

High-level direction:

- add a decorative terminal frame around the rendered content
- support multiple chrome presets (plain, GNOME-like, macOS-like, minimal)
- allow title text, tab label, shadow, padding, corner radius, and DPI scaling

### Redaction

Planned in `REDACTION.md`.

High-level direction:

- regex-based rules first
- optional local-LLM-assisted redaction
- redact at the terminal-cell layer before PNG rendering
