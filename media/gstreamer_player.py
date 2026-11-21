#!/usr/bin/env python3
"""Experimental xHome GStreamer player. Signaling uses JSON lines over stdio."""
import ctypes as c
import hashlib
import ipaddress
import json
import os
import re
import statistics
import struct
import subprocess
import sys
import threading
import time
import uuid

import gi
for namespace in ('Gst', 'GstSdp', 'GstWebRTC', 'GstVideo'):
    gi.require_version(namespace, '1.0')
from gi.repository import GLib, Gst, GstSdp, GstVideo, GstWebRTC
from stream_overlay import StreamOverlay

Gst.init(None)

BUTTONS = {304: 16, 305: 32, 307: 64, 308: 128, 310: 4096, 311: 8192,
           314: 8, 315: 4, 317: 16384, 318: 32768}
AXES = (0, 1, 3, 4)
VIEW_MENU = 8 | 4
GUIDE = 2


def emit(event, **values):
    print(json.dumps(dict(event=event, **values)), flush=True)


def milliseconds():
    return time.monotonic_ns() / 1_000_000


def ntp_unix_ms(timestamp):
    seconds, fraction = struct.unpack('>II', timestamp)
    return (seconds - 2_208_988_800 + fraction / 2**32) * 1000


def checked(promise, name):
    if promise.wait() != Gst.PromiseResult.REPLIED:
        raise RuntimeError(name)
    reply = promise.get_reply()
    if reply is not None and reply.has_field('error'):
        raise RuntimeError(name)
    return reply


class X11WindowAttributes(c.Structure):
    _fields_ = [('background_pixmap', c.c_ulong), ('background_pixel', c.c_ulong),
                ('border_pixmap', c.c_ulong), ('border_pixel', c.c_ulong),
                ('bit_gravity', c.c_int), ('win_gravity', c.c_int), ('backing_store', c.c_int),
                ('backing_planes', c.c_ulong), ('backing_pixel', c.c_ulong),
                ('save_under', c.c_int), ('event_mask', c.c_long),
                ('do_not_propagate_mask', c.c_long), ('override_redirect', c.c_int),
                ('colormap', c.c_ulong), ('cursor', c.c_ulong)]


class XResClientIdSpec(c.Structure):
    _fields_ = [('client', c.c_ulong), ('mask', c.c_uint)]


class XResClientIdValue(c.Structure):
    _fields_ = [('spec', XResClientIdSpec), ('length', c.c_long), ('value', c.c_void_p)]


class X11VideoWindow:
    def __init__(self, parent, on_close, adopt_vulkan=False, overlay_actions=None):
        self.x11 = c.CDLL('libX11.so.6')
        ptr, xid, int_, uint = c.c_void_p, c.c_ulong, c.c_int, c.c_uint
        pointer = c.POINTER
        for name, result, args in (
            ('XOpenDisplay', ptr, [c.c_char_p]),
            ('XDefaultRootWindow', xid, [ptr]),
            ('XDefaultScreen', int_, [ptr]),
            ('XDefaultDepth', int_, [ptr, int_]),
            ('XDefaultVisual', ptr, [ptr, int_]),
            ('XDefaultColormap', xid, [ptr, int_]),
            ('XCreateWindow', xid, [ptr, xid, int_, int_, uint, uint, uint, int_, uint, ptr, xid,
                pointer(X11WindowAttributes)]),
            ('XGetGeometry', int_, [ptr, xid, pointer(xid), pointer(int_), pointer(int_),
                pointer(uint), pointer(uint), pointer(uint), pointer(uint)]),
            ('XCreateSimpleWindow', xid, [ptr, xid, int_, int_, uint, uint, uint, xid, xid]),
            ('XMoveResizeWindow', int_, [ptr, xid, int_, int_, uint, uint]),
            ('XReparentWindow', int_, [ptr, xid, xid, int_, int_]),
            ('XQueryTree', int_, [ptr, xid, pointer(xid), pointer(xid), pointer(pointer(xid)), pointer(uint)]),
            ('XFree', int_, [ptr]),
            ('XStoreName', int_, [ptr, xid, c.c_char_p]),
            ('XMapRaised', int_, [ptr, xid]),
            ('XFlush', int_, [ptr]),
            ('XSync', int_, [ptr, int_]),
            ('XInternAtom', xid, [ptr, c.c_char_p, int_]),
            ('XSetWMProtocols', int_, [ptr, xid, pointer(xid), int_]),
            ('XPending', int_, [ptr]),
            ('XNextEvent', int_, [ptr, ptr]),
            ('XDestroyWindow', int_, [ptr, xid]),
            ('XCloseDisplay', int_, [ptr]),
        ):
            function = getattr(self.x11, name)
            function.restype, function.argtypes = result, args
        self.display = self.x11.XOpenDisplay(None)
        if not self.display:
            raise RuntimeError('Native playback requires an X11 display')
        self.parent = parent or self.x11.XDefaultRootWindow(self.display)
        self.on_close = on_close
        self.size = self.dimensions()
        self.overlay = (StreamOverlay(self.x11, self.display, self.parent, self.create_child, *overlay_actions)
                        if parent and overlay_actions else None)
        self.prior_focus = None
        self.focus_timer = 0
        self.adopt_vulkan = adopt_vulkan
        self.adoption_started = None
        self.vulkan_surface_ready = threading.Event()
        if adopt_vulkan:
            self.xid = 0
            try:
                self.xres = c.CDLL('libXRes.so.1')
            except OSError as error:
                self.x11.XCloseDisplay(self.display)
                raise RuntimeError('Vulkan embedding requires libXRes') from error
            self.xres.XResQueryClientIds.argtypes = [ptr, c.c_long, pointer(XResClientIdSpec),
                                                     pointer(c.c_long), pointer(pointer(XResClientIdValue))]
            self.xres.XResQueryClientIds.restype = int_
            self.xres.XResGetClientPid.argtypes = [pointer(XResClientIdValue)]
            self.xres.XResGetClientPid.restype = int_
            self.xres.XResClientIdsDestroy.argtypes = [c.c_long, pointer(XResClientIdValue)]
            self.xres.XResClientIdsDestroy.restype = None
            self.previous_windows = self.root_windows()
            self.timer = GLib.timeout_add(80, self.update)
            return
        if parent:
            # GPUI's Xwayland window is ARGB32. ximagesink creates its image
            # for the screen's default visual, so the child must use that depth.
            self.xid = self.create_child(*self.size)
        else:
            self.xid = self.x11.XCreateSimpleWindow(self.display, self.parent, 0, 0, *self.size, 0, 0, 0)
        self.x11.XStoreName(self.display, self.xid, b'Greenlight Native Video')
        delete = c.c_ulong(self.x11.XInternAtom(self.display, b'WM_DELETE_WINDOW', 0))
        self.x11.XSetWMProtocols(self.display, self.xid, c.byref(delete), 1)
        self.x11.XMapRaised(self.display, self.xid)
        self.x11.XFlush(self.display)
        if parent:
            emit('video-surface', width=self.size[0], height=self.size[1])
        self.timer = GLib.timeout_add(120, self.update)
        if not parent and os.getenv('GREENLIGHT_GAMESCOPE_FOCUS') == '1':
            self.focus_timer = GLib.timeout_add(200, self.focus)

    def create_child(self, width, height):
        screen = self.x11.XDefaultScreen(self.display)
        attributes = X11WindowAttributes(colormap=self.x11.XDefaultColormap(self.display, screen))
        return self.x11.XCreateWindow(self.display, self.parent, 0, 0, width, height, 0,
            self.x11.XDefaultDepth(self.display, screen), 1, self.x11.XDefaultVisual(self.display, screen),
            (1 << 1) | (1 << 3) | (1 << 13), c.byref(attributes))

    def dimensions(self):
        root = c.c_ulong()
        x, y = c.c_int(), c.c_int()
        width, height, border, depth = (c.c_uint() for _ in range(4))
        self.x11.XGetGeometry(self.display, self.parent, c.byref(root), c.byref(x), c.byref(y),
                              c.byref(width), c.byref(height), c.byref(border), c.byref(depth))
        return max(1, width.value), max(1, height.value)

    def root_windows(self):
        root, parent, count = c.c_ulong(), c.c_ulong(), c.c_uint()
        children = c.POINTER(c.c_ulong)()
        self.x11.XQueryTree(self.display, self.x11.XDefaultRootWindow(self.display),
                            c.byref(root), c.byref(parent), c.byref(children), c.byref(count))
        try:
            return set(children[:count.value]) if children else set()
        finally:
            if children:
                self.x11.XFree(children)

    def owns_window(self, xid):
        spec = XResClientIdSpec(xid, 2)  # XRES_CLIENT_ID_PID_MASK
        count = c.c_long()
        values = c.POINTER(XResClientIdValue)()
        self.xres.XResQueryClientIds(self.display, 1, c.byref(spec), c.byref(count), c.byref(values))
        try:
            return any(self.xres.XResGetClientPid(c.byref(values[i])) == os.getpid()
                       for i in range(count.value)) if values else False
        finally:
            if values:
                self.xres.XResClientIdsDestroy(count, values)

    def embed_vulkan_window(self):
        for xid in self.root_windows() - self.previous_windows:
            if self.owns_window(xid):
                self.xid = xid
                self.size = self.dimensions()
                self.x11.XReparentWindow(self.display, xid, self.parent, 0, 0)
                self.x11.XMoveResizeWindow(self.display, xid, 0, 0, *self.size)
                self.x11.XMapRaised(self.display, xid)
                self.x11.XSync(self.display, False)
                self.vulkan_surface_ready.set()
                emit('vulkan-window-embedded')
                emit('video-surface', width=self.size[0], height=self.size[1])
                return
        if milliseconds() - self.adoption_started > 5000:
            self.adoption_started = None
            emit('error', message='Could not attach the Vulkan video window')
            self.on_close()

    def wait_for_vulkan_surface(self, _pad, info):
        if info.get_event().type != Gst.EventType.CAPS:
            return Gst.PadProbeReturn.OK
        # vulkansink creates its swapchain on the first CAPS event. X11
        # initially creates a 320×240 window. With LSFG it can keep presenting
        # the first swapchain after a resize, so attach and size it first.
        if not self.vulkan_surface_ready.wait(6) or not self.xid:
            GLib.idle_add(self.on_close)
            return Gst.PadProbeReturn.DROP
        return Gst.PadProbeReturn.REMOVE

    def update(self):
        if self.adopt_vulkan and not self.xid:
            if self.adoption_started is not None:
                self.embed_vulkan_window()
            return GLib.SOURCE_CONTINUE
        while self.x11.XPending(self.display):
            event = (c.c_long * 24)()
            self.x11.XNextEvent(self.display, c.byref(event))
            if self.overlay:
                self.overlay.handle(event)
            if c.cast(event, c.POINTER(c.c_int))[0] == 33:  # WM_DELETE_WINDOW client message
                self.on_close()
                return GLib.SOURCE_CONTINUE
        size = self.dimensions()
        # The embedded surface follows the GPUI window size; the top-level
        # surface is sized by Gamescope.
        if size != self.size:
            self.size = size
            self.x11.XMoveResizeWindow(self.display, self.xid, 0, 0, *size)
            self.x11.XFlush(self.display)
            emit('video-surface', width=size[0], height=size[1])
        if self.overlay:
            self.overlay.update(size)
        return GLib.SOURCE_CONTINUE

    @staticmethod
    def xprop(*arguments):
        try:
            return subprocess.run(['xprop', *arguments], capture_output=True,
                                  text=True, timeout=2, check=False).stdout
        except (OSError, subprocess.TimeoutExpired):
            return ''

    def focus(self):
        self.focus_timer = 0
        name = 'GAMESCOPECTRL_BASELAYER_WINDOW'
        property_value = self.xprop('-root', name).rpartition(' = ')[2].strip()
        if property_value.isdecimal():
            self.prior_focus = property_value
            self.xprop('-id', str(self.xid), '-f', 'STEAM_GAME', '32c', '-set', 'STEAM_GAME', '413094')
            self.xprop('-root', '-f', name, '32c', '-set', name, str(self.xid))
        return GLib.SOURCE_REMOVE

    def close(self):
        GLib.source_remove(self.timer)
        if self.focus_timer:
            GLib.source_remove(self.focus_timer)
        name = 'GAMESCOPECTRL_BASELAYER_WINDOW'
        if self.prior_focus is not None and self.xprop('-root', name).rpartition(' = ')[2].strip() == str(self.xid):
            self.xprop('-root', '-f', name, '32c', '-set', name, self.prior_focus)
        if not self.adopt_vulkan:
            self.x11.XDestroyWindow(self.display, self.xid)
        if self.overlay:
            self.x11.XDestroyWindow(self.display, self.overlay.xid)
        self.x11.XCloseDisplay(self.display)


