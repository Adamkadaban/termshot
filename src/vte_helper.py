"""Private GTK4/VTE helper. Stdin is JSON; stdout is a JSON line followed by PNG bytes."""

import ctypes
import json
import os
from pathlib import Path
import sys
import tempfile
import uuid


def load_runtime():
    try:
        import gi

        gi.require_version("Gtk", "4.0")
        gi.require_version("Vte", "3.91")
        gi.require_version("Gsk", "4.0")
        gi.require_version("Gdk", "4.0")
        gi.require_version("Graphene", "1.0")
        from gi.repository import Gdk, GLib, Graphene, Gtk, Pango, Vte
    except (ImportError, ValueError) as error:
        raise RuntimeError(
            "install python3-gi, gir1.2-gtk-4.0, gir1.2-vte-3.91 and "
            "fontconfig (Debian/Ubuntu package names)"
        ) from error
    if (Vte.get_major_version(), Vte.get_minor_version()) < (0, 76):
        raise RuntimeError("the experimental backend requires VTE 0.76 or newer (GTK4 ABI)")
    if not Gtk.init_check() or Gdk.Display.get_default() is None:
        raise RuntimeError(
            "GTK cannot connect to a display; run under a desktop session "
            "or use xvfb-run -a termshot --backend vte ..."
        )
    return Gdk, GLib, Graphene, Gtk, Pango, Vte


def register_fonts(paths):
    fontconfig = ctypes.CDLL("libfontconfig.so.1")
    fontconfig.FcConfigAppFontAddFile.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
    fontconfig.FcConfigAppFontAddFile.restype = ctypes.c_int
    fontconfig.FcFreeTypeQuery.argtypes = [
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_int),
    ]
    fontconfig.FcFreeTypeQuery.restype = ctypes.c_void_p
    fontconfig.FcPatternGetString.argtypes = [
        ctypes.c_void_p,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.POINTER(ctypes.c_char_p),
    ]
    fontconfig.FcPatternGetString.restype = ctypes.c_int
    fontconfig.FcPatternDestroy.argtypes = [ctypes.c_void_p]
    families = []
    for path in paths:
        encoded = os.fsencode(path)
        if not fontconfig.FcConfigAppFontAddFile(None, encoded):
            raise RuntimeError(f"Fontconfig cannot load font {path!r}")
        count = ctypes.c_int()
        pattern = fontconfig.FcFreeTypeQuery(encoded, 0, None, ctypes.byref(count))
        if not pattern:
            raise RuntimeError(f"Fontconfig cannot inspect font {path!r}")
        try:
            family = ctypes.c_char_p()
            if fontconfig.FcPatternGetString(pattern, b"family", 0, ctypes.byref(family)):
                raise RuntimeError(f"Font has no family name: {path!r}")
            name = family.value.decode("utf-8")
            if name not in families:
                families.append(name)
        finally:
            fontconfig.FcPatternDestroy(pattern)
    return families


