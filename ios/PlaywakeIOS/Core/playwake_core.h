//
//  playwake_core.h
//  C ABI of the portable Rust protocol core (ios-core crate).
//
//  Every string is UTF-8 and NUL terminated. String pointers returned by
//  `pw_*` functions are owned by the caller and must be released with
//  `pw_free_string`. Event callbacks receive a pointer that is only valid
//  for the duration of the call.
//

#ifndef PLAYWAKE_CORE_H
#define PLAYWAKE_CORE_H

#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef void (*pw_emit)(const char *json);

const char *pw_core_version(void);

/// Points the shared session/cache code at the app's Documents directory.
int32_t pw_init(const char *session_path);

void pw_free_string(char *pointer);

/// Restores a saved refresh token and emits `account` or `signedOut`.
void pw_restore(pw_emit callback);

/// Starts the Microsoft device-code flow. Emits `device`, then `account`
/// and `done`, or `error` and `done`.
void pw_sign_in(pw_emit callback);
void pw_cancel_sign_in(void);
void pw_sign_out(pw_emit callback);

void pw_consoles(pw_emit callback);
/// Emits `cloud` with the games in letter order, cached first when there is
/// a cached list.
void pw_cloud_catalog(pw_emit callback);
/// The positions in `names_json` (a JSON array of strings) that fuzzily
/// match `query`, best first, as a JSON array; all of them for "".
char *pw_library_search(const char *names_json, const char *query);

/// Reserves and provisions a stream. Emits `status`, then `ready` (with the
/// STUN list and heartbeat interval), then `done`.
void pw_stream_home(pw_emit callback, size_t console_index, uint32_t height);
void pw_stream_cloud(pw_emit callback, const char *game_id, uint32_t height);
void pw_stop(void);

char *pw_sdp(const char *offer);
char *pw_ice(const char *candidates_json);
void pw_keepalive(void);

char *pw_munge_offer(const char *sdp, uint32_t bitrate, int capture);
char *pw_handshake(uint32_t width, uint32_t height);
int32_t pw_is_handshake_ack(const char *json);

/// Writes the packet into `out` and returns its length. If `out` is too
/// small, nothing is written and the required capacity is returned.
size_t pw_input_packet(uint32_t sequence, double time, uint16_t buttons,
                       int16_t lx, int16_t ly, int16_t rx, int16_t ry,
                       uint16_t lt, uint16_t rt,
                       uint8_t *out, size_t capacity);
size_t pw_connect_packet(double start_time, uint8_t *out, size_t capacity);

/* Stream-input route shared with the SteamOS worker: View+Menu tap opens the
 * stream menu, menu keys are reported instead of sent, Guide pulse on close. */
typedef struct pw_route pw_route;
typedef struct {
    int16_t axes[4];
    uint16_t buttons;
    uint16_t lt;
    uint16_t rt;
} pw_pad;
enum {
    PW_ROUTE_MENU_OPENED = 1 << 0,
    PW_ROUTE_MENU_CLOSED = 1 << 1,
    PW_ROUTE_KEY_PREVIOUS = 1 << 2,
    PW_ROUTE_KEY_NEXT = 1 << 3,
    PW_ROUTE_KEY_ACTIVATE = 1 << 4,
    PW_ROUTE_KEY_BACK = 1 << 5,
};
pw_route *pw_route_new(void);
void pw_route_free(pw_route *route);
/* Rewrites `pad` with what may reach the console; returns PW_ROUTE_* bits. */
uint32_t pw_route_input(pw_route *route, pw_pad *pad, double now_ms);
uint32_t pw_route_open(pw_route *route);
uint32_t pw_route_close(pw_route *route, bool guide);

/* PCs with Sunshine (GameStream), from the same core as on SteamOS. */

/// Emits `pcs` with the PCs (`state`: paired, unpaired or offline) and the
/// apps of the paired ones, then `pcs` again as their covers arrive. PC
/// failures come as `pcError`, which leaves a running stream alone.
void pw_pc_catalog(pw_emit callback);
/// Remembers the PC at `address` (one the network search misses) once
/// Sunshine answers there, then emits `pcs`; `pcError` otherwise.
void pw_pc_add(pw_emit callback, const char *address);
/// Pairs with the unpaired PC at `address`: emits `pcPin` with the PIN to
/// enter in Sunshine, then `pcPaired` or `error`.
void pw_pc_pair(pw_emit callback, const char *address);
/// One encoded frame: `data` holds 4-byte length-prefixed NAL units; a key
/// frame also brings its `count` parameter sets. Valid for the call only.
typedef void (*pw_pc_video)(const uint8_t *data, size_t length, const uint8_t *const *sets,
                            const size_t *set_lengths, size_t count, bool hevc, bool key);