class GamescopeInputFocus:
    def __init__(self, display_name):
        self.x11 = c.CDLL('libX11.so.6')
        ptr, xid = c.c_void_p, c.c_ulong
        for name, result, args in (
            ('XOpenDisplay', ptr, [c.c_char_p]),
            ('XDefaultRootWindow', xid, [ptr]),
            ('XInternAtom', xid, [ptr, c.c_char_p, c.c_int]),
            ('XGetWindowProperty', c.c_int, [ptr, xid, xid, c.c_long, c.c_long, c.c_int, xid,
                c.POINTER(xid), c.POINTER(c.c_int), c.POINTER(xid), c.POINTER(xid), c.POINTER(ptr)]),
            ('XFree', c.c_int, [ptr]),
            ('XCloseDisplay', c.c_int, [ptr]),
        ):
            function = getattr(self.x11, name)
            function.restype, function.argtypes = result, args
        self.display = self.x11.XOpenDisplay(display_name.encode())
        if not self.display:
            raise OSError('Gamescope X11 display unavailable')
        self.root = self.x11.XDefaultRootWindow(self.display)
        self.atom = self.x11.XInternAtom(self.display, b'GAMESCOPE_FOCUSED_APP', 1)

    def focused_app(self):
        if not self.atom:
            return None
        kind, count, remaining = c.c_ulong(), c.c_ulong(), c.c_ulong()
        fmt, data = c.c_int(), c.c_void_p()
        status = self.x11.XGetWindowProperty(self.display, self.root, self.atom, 0, 1, 0, 6,
            c.byref(kind), c.byref(fmt), c.byref(count), c.byref(remaining), c.byref(data))
        try:
            if status == 0 and kind.value == 6 and fmt.value == 32 and count.value == 1 and data:
                # Xlib stores format-32 values in longs, sometimes sign-extended.
                return c.cast(data, c.POINTER(c.c_ulong))[0] & 0xffffffff
            return None
        finally:
            if data:
                self.x11.XFree(data)

    def close(self):
        self.x11.XCloseDisplay(self.display)


