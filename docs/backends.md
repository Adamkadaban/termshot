# Rendering backends

The default `builtin` backend uses termshot's existing terminal emulator and
Rust renderer. It requires no graphical session or native GTK libraries and
supports scrollback, line selection, and redaction.

The experimental `vte` backend renders ANSI bytes with the real GTK4 VTE
widget, the terminal engine used by GNOME Terminal. It is an alternative
renderer, not a way to photograph or control an existing GNOME Terminal tab.
The existing PTY executor and prompt handling are unchanged.

## Enable VTE on Linux

Install the optional system runtime: GTK4 and VTE 0.76 or newer. On Ubuntu 24.04:

```sh
sudo apt install python3 python3-gi gir1.2-gtk-4.0 gir1.2-vte-3.91 fontconfig
```

The helper uses `/usr/bin/python3` in isolated mode so a project virtual
environment does not affect the native runtime. GTK/VTE are not linked into
the Rust binary, and no new Cargo dependencies are required.

```sh
termshot --backend vte exec --chrome gnome 'ls --color=always'
termshot --backend vte render --cols 100 --rows 30 capture.ansi
```

GTK needs a display and creates a short-lived native window for each capture.
For headless use, or to avoid a window appearing on your desktop:

```sh
sudo apt install xvfb xauth
xvfb-run -a termshot --backend vte exec --no-prompt 'ls --color=always'
```

Select it persistently with the top-level config key:

```toml
backend = "vte"
```

`TERMSHOT_BACKEND=vte` overrides the config key; `--backend builtin` or
`--backend vte` overrides both. An unavailable runtime or display is an error,
not a silent fallback. Runtime access is checked before executing commands.

For MCP, use `"args": ["--backend", "vte", "mcp"]`. A headless client can use
`"command": "xvfb-run"` with
`"args": ["-a", "/absolute/path/to/termshot", "--backend", "vte", "mcp"]`.
Backend selection applies to the server, not individual tool calls.

## Experimental limits

- Only the final visible viewport is captured. `rows` is a real height limit;
  offscreen scrollback is not included in the PNG or returned text.
  `max_scrollback_lines` does not enlarge a native capture. Head/tail selections
  are rejected.
- Redaction is unsupported, including automatic rules, manual masks,
  `redact_text`, and the later `redact_screenshot` tool. Requests requiring it
  fail before command execution. Use `builtin` when redaction is needed.
- Text is always plain visible text, even when ANSI stripping is not requested.
  PNG `Description` metadata contains that same text, unless disabled.
- Themes, configured fonts, 2x rendering, cropping, chrome, timestamps, and PNG
  composition remain available. Native metrics and font matching can differ
  from the built-in backend. An explicit bold face must share the primary
  font's family. Pango may use system fallback fonts; the built-in renderer's
  `TERMSHOT_SYSTEM_FONTS` and `TERMSHOT_UNICODE_SHAPING` controls do not apply.
- ANSI input is limited to 4 MiB after newline normalization, native viewports
  to 16 megapixels, and font sizes to 128 pixels. The helper has a bounded
  capture deadline. These limits produce explicit errors.

This improves fidelity to VTE rendering, but does not promise identical pixels
to every GNOME Terminal installation. GTK/VTE versions, fonts, theme settings,
and display rendering still matter.

## Native regression tests

The default suite checks backend selection and fail-closed behavior without a
graphical runtime. With the optional packages installed, run the real capture
tests explicitly:

```sh
GTK_A11Y=none GSK_RENDERER=cairo xvfb-run -a \
  cargo test --locked --test vte_backend -- --include-ignored --test-threads=1
```

The same tests should also pass without `GSK_RENDERER=cairo`, using GTK's
default renderer.
