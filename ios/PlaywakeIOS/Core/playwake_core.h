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
void pw_cloud_catalog(pw_emit callback);

/// Reserves and provisions a stream. Emits `status`, then `ready` (with the
/// STUN list and heartbeat interval), then `done`.
void pw_stream_home(pw_emit callback, size_t console_index, uint32_t height);
void pw_stream_cloud(pw_emit callback, size_t game_index, uint32_t height);
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

#ifdef __cplusplus
}
#endif

#endif /* PLAYWAKE_CORE_H */