class StreamInput:
    def __init__(self):
        self.overlay = False
        self.focused = True
        self.release_gate = False
        self.previous = 0
        self.chord_started = None
        self.both_started = None
        self.pending_buttons = 0
        self.passthrough = False
        self.guide = False
        self.guide_until = None

    def reset_chord(self):
        self.chord_started = None
        self.both_started = None
        self.pending_buttons = 0
        self.passthrough = False

    def close_overlay(self, guide=False):
        if self.overlay:
            self.overlay = False
            self.release_gate = True
            self.guide = guide
            self.guide_until = None
            self.reset_chord()
            emit('stream-overlay', open=False)

    def open_overlay(self):
        if not self.overlay:
            self.overlay = True
            self.reset_chord()
            emit('stream-overlay', open=True)

    def focus(self, active):
        if self.focused != active:
            self.focused = active
            self.release_gate = True
            self.guide = False
            self.guide_until = None
            self.reset_chord()
            emit('stream-input-focus', active=active)

    def route(self, axes, buttons, left, right, now):
        neutral = ([0, 0, 0, 0], 0, 0, 0)
        pressed = buttons & ~self.previous
        self.previous = buttons

        if not self.focused:
            return neutral
        if self.overlay:
            for bit, key in ((1024 | 256, 'left'), (2048 | 512, 'right'), (16, 'activate'), (32, 'back')):
                if pressed & bit:
                    emit('stream-overlay-key', key=key)
                    break
            return neutral
        if self.guide:
            if self.guide_until is None:
                self.guide_until = now + 100
                emit('xbox-guide-pulse', pressed=True)
            if now < self.guide_until:
                return (neutral[0], GUIDE, 0, 0)
            emit('xbox-guide-pulse', pressed=False)
            self.guide = False
            self.guide_until = None
            return neutral
        if self.release_gate:
            if not buttons and max(map(abs, axes)) < 4096 and max(left, right) < 4096:
                self.release_gate = False
                emit('stream-input-rearmed')
            return neutral

        chord = buttons & VIEW_MENU
        if not chord:
            # Replay a short tap that ended while waiting for the second button.
            replay = self.pending_buttons
            self.reset_chord()
            return (axes, buttons | replay, left, right)
        if self.passthrough:
            return (axes, buttons, left, right)
        if self.chord_started is None:
            self.chord_started = now
        self.pending_buttons |= chord
        if chord == VIEW_MENU:
            if self.both_started is None:
                self.both_started = now
            if now - self.both_started >= 500:
                self.open_overlay()
                return neutral
        else:
            self.both_started = None
            if now - self.chord_started >= 90:
                self.passthrough = True
                replay = self.pending_buttons
                self.pending_buttons = 0
                return (axes, buttons | replay, left, right)
        return (axes, buttons & ~VIEW_MENU, left, right)