def capture(request, runtime, font_dir):
    Gdk, GLib, Graphene, Gtk, Pango, Vte = runtime
    embedded = Path(font_dir) / "JetBrainsMono-Regular.ttf"
    embedded.write_bytes(bytes(request["embedded_font"]))
    paths = [request["font"]] if request["font"] else []
    paths.extend([str(embedded), *request["fallback_fonts"]])
    families = register_fonts(paths)
    if request["font_bold"]:
        bold_family = register_fonts([request["font_bold"]])[0]
        if bold_family != families[0]:
            raise RuntimeError(
                "VTE requires the bold font to belong to the primary font's family"
            )
    terminal = Vte.Terminal()
    font = Pango.FontDescription()
    font.set_family(",".join(families))
    font.set_absolute_size(request["font_size"] * Pango.SCALE)
    terminal.set_font(font)
    # VTE's limit includes the viewport. Keep one extra row to detect overflow.
    terminal.set_scrollback_lines(request["rows"] + 1)
    terminal.set_cursor_blink_mode(Vte.CursorBlinkMode.OFF)
    terminal.set_text_blink_mode(Vte.TextBlinkMode.NEVER)
    terminal.set_audible_bell(False)
    terminal.set_input_enabled(False)

    def color(value):
        rgba = Gdk.RGBA()
        rgba.red, rgba.green, rgba.blue, rgba.alpha = [part / 255 for part in value]
        return rgba

    terminal.set_colors(
        color(request["foreground"]),
        color(request["background"]),
        [color(value) for value in request["palette"]],
    )
    css = Gtk.CssProvider()
    css.load_from_data(b"* { padding: 0; margin: 0; border: 0; }")
    Gtk.StyleContext.add_provider_for_display(
        Gdk.Display.get_default(), css, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
    )
    cols, rows, scale = request["cols"], request["rows"], request["scale"]
    cell_width, cell_height = terminal.get_char_width(), terminal.get_char_height()
    width, height = cols * cell_width, rows * cell_height
    if width * height * scale * scale > 16_000_000:
        raise RuntimeError("VTE capture exceeds 16 megapixels; use fewer rows/columns or a smaller font")
    terminal.set_size(cols, rows)
    terminal.set_size_request(width, height)
    window = Gtk.Window()
    window.set_decorated(False)
    window.set_resizable(False)
    window.set_default_size(width, height)
    window.set_child(terminal)
    loop = GLib.MainLoop()
    result = []
    failures = []
    marker = "termshot-" + uuid.uuid4().hex

    def fail(message):
        failures.append(str(message))
        loop.quit()
        return GLib.SOURCE_REMOVE

    def export(_clock):
        if result or failures:
            return
        try:
            actual_width, actual_height = terminal.get_width(), terminal.get_height()
            if actual_width * actual_height * scale * scale > 16_000_000:
                raise RuntimeError("allocated VTE capture exceeds 16 megapixels")
            snapshot = Gtk.Snapshot()
            snapshot.scale(scale, scale)
            window.snapshot_child(terminal, snapshot)
            node = snapshot.to_node()
            if node is None:
                raise RuntimeError("VTE produced an empty render node")
            bounds = Graphene.Rect()
            bounds.init(0, 0, actual_width * scale, actual_height * scale)
            texture = window.get_renderer().render_texture(node, bounds)
            text = terminal.get_text_format(Vte.Format.TEXT)
            adjustment = terminal.get_vadjustment()
            header = {
                "width": texture.get_width(),
                "height": texture.get_height(),
                "cell_width": terminal.get_char_width() * scale,
                "cell_height": terminal.get_char_height() * scale,
                "text": text.rstrip("\n"),
                "truncated": adjustment.get_upper() > terminal.get_row_count(),
            }
            result.append((header, texture.save_to_png_bytes().get_data()))
            loop.quit()
        except (RuntimeError, ValueError, GLib.Error) as error:
            fail(error)

    def title_changed(_terminal):
        if terminal.get_window_title() != marker:
            return
        clock = terminal.get_frame_clock()
        clock.connect("after-paint", export)
        clock.request_phase(Gdk.FrameClockPhase.AFTER_PAINT)
        terminal.queue_draw()

    def feed(_terminal, _clock):
        if terminal.get_column_count() != cols or terminal.get_row_count() != rows:
            return GLib.SOURCE_CONTINUE
        # A nonprinting parser fence, not a sleep, confirms that all input was parsed.
        fence = b"\x18\x1b\\\x1b[?25l\x1b]2;" + marker.encode() + b"\x07"
        terminal.feed(bytes(request["data"]) + fence)
        return GLib.SOURCE_REMOVE

    terminal.connect("window-title-changed", title_changed)
    terminal.add_tick_callback(feed)
    timeout = GLib.timeout_add_seconds(12, fail, "VTE did not finish parsing and drawing within 12 seconds")
    window.set_visible(True)
    try:
        loop.run()
    finally:
        source = GLib.MainContext.default().find_source_by_id(timeout)
        if source is not None:
            source.destroy()
        window.destroy()
    if failures:
        raise RuntimeError(failures[0])
    if not result:
        raise RuntimeError("VTE returned no capture")
    header, png = result[0]
    sys.stdout.buffer.write(json.dumps(header, ensure_ascii=True).encode() + b"\n")
    sys.stdout.buffer.write(png)


def main():
    runtime = load_runtime()
    if sys.argv[1:] == ["--check"]:
        return
    request = json.load(sys.stdin)
    with tempfile.TemporaryDirectory(prefix="termshot-vte-") as font_dir:
        capture(request, runtime, font_dir)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError) as error:
        print(f"termshot VTE: {error}", file=sys.stderr)
        sys.exit(1)
