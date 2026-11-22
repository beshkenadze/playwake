"""X11 stream controls and diagnostics, composited above the video child."""
import ctypes as c
import time


class ButtonEvent(c.Structure):
    _fields_ = [('type', c.c_int), ('serial', c.c_ulong), ('send_event', c.c_int),
                ('display', c.c_void_p), ('window', c.c_ulong), ('root', c.c_ulong),
                ('subwindow', c.c_ulong), ('time', c.c_ulong), ('x', c.c_int),
                ('y', c.c_int), ('root_x', c.c_int), ('root_y', c.c_int),
                ('state', c.c_uint), ('button', c.c_uint)]


class StreamOverlay:
    def __init__(self, x11, display, parent, create_child, toggle, select, hint_visibility):
        self.x11, self.display, self.parent = x11, display, parent
        self.toggle, self.select = toggle, select
        self.hint_visibility = hint_visibility
        self.hint_pending = False
        self.hint_until = 0.0
        ptr, xid, i, u, d = c.c_void_p, c.c_ulong, c.c_int, c.c_uint, c.c_double

        def bind(library, *definitions):
            for name, result, args in definitions:
                function = getattr(library, name)
                function.restype, function.argtypes = result, args

        bind(x11,
            ('XSelectInput', i, [ptr, xid, c.c_long]),
            ('XUnmapWindow', i, [ptr, xid]),
            ('XGrabButton', i, [ptr, u, u, xid, i, u, i, i, xid, xid]))
        self.shape = c.CDLL('libXext.so.6')
        bind(self.shape,
            ('XShapeCombineRectangles', None, [ptr, xid, i, i, i, ptr, i, i, i]),
            ('XShapeCombineMask', None, [ptr, xid, i, i, i, xid, i]))
        self.cairo = c.CDLL('libcairo.so.2')
        bind(self.cairo,
            ('cairo_xlib_surface_create', ptr, [ptr, xid, ptr, i, i]),
            ('cairo_surface_destroy', None, [ptr]),
            ('cairo_create', ptr, [ptr]),
            ('cairo_destroy', None, [ptr]),
            ('cairo_set_source_rgb', None, [ptr, d, d, d]),
            ('cairo_arc', None, [ptr, d, d, d, d, d]),
            ('cairo_rectangle', None, [ptr, d, d, d, d]),
            ('cairo_fill', None, [ptr]),
            ('cairo_scale', None, [ptr, d, d]),
            ('cairo_move_to', None, [ptr, d, d]),
            ('cairo_select_font_face', None, [ptr, c.c_char_p, i, i]),
            ('cairo_set_font_size', None, [ptr, d]),
            ('cairo_show_text', None, [ptr, c.c_char_p]))

        self.xid = create_child(1, 1)
        screen = x11.XDefaultScreen(display)
        self.visual = x11.XDefaultVisual(display, screen)
        x11.XStoreName(display, self.xid, b'Playwake stream overlay')
        x11.XSelectInput(display, self.xid, (1 << 15) | (1 << 2))  # ExposureMask | ButtonPressMask
        # Right click opens the menu from the video or its parent, with no visible toolbar.
        x11.XGrabButton(display, 3, 1 << 15, parent, 0, (1 << 2) | (1 << 3), 1, 1, 0, 0)
        self.menu = False
        self.diagnostics = False
        self.selection = 0
        self.metrics = ()
        self.compact = '—'
        self.russian = False
        self.xbox_palette = False
        self.console_name = 'Xbox'
        self.rendered = None
        self.geometry = None
        self.scale = 1.0

    def handle(self, event):
        entry = c.cast(event, c.POINTER(ButtonEvent)).contents
        if entry.type == 12 and entry.window == self.xid:  # Expose
            self.rendered = None
        if entry.type != 4:  # ButtonPress
            return
        if entry.button == 3 and entry.window in (self.parent, self.xid):
            self.toggle()
        elif entry.button == 1 and entry.window == self.xid and self.menu:
            x, y = entry.x / self.scale, entry.y / self.scale
            index = int((y - 152) // 58)
            if 22 <= x <= 418 and 152 <= y < 442 and (y - 152) % 58 < 52:
                self.select(index)

    def show_startup_hint(self):
        self.hint_pending = not self.menu

    def update(self, video_size):
        now = time.monotonic()
        if self.menu:
            self.hint_pending = False
            self.hint_until = 0.0
        elif self.hint_pending:
            self.hint_pending = False
            self.hint_until = now + 4.0
        hint = now < self.hint_until
        mode = 'menu' if self.menu else 'hint' if hint else 'hud' if self.diagnostics else None
        was_hint = self.geometry is not None and self.geometry[0] == 'hint'
        if hint != was_hint:
            self.hint_visibility(hint)
        if mode is None:
            if self.geometry is not None:
                self.x11.XUnmapWindow(self.display, self.xid)
                self.x11.XFlush(self.display)
                self.geometry = self.rendered = None
            return

        width, height = video_size
        self.scale = min(2.0, width / 1280, height / 545)
        if self.menu:
            logical_width, logical_height = 440, height / self.scale
        else:
            logical_width, logical_height = (570, 62) if hint else (340, 31)
        w, h = (max(1, round(value * self.scale)) for value in (logical_width, logical_height))
        if self.menu:
            x, y = width - w, 0
        elif hint:
            x, y = (width - w) // 2, round(24 * self.scale)
        else:
            x = y = round(12 * self.scale)
        layout = (mode, x, y, w, h)
        if layout != self.geometry:
            self.x11.XMoveResizeWindow(self.display, self.xid, x, y, w, h)
            # HUD is click-through. The open menu receives pointer events.
            if self.menu:
                self.shape.XShapeCombineMask(self.display, self.xid, 2, 0, 0, 0, 0)
            else:
                self.shape.XShapeCombineRectangles(self.display, self.xid, 2, 0, 0, None, 0, 0, 0)
            self.x11.XMapRaised(self.display, self.xid)
            self.geometry = layout

        frame = (layout, self.selection, self.diagnostics, self.compact, self.metrics if self.menu and self.diagnostics else ())
        if self.rendered != frame:
            self.draw(w, h, logical_width, logical_height, mode)
            self.rendered = frame

    def draw(self, pixel_width, pixel_height, width, height, mode):
        lib = self.cairo
        surface = lib.cairo_xlib_surface_create(self.display, self.xid, self.visual,
                                                pixel_width, pixel_height)
        ctx = lib.cairo_create(surface)
        try:
            lib.cairo_scale(ctx, pixel_width / width, pixel_height / height)

            def rect(x, y, w, h, color):
                lib.cairo_set_source_rgb(ctx, *color)
                lib.cairo_rectangle(ctx, x, y, w, h)
                lib.cairo_fill(ctx)

            def text(x, y, value, size, bold=False, color=(0.93, 0.95, 0.99)):
                lib.cairo_set_source_rgb(ctx, *color)
                lib.cairo_select_font_face(ctx, b'DejaVu Sans', 0, int(bold))
                lib.cairo_set_font_size(ctx, size)
                lib.cairo_move_to(ctx, x, y)
                lib.cairo_show_text(ctx, value.encode())

            muted = (0.63, 0.63, 0.65)
            accent = (0.42, 0.75, 0.29) if self.xbox_palette else (0.55, 0.86, 0.70)

            def label(ru, en):
                return ru if self.russian else en

            if self.menu:
                rect(0, 0, width, height, (0.12, 0.12, 0.13))
                rect(0, 0, 1, height, (0.28, 0.28, 0.29))
                text(32, 53, self.console_name[:26], 26, True)
                lib.cairo_set_source_rgb(ctx, *accent)
                lib.cairo_arc(ctx, 36, 79, 4, 0, 6.284)
                lib.cairo_fill(ctx)
                text(48, 85, label('Стрим идёт', 'Streaming'), 16, color=accent)
                text(32, 117, self.compact, 14, color=muted)
                diagnostics = label('Скрыть статистику', 'Hide statistics') if self.diagnostics else label('Статистика', 'Statistics')
                names = (label('Продолжить', 'Resume'), 'Xbox Guide', diagnostics,
                         label('Переподключить', 'Reconnect'), label('Завершить стрим', 'End stream'))
                for index, name in enumerate(names):
                    y = 152 + 58 * index
                    selected = index == self.selection
                    if selected:
                        rect(20, y - 2, width - 40, 56, accent)
                        rect(23, y + 1, width - 46, 50, (0.24, 0.27, 0.26))
                    color = (0.99, 0.53, 0.51) if index == 4 else (0.93, 0.93, 0.93)
                    text(40, y + 33, name, 18, selected, color)
                if self.diagnostics and height > 645:
                    for index, line in enumerate(self.metrics[1:]):
                        text(32, 467 + 19 * index, line, 13, color=muted)
                text(32, height - 73, label('Ввод на Xbox приостановлен', 'Xbox input paused'), 15, color=muted)
                rect(0, height - 56, width, 1, (0.28, 0.28, 0.29))
                text(120, height - 22, label('A  Выбрать     B  В игру', 'A  Select     B  Back to game'), 15, True)
            elif mode == 'hint':
                rect(0, 0, width, height, (0.12, 0.12, 0.13))
                rect(0, 0, 4, height, accent)
                text(19, 25, label('Меню Playwake', 'Playwake stream menu'), 15, True, accent)
                text(19, 47, label('Удерживайте View + Menu 0,5 с', 'Hold View + Menu for 0.5 s'), 17)
            else:
                rect(0, 0, width, height, (0.08, 0.09, 0.09))
                text(10, 21, self.compact, 14)
        finally:
            lib.cairo_destroy(ctx)
            lib.cairo_surface_destroy(surface)
            self.x11.XFlush(self.display)