/// One Opus packet, 48 kHz stereo, 5 ms. Valid for the call only.
typedef void (*pw_pc_audio)(const uint8_t *data, size_t length);
/// Streams the app with this `id` (from `pcs`). Returns the stream's
/// session, or 0 when it cannot start (`pcError` says why). Emits `status`,
/// `pcStreaming`, `pcHdr` and `pcEnded`, each with its `session`; `video` and
/// `audio` are called on the stream's own thread.
uint64_t pw_pc_stream(pw_emit callback, const char *game_id, uint32_t width, uint32_t height, uint32_t fps,
                  uint32_t bitrate_kbps, bool hdr, pw_pc_video video, pw_pc_audio audio);
/// Sends a routed pad state (after `pw_route_input`) to the PC stream.
void pw_pc_gamepad(const pw_pad *pad);
/// Ends the PC stream, also one still launching, and waits up to 3 s for its
/// thread: after it returns no frame or sound of it arrives. `pcEnded` follows.
void pw_pc_stop(void);

/* Keyboard, mouse and touch for the current stream: a PC gets them as
 * GameStream input; without one they become a gamepad that the Xbox stream
 * mixes in with `pw_input_pad`. While a route's menu is open (from the
 * moment the route reports MENU_OPENED) input calls change nothing.
 * Safe from any thread. */
enum {
    PW_INPUT_GAME = 0,      /* mouse: relative motion, pointer captured */
    PW_INPUT_DESKTOP = 1,   /* mouse: absolute position over the video */
    PW_INPUT_TRACKPAD = 2,  /* touch: trackpad gestures */
    PW_INPUT_DIRECT = 3,    /* touch: the finger touches the video */
};
enum {
    PW_INPUT_BUTTON_LEFT = 0,
    PW_INPUT_BUTTON_RIGHT = 1,
    PW_INPUT_BUTTON_MIDDLE = 2,
    PW_INPUT_BUTTON_X1 = 3,
    PW_INPUT_BUTTON_X2 = 4,
};
enum {
    PW_INPUT_PHASE_BEGAN = 0,
    PW_INPUT_PHASE_MOVED = 1,
    PW_INPUT_PHASE_ENDED = 2,
    PW_INPUT_PHASE_CANCELLED = 3,
};
enum {
    PW_INPUT_SHOW_KEYBOARD = 1 << 0,
};
/* Where a key or button comes from: the host hears it let go only once no
 * source holds it. */
enum {
    PW_INPUT_SOURCE_KEYBOARD = 0,
    PW_INPUT_SOURCE_MOUSE = 1,
    PW_INPUT_SOURCE_PANEL = 2,  /* the on-screen key bar and keyboard */
};
enum {
    PW_INPUT_CHAR_SHIFT = 1 << 16,
};
/// Sets the mouse mode (GAME, DESKTOP) or the touch mode (TRACKPAD, DIRECT);
/// a change lets go of everything held.
void pw_input_mode(uint32_t mode);
/// A key by HID usage from a PW_INPUT_SOURCE_*; only the keyboard page 0x07 is taken.
void pw_input_key(uint32_t source, uint16_t usage_page, uint16_t usage, bool down);
/// The HID usage that types the Unicode scalar `c` on a US keyboard, with
/// PW_INPUT_CHAR_SHIFT if it takes Shift; 0 for none.
uint32_t pw_input_char_usage(uint32_t c);
/// Types UTF-8 text on a PC.
void pw_input_text(const char *text);
/// Relative motion in host pixels, `dy` positive down; a PC in DESKTOP mode ignores it.
void pw_input_mouse_move(float dx, float dy);
/// The pointer over the video, 0…1 from its top left; only a PC in DESKTOP mode takes it.
void pw_input_mouse_position(float x, float y);
/// A PW_INPUT_BUTTON_* from a PW_INPUT_SOURCE_*.
void pw_input_mouse_button(uint32_t source, uint32_t button, bool down);
/// Wheel notches, fractions allowed: `dy` positive scrolls up, `dx` right.
void pw_input_scroll(float dx, float dy);
/// The touch surface in points, for trackpad distances.
void pw_input_surface(float width, float height);
/// One touch (`phase` a PW_INPUT_PHASE_*, `now_ms` monotonic). TRACKPAD: `x`, `y`
/// 0…1 over the surface, the core reads gestures. DIRECT: 0…1 over the video; a
/// PC without touch gets the left button there. ENDED carries the last point.
/// Returns PW_INPUT_SHOW_KEYBOARD when three fingers ask for the keyboard.
uint32_t pw_input_touch(uint32_t id, uint32_t phase, float x, float y, float pressure, double now_ms);
/// Lets go of every key and button, from every source: on focus loss.
void pw_input_release_all(void);
/// Whether the PC stream takes touches; false without one.
bool pw_input_host_supports_touch(void);
/// Mixes the keyboard and mouse, as a gamepad, into `pad`; call it each Xbox
/// input tick before `pw_route_input`.
void pw_input_pad(pw_pad *pad, double now_ms);

#ifdef __cplusplus
}
#endif

#endif /* PLAYWAKE_CORE_H */