class Player:
    def __init__(self):
        self.loop = GLib.MainLoop()
        self.pipeline = Gst.Pipeline.new('greenlight-native')
        self.rtc = Gst.ElementFactory.make('webrtcbin')
        if self.rtc is None:
            raise RuntimeError('GStreamer webrtcbin is unavailable')
        self.rtc.set_property('bundle-policy', GstWebRTC.WebRTCBundlePolicy.MAX_BUNDLE)
        self.rtc.set_property('latency', 20)
        self.pipeline.add(self.rtc)
        self.rtc.connect('on-ice-candidate', self.candidate)
        self.rtc.connect('pad-added', self.link_track)
        self.rtc.connect('notify::connection-state', self.connection_changed)
        self.rtc.connect('notify::ice-gathering-state', self.gathering_changed)
        self.pipeline.get_bus().add_signal_watch()
        self.pipeline.get_bus().connect('message', self.bus_message)
        self.candidates = []
        self.channels = {}
        self.sequence = 0
        self.start_time = milliseconds()
        self.width = 0
        self.height = 0
        self.encoded_frames = 0
        self.encoded_bytes = 0
        self.decoded_frames = 0
        self.input_device = None
        self.stream_input = StreamInput()
        self.window_focused = True
        self.gamescope_focus = None
        self.gamescope_timer = None
        self.steam_app_id = None
        self.input_audit = os.getenv('ALLY_INPUT_AUDIT') == '1'
        self.last_input_sample = None
        self.held = {}
        self.axis_info = {}
        self.override = None
        self.trial = None
        self.frame_trial = None
        self.frame_timing = {}
        self.rtp_received = {}
        self.ingress_rtp = {}
        self.benchmark = False
        self.capture_diagnostics = False
        self.capture_extension = None
        self.capture_packets = 0
        self.capture_timestamps = set()
        self.capture_anchors = {}
        self.capture_clock_offset = False
        self.capture_clock_offset_max_ms = 0
        self.rtcp_video_ssrcs = set()
        self.remote_clock_samples = {}
        self.send_time_offered = False
        self.last_video_sample = None
        self.pending_rtp = None
        self.tracks = set()
        self.video_sink = None
        self.renderer = 'auto'
        self.vulkan_timing = {}
        self.mark_vulkan_frame = None
        self.video_queue = None
        self.video_queue_in = 0
        self.video_queue_out = 0
        self.vulkan_conversion = None
        self.lsfg = False
        self.motion_in_parallel = False
        self.queue_times = {}
        self.stage_profile = []
        self.motion_profile = []
        self.window_id = int(os.getenv('GREENLIGHT_VIDEO_XID', '0'), 0)
        self.window = None
        self.hud_timer = None
        self.hud_sample = None
        self.transport_pending = False
        self.hud_rtt = None
        self.hud_jitter = None
        self.hud_loss = None
        self.decoder_name = None
        self.external_pad = None

    def connect_gamepad(self):
        try:
            import evdev
        except ImportError:
            return
        preferred = os.getenv('GREENLIGHT_GAMEPAD')
        for path in ([preferred] if preferred else evdev.list_devices()):
            device = evdev.InputDevice(path)
            codes = device.capabilities(absinfo=False)
            if 304 in codes.get(1, []) and 3 in codes.get(3, []):
                self.input_device = device
                self.axis_info = {axis: device.absinfo(axis) for axis in codes[3]}
                break
            device.close()

    def update_input_focus(self):
        focused_app = self.gamescope_focus.focused_app() if self.gamescope_focus else None
        # Gamescope includes the embedded video child in the shortcut's focus.
        active = self.window_focused if focused_app is None else self.steam_app_id == focused_app
        if active != self.stream_input.focused:
            self.stream_input.focus(active)
            if active and self.input_device:
                try:
                    self.held = {(1, key): 1 for key in self.input_device.active_keys()}
                    self.held.update({(3, axis): self.input_device.absinfo(axis).value for axis in self.axis_info})
                except OSError:
                    self.held.clear()
        return GLib.SOURCE_CONTINUE

    def set_overlay(self, open_, guide=False):
        was_open = self.stream_input.overlay
        if open_:
            self.stream_input.open_overlay()
        else:
            self.stream_input.close_overlay(guide)
        if self.window and self.window.overlay:
            overlay = self.window.overlay
            overlay.menu = self.stream_input.overlay
            if overlay.menu and not was_open:
                overlay.selection = 0

    def toggle_overlay(self):
        if self.stream_input.focused:
            self.set_overlay(not self.stream_input.overlay)

    def set_diagnostics(self):
        overlay = self.window.overlay if self.window else None
        if not overlay:
            return
        overlay.diagnostics = not overlay.diagnostics
        emit('stream-diagnostics', enabled=overlay.diagnostics)
        if overlay.diagnostics:
            self.hud_sample = None
            overlay.metrics = ()
            self.refresh_hud()
            self.hud_timer = GLib.timeout_add_seconds(1, self.refresh_hud)
        elif self.hud_timer is not None:
            GLib.source_remove(self.hud_timer)
            self.hud_timer = None

    def handle(self, command):
        try:
            match command['action']:
                case 'start':
                    self.start(command['bitrate'], command.get('width', 1920), command.get('height', 1080),
                               command.get('benchmark', False), command.get('capture_diagnostics', False),
                               command.get('renderer', 'auto'), command.get('lsfg', False),
                               command.get('interactive', False))
                case 'answer':
                    if self.capture_diagnostics:
                        self.inspect_timestamps(command['sdp'])
                    _, sdp = GstSdp.SDPMessage.new_from_text(command['sdp'])
                    answer = GstWebRTC.WebRTCSessionDescription.new(GstWebRTC.WebRTCSDPType.ANSWER, sdp)
                    promise = Gst.Promise.new()
                    self.rtc.emit('set-remote-description', answer, promise)
                    checked(promise, 'Remote description rejected')
                    emit('answer-set')
                case 'ice':
                    for candidate in command['candidates']:
                        value = candidate.get('candidate', '').removeprefix('a=')
                        if value.startswith('candidate:'):
                            self.rtc.emit('add-ice-candidate', int(candidate.get('sdpMLineIndex') or 0), value)
                case 'pulse':
                    self.override = (command.get('axis', 0), command.get('buttons', 0),
                                     milliseconds() + command.get('duration', 230))
                    if command.get('trial'):
                        self.trial = {'actuatedAt': milliseconds()}
                case 'trial':
                    if self.frame_trial is None:
                        self.begin_trial(command.get('restore_view', False))
                case 'gamepad':
                    axes = command.get('axes')
                    if isinstance(axes, list) and len(axes) == 4:
                        self.external_pad = ([max(-32767, min(32767, int(axis))) for axis in axes],
                                             int(command.get('buttons', 0)) & 0xffff,
                                             max(0, min(65535, int(command.get('lt', 0)))),
                                             max(0, min(65535, int(command.get('rt', 0)))))
                case 'overlay-open':
                    self.set_overlay(True)
                case 'overlay-close':
                    self.set_overlay(False)
                case 'xbox-guide':
                    self.set_overlay(False, guide=True)
                case 'diagnostics-toggle':
                    self.set_diagnostics()
                case 'overlay-selection':
                    if self.window and self.window.overlay:
                        self.window.overlay.selection = int(command['index'])
                case 'input-focus':
                    self.window_focused = bool(command['active'])
                    self.update_input_focus()
                case 'stats':
                    self.stats()
                case 'stop':
                    self.loop.quit()
        except (RuntimeError, ValueError, KeyError, TypeError) as error:
            emit('error', message=str(error))
            self.loop.quit()
        return GLib.SOURCE_REMOVE

    def start(self, bitrate, width, height, benchmark=False, capture_diagnostics=False, renderer='auto',
              lsfg=False, interactive=False):
        self.target_width, self.target_height = int(width), int(height)
        self.bitrate_limit = int(bitrate)
        self.benchmark = benchmark
        self.capture_diagnostics = capture_diagnostics
        self.renderer = renderer
        self.lsfg = lsfg
        if renderer == 'vulkan' and not all(Gst.ElementFactory.find(name) for name in ('vulkanupload', 'vulkansink')):
            raise RuntimeError('Vulkan upload and video sink are required')
        if lsfg and not all(Gst.ElementFactory.find(name) for name in ('vah264dec', 'vapostproc', 'vulkancolorconvert')):
            raise RuntimeError('LSFG requires the VA-API decoder and Vulkan color converter')
        if renderer == 'vulkan' and (library := os.getenv('ALLY_VULKAN_PRESENT_LIBRARY')):
            self.mark_vulkan_frame = c.CDLL(library).ally_present_mark
            self.mark_vulkan_frame.argtypes = [c.c_uint64]
            self.mark_vulkan_frame.restype = None
        if benchmark:
            rtpbin = self.rtc.get_by_name('rtpbin')
            if rtpbin:
                rtpbin.connect('new-jitterbuffer', self.watch_ingress)
                if self.capture_diagnostics:
                    rtpbin.connect('pad-added', self.watch_rtcp_pad)
        self.connect_gamepad()
        if (control_display := os.getenv('ALLY_GAMESCOPE_CONTROL_DISPLAY')) and (
            app_id := os.getenv('SteamAppId', '')
        ).isdecimal() and int(app_id) != 0:
            self.steam_app_id = int(app_id)
            try:
                self.gamescope_focus = GamescopeInputFocus(control_display)
            except OSError:
                pass  # GPUI still supplies window focus on non-Gamescope sessions.
            else:
                self.update_input_focus()
                self.gamescope_timer = GLib.timeout_add(16, self.update_input_focus)
        if not self.window_id and sys.platform.startswith('linux') and os.getenv('DISPLAY'):
            parent = int(os.getenv('GREENLIGHT_VIDEO_PARENT_XID', '0'), 0)
            if renderer != 'vulkan' or parent:
                actions = (self.toggle_overlay, lambda index: emit('stream-overlay-key', key='select', index=index))
                self.window = X11VideoWindow(parent, self.loop.quit, adopt_vulkan=renderer == 'vulkan',
                                             overlay_actions=actions if interactive else None)
                self.window_id = self.window.xid
        direction = GstWebRTC.WebRTCRTPTransceiverDirection.RECVONLY
        for caps in (
            'application/x-rtp,media=audio,encoding-name=OPUS,clock-rate=48000,payload=111,encoding-params=(string)2',
            'application/x-rtp,media=video,encoding-name=H264,clock-rate=90000,payload=103,packetization-mode=(string)1,profile-level-id=(string)42001f',
        ):
            self.rtc.emit('add-transceiver', direction, Gst.Caps.from_string(caps))
        if self.pipeline.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
            raise RuntimeError('Cannot start pipeline')
        for label, protocol in [('input', '1.0'), ('control', 'controlV1'), ('message', 'messageV1'), ('chat', 'chatV1')]:
            options = Gst.Structure.new_empty('application/data-channel')
            options.set_value('protocol', protocol)
            options.set_value('ordered', True)
            channel = self.rtc.emit('create-data-channel', label, options)
            if channel is None:
                raise RuntimeError('Cannot create WebRTC data channel')
            self.channels[label] = channel
        message = self.channels['message']
        message.connect('on-open', lambda _: self.send_json('message',
            {'type': 'Handshake', 'version': 'messageV1', 'id': str(uuid.uuid4()), 'cv': ''}))
        message.connect('on-message-string', lambda _, data: self.message(data))
        message.connect('on-message-data', lambda _, data: self.message(data.get_data().decode()))
        promise = Gst.Promise.new()
        self.rtc.emit('create-offer', None, promise)
        reply = checked(promise, 'Offer creation failed')
        offer = reply.get_value('offer')
        sdp = offer.sdp.as_text()
        lines = sdp.splitlines()
        video = next(i for i, line in enumerate(lines) if line.startswith('m=video '))
        end = next((i for i in range(video + 1, len(lines)) if lines[i].startswith('m=')), len(lines))
        # Place bandwidth ahead of media attributes (RFC 4566 field order).
        before_attributes = next((i for i in range(video + 1, end) if lines[i].startswith('a=')), end)
        lines.insert(before_attributes, f'b=AS:{int(bitrate)}')
        if self.capture_diagnostics:
            used = {int(match[1]) for line in lines if (match := re.match(r'a=extmap:(\d+)', line))}
            extension = next((value for value in range(14, 0, -1) if value not in used), None)
            if extension is not None:
                lines.insert(before_attributes + 1, f'a=extmap:{extension}/recvonly http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time')
        sdp = '\r\n'.join(lines) + '\r\n'
        _, description = GstSdp.SDPMessage.new_from_text(sdp)
        offer = GstWebRTC.WebRTCSessionDescription.new(GstWebRTC.WebRTCSDPType.OFFER, description)
        promise = Gst.Promise.new()
        self.rtc.emit('set-local-description', offer, promise)
        checked(promise, 'Local description rejected')
        emit('offer', sdp=sdp)

    def inspect_timestamps(self, sdp):
        video = False
        extensions = {}
        for line in sdp.splitlines():
            if line.startswith('m='):
                video = line.startswith('m=video ')
            if video and (match := re.fullmatch(r'a=extmap:(\d+)(?:/\w+)?\s+(\S+).*', line)):
                extensions[match[2].rsplit('/', 1)[-1]] = int(match[1])
        self.capture_extension = extensions.get('abs-capture-time')
        self.send_time_offered = 'abs-send-time' in extensions
        emit('capture-negotiation', capture=self.capture_extension is not None, sendTime=self.send_time_offered)

    def candidate(self, _, index, candidate):
        self.candidates.append({'candidate': candidate, 'sdpMLineIndex': index})

    def gathering_changed(self, *_):
        if self.rtc.get_property('ice-gathering-state') == GstWebRTC.WebRTCICEGatheringState.COMPLETE:
            emit('candidates', candidates=self.candidates)

    def connection_changed(self, *_):
        emit('connection', state=self.rtc.get_property('connection-state').value_nick)

    def send_json(self, label, data):
        channel = self.channels[label]
        channel.emit('send-data', GLib.Bytes.new(json.dumps(data, separators=(',', ':')).encode()))

    def message(self, data):
        if json.loads(data).get('type') == 'HandshakeAck':
            GLib.idle_add(self.handshake_ack)

    def handshake_ack(self):
        width, height = self.target_width, self.target_height
        dimensions = {'width': width, 'height': height}
        self.send_json('control', {'message': 'authorizationRequest',
            'accessKey': '4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E'})
        self.send_json('control', {'message': 'gamepadChanged', 'gamepadIndex': 0, 'wasAdded': True})
        self.send_json('control', {'message': 'VideoPreference', 'type': 'VideoPreference',
            'resolution': dimensions, 'dimensions': dimensions, 'maxFPS': 60})
        self.send_json('control', {'message': 'userRequestedResolutionUpdate', 'resolutionAlias': str(height)})
        for target, content in (
            ('systemUi/configuration', {'version': [0, 1, 0], 'systemUis': []}),
            ('properties/clientappinstallidchanged', {'clientAppInstallId': str(uuid.uuid4())}),
            ('characteristics/orientationchanged', {'orientation': 0}),
            ('characteristics/touchinputenabledchanged', {'touchInputEnabled': False}),
            ('characteristics/clientdevicecapabilities', {}),
            ('characteristics/dimensionschanged', {'horizontal': width, 'vertical': height,
                'preferredWidth': width, 'preferredHeight': height, 'safeAreaLeft': 0, 'safeAreaTop': 0,
                'safeAreaRight': width, 'safeAreaBottom': height, 'supportsCustomResolution': True}),
        ):
            self.send_json('message', {'type': 'Message', 'target': '/streaming/' + target,
                'content': json.dumps(content), 'id': str(uuid.uuid4()), 'cv': ''})
        self.channels['input'].emit('send-data', GLib.Bytes.new(struct.pack('<HIdB', 8, 0, self.start_time, 2)))
        GLib.timeout_add(8, self.send_input)
        emit('input-ready')
        return GLib.SOURCE_REMOVE

    def send_input(self):
        channel = self.channels['input']
        if channel.get_property('ready-state') != GstWebRTC.WebRTCDataChannelState.OPEN:
            return GLib.SOURCE_REMOVE
        polled = milliseconds()
        if self.input_device:
            try:
                for event in self.input_device.read():
                    self.held[event.type, event.code] = event.value
            except BlockingIOError:
                pass
            except OSError:
                self.input_device.close()
                self.input_device = None
                self.held.clear()
                self.axis_info.clear()

        def value(code):
            info = self.axis_info.get(code)
            if not self.input_device or not info or info.max <= info.min:
                return 0
            raw = self.held.get((3, code), info.value)
            return max(-32767, min(32767, round((2 * (raw - info.min) / (info.max - info.min) - 1) * 32767)))

        axes = [value(code) for code in AXES]
        buttons = sum(mask for code, mask in BUTTONS.items() if self.held.get((1, code)))
        buttons |= {-1: 1024, 1: 2048}.get(self.held.get((3, 16)), 0)
        buttons |= {-1: 256, 1: 512}.get(self.held.get((3, 17)), 0)
        left_trigger = value(2) + 32767 if 2 in self.axis_info else 0
        right_trigger = value(5) + 32767 if 5 in self.axis_info else 0
        if self.external_pad is not None:
            axes, buttons, left_trigger, right_trigger = self.external_pad
            axes = list(axes)
        if self.override:
            axis, virtual_buttons, expires = self.override
            if polled < expires:
                axes[2] = round(max(-1, min(1, axis)) * 32767)
                buttons = virtual_buttons
            else:
                self.override = None
        axes, buttons, left_trigger, right_trigger = self.stream_input.route(axes, buttons, left_trigger, right_trigger, polled)
        if self.window and self.window.overlay and self.window.overlay.menu != self.stream_input.overlay:
            self.window.overlay.menu = self.stream_input.overlay
            self.window.overlay.selection = 0
        if self.input_audit:
            sample = (buttons, any(axes), bool(left_trigger or right_trigger))
            if self.last_input_sample != sample:
                self.last_input_sample = sample
                emit('stream-input-audit', buttons=buttons, axisActive=sample[1], triggerActive=sample[2])
        self.sequence = (self.sequence + 1) & 0xffffffff
        packet = struct.pack('<HIdBBHhhhhHHII', 2, self.sequence, polled - self.start_time,
            1, 0, buttons, axes[0], -axes[1], axes[2], -axes[3],
            left_trigger, right_trigger, 0, 0)
        before = channel.get_property('buffered-amount') if self.trial and axes[2] else 0
        channel.emit('send-data', GLib.Bytes.new(packet))
        if self.trial and axes[2]:
            timing = dict(**self.trial, polledAt=polled, sentAt=milliseconds(),
                          queuedBeforeBytes=before, queuedAfterBytes=channel.get_property('buffered-amount'))
            if self.frame_trial is not None:
                self.frame_trial['input'] = timing
            emit('input-timing', **timing)
            self.trial = None
        return GLib.SOURCE_CONTINUE

    def link_track(self, _, pad):
        caps = pad.get_current_caps() or pad.query_caps(None)
        media = caps.get_structure(0).get_string('media')
        if media in self.tracks:
            return
        if media == 'video':
            if sys.platform == 'win32':
                choices = ('d3d11h264dec', 'd3d12h264dec', 'avdec_h264')
            elif sys.platform == 'darwin':
                choices = ('vtdec_hw', 'vtdec', 'avdec_h264')
            else:
                choices = ('vah264dec', 'avdec_h264')
            self.decoder_name = next((name for name in choices if Gst.ElementFactory.find(name)), None)
            if not self.decoder_name:
                emit('error', message='GStreamer H.264 decoder unavailable')
                return
            if self.renderer == 'vulkan':
                download = 'vapostproc ! video/x-raw,format=NV12 ! ' if self.decoder_name == 'vah264dec' else ''
                queue = 'queue name=video_queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream ! '
                self.motion_in_parallel = (self.benchmark and self.decoder_name == 'vah264dec'
                                           and os.getenv('ALLY_VULKAN_MOTION_PROBE') != 'inline')
                convert_on_gpu = (self.decoder_name == 'vah264dec' and os.getenv('ALLY_VULKAN_CONVERSION') != 'cpu'
                                  and Gst.ElementFactory.find('vulkancolorconvert') is not None)
                if convert_on_gpu:
                    self.vulkan_conversion = 'gpu_nv12'
                    conversion = ('vulkanupload name=upload ! vulkancolorconvert ! '
                                  'video/x-raw(memory:VulkanImage),format=BGRA ! vulkansink')
                else:
                    self.vulkan_conversion = 'cpu_bgra'
                    conversion = 'videoconvert ! video/x-raw,format=BGRA ! vulkanupload name=upload ! vulkansink'
                tap = 'tee name=frame_tap ! ' if self.motion_in_parallel else ''
                video_output = queue + download + tap + conversion
                options = 'sync=false force-aspect-ratio=true'
            elif self.window_id:
                download = 'vapostproc ! video/x-raw,format=NV12 ! ' if self.decoder_name == 'vah264dec' else ''
                video_output = download + 'videoconvert ! video/x-raw,format=BGRx ! ximagesink'
                options = 'sync=false force-aspect-ratio=true'
            else:
                video_output = 'videoconvert ! autovideosink'
                options = 'sync=false'
            motion_branch = (
                ' frame_tap. ! queue name=motion_queue max-size-buffers=1 max-size-time=0 max-size-bytes=0 leaky=downstream '
                '! videoscale method=0 ! video/x-raw,format=NV12,width=640,height=360 '
                '! fakesink name=motion_sink sync=false async=false'
            ) if self.motion_in_parallel else ''
            branch = Gst.parse_bin_from_description(
                f'rtph264depay name=depay ! h264parse name=parser ! {self.decoder_name} name=decoder ! '
                f'{video_output} name=video_sink {options}{motion_branch}', True)
            self.video_sink = branch.get_by_name('video_sink')
            if self.window_id:
                GstVideo.VideoOverlay.set_window_handle(self.video_sink, self.window_id)
            branch.get_by_name('depay').get_static_pad('sink').add_probe(Gst.PadProbeType.BUFFER, self.receive_rtp)
            sink_pad = self.video_sink.get_static_pad('sink')
            if self.window and self.window.adopt_vulkan:
                sink_pad.add_probe(Gst.PadProbeType.EVENT_DOWNSTREAM, self.window.wait_for_vulkan_surface)
            if self.lsfg:
                sink_pad.add_probe(Gst.PadProbeType.BUFFER, self.check_lsfg_layer)
            if self.renderer == 'vulkan':
                self.video_queue = branch.get_by_name('video_queue')
                self.video_queue.get_static_pad('sink').add_probe(Gst.PadProbeType.BUFFER, self.queue_input)
                self.video_queue.get_static_pad('src').add_probe(Gst.PadProbeType.BUFFER, self.queue_output)
            if self.renderer == 'vulkan' and self.benchmark:
                branch.get_by_name('upload').get_static_pad('sink').add_probe(Gst.PadProbeType.BUFFER, self.before_vulkan_upload)
                sink_pad.add_probe(Gst.PadProbeType.BUFFER, self.vulkan_frame)
                if self.motion_in_parallel:
                    branch.get_by_name('motion_queue').get_static_pad('sink').add_probe(Gst.PadProbeType.BUFFER, self.motion_gate)
                    branch.get_by_name('motion_sink').get_static_pad('sink').add_probe(Gst.PadProbeType.BUFFER, self.analyze_motion)
            else:
                sink_pad.add_probe(Gst.PadProbeType.BUFFER, self.present_frame)
            branch.get_by_name('parser').get_static_pad('src').add_probe(Gst.PadProbeType.BUFFER, self.encoded)
            branch.get_by_name('decoder').get_static_pad('src').add_probe(Gst.PadProbeType.BUFFER, self.decoded)
        elif media == 'audio':
            audio_sink = 'pulsesink' if sys.platform.startswith('linux') and Gst.ElementFactory.find('pulsesink') else 'autoaudiosink'
            branch = Gst.parse_bin_from_description(
                f'rtpopusdepay ! opusdec ! audioconvert ! audioresample ! {audio_sink} sync=true', True)
        else:
            return
        self.tracks.add(media)
        if media == 'video' and self.window and self.window.adopt_vulkan:
            self.window.adoption_started = milliseconds()
        self.pipeline.add(branch)
        pad.link(branch.get_static_pad('sink'))
        branch.sync_state_with_parent()

    def check_lsfg_layer(self, *_):
        try:
            with open('/proc/self/maps', encoding='utf-8') as mappings:
                loaded = any('liblsfg-vk-layer.so' in line for line in mappings)
        except OSError:
            loaded = False
        if loaded:
            emit('lsfg-ready')
        else:
            emit('error', message='The lsfg-vk 2.x layer was not loaded by Vulkan')
            GLib.idle_add(self.loop.quit)
        return Gst.PadProbeReturn.REMOVE

    def receive_rtp(self, _, info):
        buffer = info.get_buffer()
        if self.capture_diagnostics:
            self.capture_packets += 1
            key = self.rtp_key(buffer)
            if key is not None:
                rtp, ssrc = struct.unpack('>II', key)
                self.rtcp_video_ssrcs.add(ssrc)
                stamp = self.read_capture_extension(buffer) if self.capture_extension is not None else None
                if stamp:
                    self.capture_timestamps.add(key)
                    offset = struct.unpack('>q', stamp[8:])[0] * 1000 / 2**32 if len(stamp) == 16 else 0
                    self.capture_clock_offset |= len(stamp) == 16
                    self.capture_clock_offset_max_ms = max(self.capture_clock_offset_max_ms, abs(offset))
                    self.capture_anchors[ssrc] = (rtp, ntp_unix_ms(stamp[:8]) - offset)
        self.pending_rtp = (buffer.pts, milliseconds())
        if buffer.pts != Gst.CLOCK_TIME_NONE:
            first_packet = self.ingress_rtp.get(self.rtp_key(buffer))
            timing = {
                'receivedAt': self.pending_rtp[1] if first_packet is None else first_packet,
                'preJitter': first_packet is not None,
            }
            if self.capture_diagnostics and key is not None and (anchor := self.capture_anchors.get(ssrc)):
                ticks = ((rtp - anchor[0] + 2**31) % 2**32) - 2**31
                if abs(ticks) <= 2 * 90_000:
                    estimated_capture_wall = anchor[1] + ticks / 90
                    timing['captureAt'] = estimated_capture_wall - (time.time_ns() / 1_000_000 - milliseconds())
                    reports = self.remote_clock_samples.get(ssrc)
                    if reports:
                        timing['srWallGapMs'] = statistics.median(reports[-21:])
            self.rtp_received[buffer.pts] = timing
            if len(self.rtp_received) > 180:
                self.rtp_received.pop(next(iter(self.rtp_received)))
        return Gst.PadProbeReturn.OK

    def read_capture_extension(self, buffer):
        if buffer.get_size() < 16:
            return None
        first = buffer.extract_dup(0, 1)[0]
        offset = 12 + 4 * (first & 15)
        if not first & 16 or offset + 4 > buffer.get_size():
            return None
        profile, words = struct.unpack('>HH', buffer.extract_dup(offset, 4))
        if words > 256 or offset + 4 + 4 * words > buffer.get_size():
            return None
        ext = buffer.extract_dup(offset + 4, 4 * words)
        i = 0
        while i < len(ext):
            header = ext[i]
            i += 1
            if not header:
                continue
            if profile == 0xbede:
                ext_id, size = header >> 4, (header & 15) + 1
                if ext_id == 15:
                    break
            elif profile & 0xfff0 == 0x1000 and i < len(ext):
                ext_id, size = header, ext[i]
                i += 1
            else:
                break
            if ext_id == self.capture_extension and size in (8, 16) and i + size <= len(ext):
                return ext[i:i + size]
            i += size
        return None

    def watch_rtcp_pad(self, _, pad):
        if pad.get_name().startswith('recv_rtcp_sink_'):
            pad.add_probe(Gst.PadProbeType.BUFFER, self.rtcp_packet)

    def rtcp_packet(self, _, info):
        buffer = info.get_buffer()
        packet = buffer.extract_dup(0, buffer.get_size())
        now = time.time_ns() / 1_000_000
        offset = 0
        while offset + 4 <= len(packet):
            flags, packet_type, words = struct.unpack_from('>BBH', packet, offset)
            size = (words + 1) * 4
            if flags >> 6 != 2 or offset + size > len(packet):
                break
            if packet_type == 200 and size >= 28:
                ssrc = struct.unpack_from('>I', packet, offset + 4)[0]
                wall_gap = now - ntp_unix_ms(packet[offset + 8:offset + 16])
                self.remote_clock_samples.setdefault(ssrc, []).append(wall_gap)
            offset += size
        return Gst.PadProbeReturn.OK

    @staticmethod
    def rtp_key(buffer):
        if buffer.get_size() >= 12:
            return buffer.extract_dup(4, 8)
        return None

    def watch_ingress(self, _, jitter, _session, _ssrc):
        jitter.get_static_pad('sink').add_probe(Gst.PadProbeType.BUFFER, self.ingress_packet)

    def ingress_packet(self, _, info):
        key = self.rtp_key(info.get_buffer())
        if key is not None:
            self.ingress_rtp.setdefault(key, milliseconds())
            if len(self.ingress_rtp) > 512:
                self.ingress_rtp.pop(next(iter(self.ingress_rtp)))
        return Gst.PadProbeReturn.OK

    def encoded(self, _, info):
        self.encoded_frames += 1
        buffer = info.get_buffer()
        self.encoded_bytes += buffer.get_size()
        received = self.rtp_received.get(buffer.pts)
        if received is not None:
            self.frame_timing[buffer.pts] = dict(received)
        if len(self.frame_timing) > 180:
            self.frame_timing.pop(next(iter(self.frame_timing)))
        return Gst.PadProbeReturn.OK

    def decoded(self, pad, probe):
        self.decoded_frames += 1
        timing = self.frame_timing.get(probe.get_buffer().pts)
        if timing is not None:
            timing['decodedAt'] = milliseconds()
        caps = pad.get_current_caps().get_structure(0)
        dimensions = caps.get_value('width'), caps.get_value('height')
        if dimensions != (self.width, self.height):
            self.width, self.height = dimensions
            emit('video', width=self.width, height=self.height, decoder=self.decoder_name,
                 renderer=self.video_sink.get_factory().get_name() if self.video_sink else None,
                 conversion=self.vulkan_conversion,
                 motionProbe=('parallel' if self.motion_in_parallel else 'inline') if self.benchmark else None)
        return Gst.PadProbeReturn.OK

    def begin_trial(self, restore_view=False):
        try:
            import numpy
        except ImportError:
            emit('trial-error', message='Native benchmarking requires NumPy')
            return
        trial = {'numpy': numpy, 'previous': None, 'candidate': None, 'frames': 0,
                 'started': None, 'input': None, 'baselineMoved': False, 'scene': None}
        self.frame_trial = trial

        def return_camera():
            now = milliseconds()
            self.override = (-0.9, 0, now + 230)
            return GLib.SOURCE_REMOVE

        def actuate():
            if self.frame_trial is not trial:
                return GLib.SOURCE_REMOVE
            if trial['baselineMoved'] or trial['frames'] < 4:
                self.finish_trial(trial, error='Hold the camera still and try again')
            else:
                now = milliseconds()
                trial['started'] = now
                self.trial = {'actuatedAt': now}
                self.override = (0.9, 0, now + 230)
                if restore_view:
                    GLib.timeout_add(550, return_camera)
            return GLib.SOURCE_REMOVE

        GLib.timeout_add(300, actuate)
        GLib.timeout_add(1500, lambda: self.finish_trial(trial, error='No confirmed camera turn'))

    def finish_trial(self, trial, result=None, error=None):
        if self.frame_trial is trial:
            self.frame_trial = None
            if error:
                emit('trial-error', message=error)
            else:
                emit('trial', **result)
        return GLib.SOURCE_REMOVE

    def video_timing(self, timing, now):
        if self.benchmark and timing and (self.last_video_sample is None or now - self.last_video_sample >= 250):
            self.last_video_sample = now
            emit('video-timing', receivedAt=timing['receivedAt'], sinkAt=now, preJitter=timing['preJitter'],
                 captureAt=timing.get('captureAt'), srWallGapMs=timing.get('srWallGapMs'))

    def queue_input(self, _, probe):
        self.video_queue_in += 1
        if not self.benchmark:
            return Gst.PadProbeReturn.OK
        key = probe.get_buffer().pts
        if key != Gst.CLOCK_TIME_NONE:
            self.queue_times[key] = {'entered': milliseconds()}
            if len(self.queue_times) > 180:
                self.queue_times.pop(next(iter(self.queue_times)))
        return Gst.PadProbeReturn.OK

    def queue_output(self, _, probe):
        self.video_queue_out += 1
        if not self.benchmark:
            return Gst.PadProbeReturn.OK
        stage = self.queue_times.get(probe.get_buffer().pts)
        if stage is not None:
            stage['dequeued'] = milliseconds()
        return Gst.PadProbeReturn.OK

    def before_vulkan_upload(self, pad, probe):
        stage = self.queue_times.get(probe.get_buffer().pts)
        if stage is None:
            return Gst.PadProbeReturn.OK if self.motion_in_parallel else self.present_frame(pad, probe)
        begin = milliseconds()
        active = self.frame_trial is not None
        result = Gst.PadProbeReturn.OK if self.motion_in_parallel else self.present_frame(pad, probe)
        stage['pre_upload'] = begin
        stage['probe_ms'] = milliseconds() - begin if active and not self.motion_in_parallel else 0
        stage['motion_probe'] = active
        return result

    def motion_gate(self, *_):
        return Gst.PadProbeReturn.OK if self.frame_trial is not None else Gst.PadProbeReturn.DROP

    def analyze_motion(self, pad, probe):
        begin = milliseconds()
        result = self.present_frame(pad, probe)
        self.motion_profile.append(milliseconds() - begin)
        return result

    def vulkan_frame(self, _, probe):
        buffer = probe.get_buffer()
        now = milliseconds()
        if self.mark_vulkan_frame:
            self.mark_vulkan_frame(buffer.pts)
        stage = self.queue_times.pop(buffer.pts, None)
        if stage is not None and all(key in stage for key in ('dequeued', 'pre_upload')):
            self.stage_profile.append((
                stage['dequeued'] - stage['entered'],
                stage['pre_upload'] - stage['dequeued'],
                now - stage['pre_upload'] - stage['probe_ms'],
                stage['probe_ms'],
                stage['motion_probe'],
            ))
        self.vulkan_timing[buffer.pts] = now
        if len(self.vulkan_timing) > 180:
            self.vulkan_timing.pop(next(iter(self.vulkan_timing)))
        self.video_timing(self.frame_timing.get(buffer.pts), now)
        return Gst.PadProbeReturn.OK

    def present_frame(self, pad, probe):
        trial = self.frame_trial
        if trial is None and not self.benchmark:
            return Gst.PadProbeReturn.OK
        now = milliseconds()
        buffer = probe.get_buffer()
        timing = self.frame_timing.get(buffer.pts)
        if self.renderer != 'vulkan':
            self.video_timing(timing, now)
        if trial is None:
            return Gst.PadProbeReturn.OK
        info = GstVideo.VideoInfo.new_from_caps(pad.get_current_caps())
        meta = GstVideo.buffer_get_video_meta(buffer)
        stride = meta.stride[0] if meta else info.stride[0]
        offset = meta.offset[0] if meta else info.offset[0]
        ok, mapped = buffer.map(Gst.MapFlags.READ)
        if not ok:
            return Gst.PadProbeReturn.OK
        np = trial['numpy']
        try:
            y = (np.linspace(0.14, 0.61, 110, endpoint=False) * info.height).astype(int)
            x = (np.linspace(0.10, 0.90, 320, endpoint=False) * info.width).astype(int)
            if info.finfo.name == 'NV12':
                image = np.ndarray((info.height, info.width), dtype=np.uint8, buffer=mapped.data,
                                   strides=(stride, 1), offset=offset)
                pixels = image[y[:, None], x, None].astype(np.int16)
            else:
                image = np.ndarray((info.height, info.width, 4), dtype=np.uint8, buffer=mapped.data,
                                   strides=(stride, 4, 1), offset=offset)
                pixels = image[y[:, None], x, :2].astype(np.int16)
        finally:
            buffer.unmap(mapped)
        trial['frames'] += 1
        previous = trial['previous']
        trial['previous'] = pixels
        if previous is None:
            return Gst.PadProbeReturn.OK
        middle = previous[5:-5:4, 13:-13:3]
        errors = [int(np.abs(middle - pixels[5:-5:4, 13 + shift:320 - 13 + shift:3]).sum())
                  for shift in range(-12, 13)]
        best = min(range(len(errors)), key=errors.__getitem__)
        shifted = errors[12] > 1600 * pixels.shape[-1] and (errors[12] - errors[best]) / errors[12] >= 0.18
        if trial['started'] is None:
            trial['baselineMoved'] |= shifted
            luminance = pixels.mean(axis=2)
            tiles = luminance.reshape(10, 11, 16, 20).mean(axis=(1, 3))
            trial['scene'] = np.packbits(tiles[:, 1:] > tiles[:, :-1]).tobytes().hex()
        elif trial['input'] and shifted and best < 12:
            first = trial['candidate']
            if first and now - first['observedAt'] <= 85:
                sink_at = (self.vulkan_timing.get(first['pts']) if self.renderer == 'vulkan' else first['observedAt'])
                if sink_at is not None:
                    event = dict(trial['input'], **first['timing'], sinkAt=sink_at, scene=trial['scene'],
                                 framePts=first['pts'])
                    GLib.idle_add(self.finish_trial, trial, event)
                elif self.motion_in_parallel and not trial.get('waitingForSink'):
                    trial['waitingForSink'] = True
                    GLib.timeout_add(3, self.confirm_vulkan_turn, trial, first)
            if timing:
                trial['candidate'] = {'timing': dict(timing), 'pts': buffer.pts, 'observedAt': now}
        elif shifted and best > 12:
            trial['candidate'] = None
        return Gst.PadProbeReturn.OK

    def confirm_vulkan_turn(self, trial, first):
        if self.frame_trial is not trial:
            return GLib.SOURCE_REMOVE
        if (sink_at := self.vulkan_timing.get(first['pts'])) is not None:
            event = dict(trial['input'], **first['timing'], sinkAt=sink_at, scene=trial['scene'],
                         framePts=first['pts'])
            return self.finish_trial(trial, event)
        if milliseconds() - first['observedAt'] < 100:
            return GLib.SOURCE_CONTINUE
        trial['waitingForSink'] = False
        return GLib.SOURCE_REMOVE

    def stats(self):
        stats = self.video_sink.get_property('stats') if self.video_sink and self.video_sink.find_property('stats') else None
        emit('stats', time=milliseconds(), width=self.width, height=self.height,
            encodedFrames=self.encoded_frames, encodedBytes=self.encoded_bytes,
            decodedFrames=self.decoded_frames, renderedFrames=stats.get_value('rendered') if stats else 0,
            sinkDropped=stats.get_value('dropped') if stats else 0,
            queueInput=self.video_queue_in, queueOutput=self.video_queue_out,
            queueDepth=self.video_queue.get_property('current-level-buffers') if self.video_queue else 0)
        if self.stage_profile:
            samples, self.stage_profile = self.stage_profile, []
            motion, self.motion_profile = self.motion_profile, []
            emit('vulkan-stage-profile', time=milliseconds(), width=self.width, height=self.height,
                 samples=samples, motion=motion)
        if self.capture_diagnostics:
            self.transport_stats()

    def refresh_hud(self):
        overlay = self.window.overlay
        now = milliseconds()
        sample = (now, self.decoded_frames, self.encoded_bytes)
        if self.hud_sample and self.width and self.height:
            previous_time, previous_frames, previous_bytes = self.hud_sample
            seconds = (now - previous_time) / 1000
            fps = (sample[1] - previous_frames) / seconds
            mbps = (sample[2] - previous_bytes) * 8 / seconds / 1_000_000
            sink_stats = self.video_sink.get_property('stats') if self.video_sink else None
            dropped = sink_stats.get_value('dropped') if sink_stats else 0
            depth = self.video_queue.get_property('current-level-buffers') if self.video_queue else 0
            queue_drops = max(0, self.video_queue_in - self.video_queue_out - depth)
            size = self.window.size
            ms = lambda value: f'{value:.0f} ms' if value is not None else '—'
            loss = self.hud_loss if self.hud_loss is not None else '—'
            overlay.metrics = (
                f'Video {self.width}×{self.height} · {fps:.1f} decoded FPS',
                f'Viewport {size[0]}×{size[1]} · {self.decoder_name} · LSFG {"on" if self.lsfg else "off"}',
                f'Video {mbps:.1f} Mb/s · limit {self.bitrate_limit / 1000:g} Mb/s',
                f'RTT {ms(self.hud_rtt)} · jitter {ms(self.hud_jitter)} · RTP lost {loss}',
                f'Queue {depth} · queue drops ≈{queue_drops} · sink drops {dropped}',
            )
            emit('hud-stats', width=self.width, height=self.height, viewportWidth=size[0], viewportHeight=size[1],
                 decodedFps=round(fps, 2), videoMbps=round(mbps, 2), limitKbps=self.bitrate_limit,
                 rttMs=self.hud_rtt, jitterMs=self.hud_jitter, rtpLost=self.hud_loss,
                 queueDepth=depth, queueDrops=queue_drops, sinkDrops=dropped)
        self.hud_sample = sample
        if not self.transport_pending:
            self.transport_pending = True
            promise = Gst.Promise.new_with_change_func(self.hud_transport_reply, None)
            self.rtc.emit('get-stats', None, promise)
        return GLib.SOURCE_CONTINUE

    @staticmethod
    def parse_transport(report):
        records = [report.get_value(report.nth_field_name(i)) for i in range(report.n_fields())]
        records = [record for record in records if isinstance(record, Gst.Structure)]
        pair = next((report.get_value(record.get_string('selected-candidate-pair-id')) for record in records
                     if record.has_field('selected-candidate-pair-id')), None)
        return records, pair

    def hud_transport_reply(self, promise, *_):
        rtt = jitter = loss = None
        if promise.wait() == Gst.PromiseResult.REPLIED and (report := promise.get_reply()):
            records, pair = self.parse_transport(report)
            if pair and pair.has_field('current-round-trip-time'):
                rtt = 1000 * pair.get_value('current-round-trip-time')
            inbound_video = next((record for record in records if record.get_name() == 'inbound-rtp'
                                  and record.get_string('kind') == 'video'), None)
            if inbound_video:
                if inbound_video.has_field('jitter'):
                    jitter = 1000 * inbound_video.get_value('jitter')
                if inbound_video.has_field('packets-lost'):
                    loss = inbound_video.get_value('packets-lost')
        GLib.idle_add(self.update_hud_transport, rtt, jitter, loss)

    def update_hud_transport(self, rtt, jitter, loss):
        self.hud_rtt, self.hud_jitter, self.hud_loss = rtt, jitter, loss
        self.transport_pending = False
        return GLib.SOURCE_REMOVE

    def transport_stats(self):
        promise = Gst.Promise.new()
        self.rtc.emit('get-stats', None, promise)
        report = checked(promise, 'WebRTC statistics unavailable')
        _, pair = self.parse_transport(report)
        remote = report.get_value(pair.get_string('remote-candidate-id')) if pair and pair.has_field('remote-candidate-id') else None
        if remote and os.getenv('ALLY_TRACE_PEER'):
            try:
                address = ipaddress.ip_address(remote.get_string('address')).packed
                key = hashlib.sha256(address + struct.pack('>H', remote.get_value('port'))).hexdigest()
                with open(os.environ['ALLY_TRACE_PEER'], 'w') as peer:
                    peer.write(key)
            except (ValueError, OSError):
                pass
        rtt = pair.get_value('current-round-trip-time') if pair and pair.has_field('current-round-trip-time') else None
        clock_gaps = [gap for ssrc in self.rtcp_video_ssrcs for gap in self.remote_clock_samples.get(ssrc, [])]
        emit('transport-stats', selectedPair=pair is not None,
             candidateType=remote.get_string('candidate-type') if remote else None,
             rttMs=1000 * rtt if rtt is not None else None, videoRtpPackets=self.capture_packets,
             captureTimestamps=len(self.capture_timestamps), captureClockOffset=self.capture_clock_offset,
             captureClockOffsetMaxMs=self.capture_clock_offset_max_ms,
             videoSenderReports=len(clock_gaps), latestSenderReportGapMs=clock_gaps[-1] if clock_gaps else None)

    def bus_message(self, _, message):
        if message.type == Gst.MessageType.ERROR:
            error, _ = message.parse_error()
            emit('error', message=error.message)
            self.loop.quit()

    def run(self):
        def commands():
            for line in sys.stdin:
                try:
                    GLib.idle_add(self.handle, json.loads(line))
                except ValueError:
                    emit('error', message='Invalid command')
            GLib.idle_add(self.loop.quit)

        threading.Thread(target=commands, daemon=True).start()
        try:
            self.loop.run()
        finally:
            if self.hud_timer is not None:
                GLib.source_remove(self.hud_timer)
            if self.window:
                self.window.vulkan_surface_ready.set()
            self.pipeline.set_state(Gst.State.NULL)
            if self.gamescope_timer is not None:
                GLib.source_remove(self.gamescope_timer)
                self.gamescope_focus.close()
            if self.window:
                self.window.close()
            if self.input_device:
                self.input_device.close()


if __name__ == '__main__':
    try:
        Player().run()
    except (RuntimeError, GLib.Error) as error:
        emit('error', message=str(error))
        sys.exit(1)
