# Terminal Chrome Plan

## Goal

Add optional window chrome around rendered terminal screenshots so they look more like real terminal application windows rather than raw terminal content on a flat background.

## Why

- Better screenshots for pentest reports and writeups
- Better visual fidelity for agent screenshots
- Easier visual distinction between terminal content and surrounding page/report backgrounds

## Scope

The first version should be decorative only and should not affect terminal text layout.

## Proposed Architecture

### Current pipeline

```text
raw ANSI/PTy bytes
  -> vt100 parser
  -> renderer draws terminal cells to PNG
```

### Future pipeline

```text
raw ANSI/PTy bytes
  -> vt100 parser
  -> renderer draws terminal cells to intermediate image
  -> chrome compositor wraps image in frame/title bar/shadow
  -> final PNG
```

## Chrome presets

### `none`

Current behavior. Render only terminal content.

### `minimal`

- outer padding
- rounded corners
- subtle border
- subtle shadow

### `gnome`

- top title bar
- centered or left-aligned title text
- subtle GNOME-like header background
- optional tab strip styling later

### `macos`

- title bar
- red/yellow/green traffic lights
- rounded corners
- soft shadow

### `report`

- optimized for embedding in markdown/PDF reports
- neutral chrome
- stronger contrast and padding

## Config/API additions

### Config file

```toml
[chrome]
enabled = true
preset = "gnome"
title = "termshot"
shadow = true
radius = 12
padding = 14
```

### CLI

```bash
termshot exec --chrome gnome --title "nmap scan" "nmap -sV 10.0.0.1"
```

### MCP

```json
{
  "command": "nmap -sV 10.0.0.1",
  "theme": "adamkadaban",
  "chrome": {
    "preset": "gnome",
    "title": "nmap scan"
  }
}
```

## Implementation steps

1. Refactor renderer to produce an intermediate terminal-content image
2. Add a chrome compositor layer that can draw:
   - title bar
   - border
   - background frame
   - shadow
   - corner radius mask
3. Add presets + defaults
4. Expose via config/CLI/MCP
5. Add golden-image tests for layout and dimensions

## Rendering details

- Use `image` crate for simple composition first
- Shadow can be approximated with layered translucent rectangles initially
- Rounded corners can be done with clipping/masking
- Chrome must not change the terminal cell grid size; only add outer frame/padding

## Nice-to-have later

- tab strip
- command title inferred from first command
- hostname badge or session metadata
- timestamp watermark for report screenshots
- side-by-side split panes
