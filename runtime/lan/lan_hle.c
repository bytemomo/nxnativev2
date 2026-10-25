// Local play in the host: nn::ldn and nn::socket take-overs that run on the core (lan_core.c) and the device transport (lan_nw.m).
// The entries go in their own table (__DATA,swforcert), which the loader takes before the host's offline ones (hle_offline.c, hle_net.c). Every name the SDKs use is
// listed, the SDK 4 forms (plain sockaddr / int flags) and the later ones (nn::socket types).
// DM_LAN=0 (the launcher's Local play, read at the game's first call: the launcher sets it after the modules are loaded) turns local play off, and every call
// then answers as the host's offline ones do. Local play's settings (password, port, rooms by address, nearby search) are set on the overlay over the game
// (lan_ui.m) while it hosts or looks for a local game.
// DM_LAN_DEBUG=1 (a debug build: nxnative build <game> --debug) adds the diagnostics of lan_diag.c.
#include "hle.h"
#include "lan_core.h"
#include <stdatomic.h>
#include "lan_diag.h"
#include <pthread.h>
unsigned long long lan_diag_ms(void); unsigned lan_diag_thread(void);   // lan_diag.c
#define LANLOG(fmt, ...) do { if (lan_debug()) LOG("[%6llu ms th %x] " fmt, lan_diag_ms(), lan_diag_thread(), ##__VA_ARGS__); else LOG(fmt, ##__VA_ARGS__); } while (0)

// Every entry runs on the thread's big host stack (hle_bigstack.c, as hle.h's HLE_FORCE_BIG): a game's network thread can have a stack of a few KB, and these calls
// reach Objective-C and the system (the saved password in UserDefaults, Network.framework, the overlay), far deeper than one IPC on a Switch
#define RT_FORCE(mangled, why, ret, cname, ...)                                                   \
    static ret cname(__VA_ARGS__) __attribute__((used));                                          \
    extern const char cname##_bs[];                                                               \
    __asm__(".text\n.p2align 2\n.globl _" #cname "_bs\n.private_extern _" #cname "_bs\n_" #cname "_bs:\n" \
            "    adrp x16, _" #cname "@PAGE\n    add x16, x16, _" #cname "@PAGEOFF\n    b _dm_bigstack_enter\n"); \
    __attribute__((used, section("__DATA,swforcert"))) static const hle_force_ent rt_force_##cname = {mangled, (void *)(uintptr_t)cname##_bs, why}; \
    static ret cname(__VA_ARGS__)
#define RT_FORCE_GUEST_STACK(mangled, why, ret, cname, ...)   /* one that runs the game's own SDK code: on the game's stack */ \
    static ret cname(__VA_ARGS__);                                                                \
    __attribute__((used, section("__DATA,swforcert"))) static const hle_force_ent rt_force_##cname = {mangled, (void *)cname, why}; \
    static ret cname(__VA_ARGS__)

#include "lan_nw.h"   // the transport (lan_nw.m) and its settings, from the overlay (lan_ui.m)

static lan_ctx *_Atomic g_lan;
static void *_Atomic g_ev;   // the game's state change event (AttachStateChangeEvent)
static __thread int t_err;   // GetLastErrno: per thread, as the SDK's
void hle_bind_system_event_fresh(void *ev, int autoclear) __attribute__((weak));   // hle_thread.c (the patch)

// What the game asks and what it gets, in the log: every call that changes the network, and failures of the frequent ones (scans, state, info), so a session
// that ends early says where. Sockets: each one made, bound or connected, and failures other than "would block" (the first 400 lines). Debugging: everything
static _Atomic int g_said_lines, g_sk_lines, g_sk_eagain;
static uint32_t said(const char *what, uint32_t r, int always) {
    if (r) LANLOG("lan: [RED] %s → %#x", what, r);
    else if (always || (lan_debug() && atomic_fetch_add(&g_said_lines, 1) < 400)) LANLOG("lan: %s → ok", what);
    return r;
}
static long sk_said(const char *what, int fd, long r, int always) {
    if (r < 0 && t_err == 11 && !always) { atomic_fetch_add(&g_sk_eagain, 1); return r; }   // EAGAIN in nn::socket's numbering
    if ((always || r < 0 || lan_debug()) && atomic_fetch_add(&g_sk_lines, 1) < 400) {
        if (r < 0) LANLOG("lan: socket %s(%d) → -1, errno %d", what, fd, t_err);
        else LANLOG("lan: socket %s(%d) → %ld", what, fd, r);
    }
    return r;
}

static int lan_on(void) {
    static _Atomic int on = -1; int v = atomic_load(&on);
    if (v < 0) { const char *e = getenv("DM_LAN"); v = !(e && !strcmp(e, "0")); atomic_store(&on, v); if (!v) LANLOG("lan: local play off (DM_LAN=0)"); }
    return v;
}
int dm_rt_force_on(const char *name) { (void)name; return 1; }   // loader.c: the runtime's entries are always taken; off, they answer as the offline ones

static void log_line(int red, const char *msg) { if (red) LOG("[RED] %s", msg); else LOG("%s", msg); }
static void core_log(void *user, int red, const char *msg) { (void)user; log_line(red, msg); }
static void state_changed(void *user) {
    (void)user; void *ev = atomic_load(&g_ev); if (ev) hle_signal_system_event(ev);
    lan_ctx *c = atomic_load(&g_lan); if (c) lan_ui_state(lan_ldn_state(c));   // the overlay shows while the game hosts or looks
}
lan_ctx *lan_current(void) { return atomic_load(&g_lan); }   // lan_nw.h: the overlay's status
static lan_ctx *lan(void) {
    lan_ctx *c = atomic_load(&g_lan); if (c) return c;
    static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
    pthread_mutex_lock(&mu);
    if (!(c = atomic_load(&g_lan))) {
        lan_nw_config nc; lan_ui_config(&nc);
        const lan_transport *t = lan_nw_new(&g_lan, DM_GAME_PROGRAM_ID, &nc, log_line);
        lan_config cfg = {DM_GAME_PROGRAM_ID, state_changed, core_log, NULL};
        c = lan_new(t, &cfg);
        lan_set_room_name(c, nc.room);
        atomic_store(&g_lan, c);
        LANLOG("lan: local play on (%s; port %d; nearby %s, VPN %s; %d friend%s by address%s)", nc.password[0] ? "with a password" : "no password: any device with this game can join",
               nc.port, nc.nearby ? "on" : "off", nc.vpn ? "on" : "off", nc.nfriends, nc.nfriends == 1 ? "" : "s", nc.friends ? "" : ", not looked for");
    }
    pthread_mutex_unlock(&mu);
    return c;
}

// ---------- nn::ldn ----------
#define LDN(m, why, ret, cname, ...) RT_FORCE(m, "local play: " why, ret, cname, __VA_ARGS__)
#define OFF(v) do { if (!lan_on()) return v; } while (0)   // local play off: the offline answer (hle_offline.c)
LDN("_ZN2nn3ldn10InitializeEv", "initialize", Result, r_ldn_Init, void) { OFF(RES_FAIL); lan_diag_start(); return said("Initialize", lan_ldn_initialize(lan()), 1); }
LDN("_ZN2nn3ldn8FinalizeEv", "finalize", void, r_ldn_Fini, void) { OFF(); lan_ldn_finalize(lan()); lan_diag_stop(); LANLOG("lan: Finalize"); }
LDN("_ZN2nn3ldn8GetStateEv", "state", int, r_ldn_State, void) { OFF(0); return lan_ldn_state(lan()); }
RT_FORCE_GUEST_STACK("_ZN2nn3ldn22AttachStateChangeEventEPNS_2os15SystemEventTypeE", "local play: the state change event, signalled on every change of state, network or players", void, r_ldn_Attach, void *ev) {
    if (hle_bind_system_event_fresh) hle_bind_system_event_fresh(ev, 1); else hle_bind_system_event(ev, 1);   // bound off too: the offline host binds it and never signals it
    atomic_store(&g_ev, ev); LANLOG("lan: AttachStateChangeEvent(%p)", ev);
}
LDN("_ZN2nn3ldn15OpenAccessPointEv", "host", Result, r_ldn_OpenAp, void) { OFF(RES_FAIL); return said("OpenAccessPoint", lan_ldn_open_ap(lan()), 1); }
LDN("_ZN2nn3ldn16CloseAccessPointEv", "stop hosting", Result, r_ldn_CloseAp, void) { OFF(RES_FAIL); return said("CloseAccessPoint", lan_ldn_close_ap(lan()), 1); }
LDN("_ZN2nn3ldn13CreateNetworkERKNS0_13NetworkConfigERKNS0_14SecurityConfigERKNS0_10UserConfigE", "create a network", Result, r_ldn_Create,
    const ldn_network_config *n, const ldn_security_config *s, const ldn_user_config *u) { OFF(RES_FAIL); return said("CreateNetwork", lan_ldn_create(lan(), n, s, NULL, u), 1); }
LDN("_ZN2nn3ldn20CreateNetworkPrivateERKNS0_13NetworkConfigERKNS0_14SecurityConfigERKNS0_17SecurityParameterERKNS0_10UserConfigEiPKNS0_12AddressEntryE",
    "create a network joined by its session", Result, r_ldn_CreateP, const ldn_network_config *n, const ldn_security_config *s, const ldn_security_param *p,
    const ldn_user_config *u, int count, const ldn_address_entry *list) { (void)count; (void)list; OFF(RES_FAIL); return said("CreateNetworkPrivate", lan_ldn_create(lan(), n, s, p, u), 1); }
LDN("_ZN2nn3ldn14DestroyNetworkEv", "end the network", Result, r_ldn_Destroy, void) { OFF(RES_FAIL); lan_diag_dump("the game ends its network"); return said("DestroyNetwork", lan_ldn_destroy(lan()), 1); }
LDN("_ZN2nn3ldn16SetAdvertiseDataEPKvm", "the data shown to devices looking", Result, r_ldn_Adv, const void *d, uint64_t n) { OFF(RES_FAIL); return said("SetAdvertiseData", lan_ldn_set_advertise(lan(), d, n), 1); }
LDN("_ZN2nn3ldn22SetStationAcceptPolicyENS0_12AcceptPolicyE", "who may join", Result, r_ldn_Policy, int p) { OFF(RES_FAIL); return said("SetStationAcceptPolicy", lan_ldn_set_accept_policy(lan(), p & 0xFF), 1); }
LDN("_ZN2nn3ldn20AddAcceptFilterEntryENS0_10MacAddressE", "accept list (kept, not applied)", Result, r_ldn_Filter, uint64_t mac) {
    OFF(RES_FAIL); uint8_t m[6]; memcpy(m, &mac, 6); return lan_ldn_add_accept_filter(lan(), m);
}
LDN("_ZN2nn3ldn17ClearAcceptFilterEv", "accept list", Result, r_ldn_ClearFilter, void) { return 0; }
LDN("_ZN2nn3ldn6RejectENS0_11Ipv4AddressE", "send a player away", Result, r_ldn_Reject, uint32_t ip) { OFF(RES_FAIL); return said("Reject", lan_ldn_reject(lan(), ip), 1); }
LDN("_ZN2nn3ldn11OpenStationEv", "join", Result, r_ldn_OpenSta, void) { OFF(RES_FAIL); return said("OpenStation", lan_ldn_open_station(lan()), 0); }
LDN("_ZN2nn3ldn12CloseStationEv", "stop joining", Result, r_ldn_CloseSta, void) { OFF(RES_FAIL); return said("CloseStation", lan_ldn_close_station(lan()), 0); }
LDN("_ZN2nn3ldn4ScanEPNS0_11NetworkInfoEPiiRKNS0_10ScanFilterEi", "look for networks", Result, r_ldn_Scan,
    ldn_network_info *out, int *count, int max, const ldn_scan_filter *f, int channel) { (void)channel; OFF(RES_FAIL); return said("Scan", lan_ldn_scan(lan(), out, count, max, f), 0); }
LDN("_ZN2nn3ldn11ScanPrivateEPNS0_11NetworkInfoEPiiRKNS0_10ScanFilterEi", "look for networks", Result, r_ldn_ScanP,
    ldn_network_info *out, int *count, int max, const ldn_scan_filter *f, int channel) { (void)channel; OFF(RES_FAIL); return said("Scan", lan_ldn_scan(lan(), out, count, max, f), 0); }
LDN("_ZN2nn3ldn7ConnectERKNS0_11NetworkInfoERKNS0_14SecurityConfigERKNS0_10UserConfigEiNS0_13ConnectOptionE", "join a network found", Result, r_ldn_Connect,
    const ldn_network_info *n, const ldn_security_config *s, const ldn_user_config *u, int lcv, int option) { (void)option; OFF(RES_FAIL); return said("Connect", lan_ldn_connect(lan(), n, NULL, s, NULL, u, lcv), 1); }
LDN("_ZN2nn3ldn14ConnectPrivateERKNS0_13NetworkConfigERKNS0_14SecurityConfigERKNS0_17SecurityParameterERKNS0_10UserConfigEiNS0_13ConnectOptionE", "join a network by its session",
    Result, r_ldn_ConnectP, const ldn_network_config *n, const ldn_security_config *s, const ldn_security_param *p, const ldn_user_config *u, int lcv, int option) {
    (void)option; OFF(RES_FAIL); return said("ConnectPrivate", lan_ldn_connect(lan(), NULL, n, s, p, u, lcv), 1);
}
LDN("_ZN2nn3ldn10DisconnectEv", "leave the network", Result, r_ldn_Disc, void) { OFF(RES_FAIL); return said("Disconnect", lan_ldn_disconnect(lan()), 1); }
LDN("_ZN2nn3ldn14GetNetworkInfoEPNS0_11NetworkInfoE", "the network", Result, r_ldn_Info, ldn_network_info *o) { OFF(RES_FAIL); lan_diag_info(o); return said("GetNetworkInfo", lan_ldn_network_info(lan(), o, NULL, 0), 0); }
LDN("_ZN2nn3ldn14GetNetworkInfoEPNS0_11NetworkInfoEPNS0_16NodeLatestUpdateEi", "the network and who came or went", Result, r_ldn_InfoU,
    ldn_network_info *o, ldn_node_update *u, int n) { OFF(RES_FAIL); return said("GetNetworkInfo", lan_ldn_network_info(lan(), o, u, n), 0); }
LDN("_ZN2nn3ldn14GetIpv4AddressEPNS0_11Ipv4AddressEPNS0_10SubnetMaskE", "this device's address", Result, r_ldn_Ip, uint32_t *a, uint32_t *m) { OFF(RES_FAIL); return said("GetIpv4Address", lan_ldn_ipv4(lan(), a, m), 1); }
LDN("_ZN2nn3ldn20GetSecurityParameterEPNS0_17SecurityParameterE", "the network's session", Result, r_ldn_SecPrm, ldn_security_param *o) { OFF(RES_FAIL); return said("GetSecurityParameter", lan_ldn_security_param(lan(), o), 1); }
LDN("_ZN2nn3ldn16GetNetworkConfigEPNS0_13NetworkConfigE", "the network's settings", Result, r_ldn_NetCfg, ldn_network_config *o) { OFF(RES_FAIL); return said("GetNetworkConfig", lan_ldn_network_config(lan(), o), 1); }
LDN("_ZN2nn3ldn19GetDisconnectReasonEv", "why the network ended", int, r_ldn_DiscReason, void) { OFF(0); return lan_ldn_disconnect_reason(lan()); }
LDN("_ZN2nn3ldn32SetWirelessControllerRestrictionENS0_30WirelessControllerRestrictionE", "no controller limit here", Result, r_ldn_CtlRestrict, int r) { (void)r; return 0; }

// The game's error screen (the host has none): what it would show, in the log. With local play on, the error after a failed host or join names the step that failed
RT_FORCE("_ZN2nn3err9ShowErrorERKNS0_18ErrorResultVariantE", "error display: the error's contents in the log (the host shows no applet)", void, r_err_ShowVar, const uint32_t *v) {
    if (!v) { LOG("game asks to show an error (none)"); return; }
    LOG("game asks to show an error (ErrorResultVariant %08x %08x %08x %08x)", v[0], v[1], v[2], v[3]);
    lan_diag_dump("the game shows an error");
}
RT_FORCE("_ZN2nn3err9ShowErrorENS0_9ErrorCodeE", "error display: the code in the log", void, r_err_ShowCode, uint64_t code) {
    LOG("game asks to show an error: %04u-%04u", (unsigned)(code & 0xffffffff), (unsigned)(code >> 32));
    lan_diag_dump("the game shows an error");
}

// ---------- nn::socket ----------
// Off the network every call fails with ENETDOWN, as the host's offline sockets do; Poll / Select with none of the network's descriptors wait out their timeout (hle_net.c)
int dm_sk_poll(void *fds, uint64_t n, int ms);
int dm_sk_select(int nfds, const uint64_t *r, const uint64_t *w, const uint64_t *e, const int64_t *tv);
// nn::socket's own numbering, which the game reads: its errno values are Linux's (EAGAIN 11, ENETDOWN 100; Mario Kart 8 Deluxe's Pia maps any other number to
// a fatal 68616, and so took the core's FreeBSD 35 for "nothing to read" as a broken socket: its receive thread failed, and every room it created ended in 2618-0006),
// and so are its fcntl flags (O_NONBLOCK 0x800). Levels, options, message flags and poll events are BSD's, as the core's
static int nn_errno(int e) {
    switch (e) {
        case SK_EAGAIN: return 11; case SK_EINPROGRESS: return 115; case SK_EALREADY: return 114; case SK_ENOTSOCK: return 88; case SK_EDESTADDRREQ: return 89;
        case SK_EMSGSIZE: return 90; case SK_EPROTOTYPE: return 91; case SK_ENOPROTOOPT: return 92; case SK_EPROTONOSUPPORT: return 93; case SK_EOPNOTSUPP: return 95;
        case SK_EAFNOSUPPORT: return 97; case SK_EADDRINUSE: return 98; case SK_EADDRNOTAVAIL: return 99; case SK_ENETDOWN: return 100; case SK_ENETUNREACH: return 101;
        case SK_ECONNABORTED: return 103; case SK_ECONNRESET: return 104; case SK_ENOBUFS: return 105; case SK_EISCONN: return 106; case SK_ENOTCONN: return 107;
        case SK_ETIMEDOUT: return 110; case SK_ECONNREFUSED: return 111; case SK_EHOSTUNREACH: return 113;
        default: return e;   // EBADF, ENOMEM, EFAULT, EINVAL, EMFILE, EPIPE: the same in both
    }
}
#define NN_O_NONBLOCK 0x800
#define NN_SOCK_NONBLOCK 0x800
#define NN_SOCK_CLOEXEC 0x80000
static int fail(int e) { t_err = nn_errno(e); return -1; }
static long ret(long r) { if (r < 0) { t_err = nn_errno((int)-r); return -1; } return r; }
static int mine(int fd) { return lan_on() && fd >= LAN_FD_BASE && fd < LAN_FD_BASE + LAN_FD_MAX && lan_is_fd(lan(), fd); }
#define FD_OR_FAIL(fd) do { if (!mine(fd)) return fail(lan_on() && lan_active(lan()) ? SK_EBADF : SK_ENETDOWN); } while (0)

// Debugging: the first packets each way, with their addresses (where a game's session traffic goes), and the first polls
static _Atomic int g_pk_out, g_pk_in, g_poll_lines;
static void ip4(char *b, size_t n, const void *a, uint32_t len) {
    const sk_sockaddr_in *x = a;
    if (!a || len < 8) { snprintf(b, n, "(none)"); return; }
    const uint8_t *q = (const uint8_t *)&x->addr_be; snprintf(b, n, "%u.%u.%u.%u:%u", q[0], q[1], q[2], q[3], (unsigned)(x->port_be >> 8 | (x->port_be & 0xFF) << 8));
}
static long traced_send(int s, unsigned long n, int f, const void *a, unsigned al, long r) {
    if (r >= 0 && lan_debug() && atomic_fetch_add(&g_pk_out, 1) < 40) { char t[40]; ip4(t, sizeof t, a, al); LANLOG("lan: packet out fd %d → %s, %lu bytes, flags %#x: %ld", s, a ? t : "(connected)", n, f, r); }
    return r;
}
static long traced_recv(int s, int f, void *a, unsigned *al, long r) {
    if (r >= 0 && lan_debug() && atomic_fetch_add(&g_pk_in, 1) < 40) { char t[40]; ip4(t, sizeof t, a, al ? *al : 0); LANLOG("lan: packet in fd %d ← %s, %ld bytes, flags %#x", s, a ? t : "(no address asked)", r, f); }
    return r;
}
static int sk_socket(int d, int t, int p) {
    if (t & NN_SOCK_NONBLOCK) t = (t & ~NN_SOCK_NONBLOCK) | SK_SOCK_NONBLOCK;   // either numbering
    t &= ~NN_SOCK_CLOEXEC;
    if (!lan_on()) return fail(SK_ENETDOWN); return (int)sk_said("socket", -1, ret(lan_sk_socket(lan(), d, t, p)), 1); }
static int sk_bind(int s, const void *a, unsigned n) { FD_OR_FAIL(s); return (int)sk_said("bind", s, ret(lan_sk_bind(lan(), s, a, n)), 1); }
static int sk_connect(int s, const void *a, unsigned n) { FD_OR_FAIL(s); return (int)sk_said("connect", s, ret(lan_sk_connect(lan(), s, a, n)), 1); }
static int sk_listen(int s, int b) { FD_OR_FAIL(s); return (int)sk_said("listen", s, ret(lan_sk_listen(lan(), s, b)), 1); }
static int sk_accept(int s, void *a, unsigned *n) { FD_OR_FAIL(s); return (int)sk_said("accept", s, ret(lan_sk_accept(lan(), s, a, n)), 0); }
static int sk_close(int s) { FD_OR_FAIL(s); if (lan_debug()) LANLOG("lan: socket %d: %d calls would have blocked so far", s, atomic_load(&g_sk_eagain)); return (int)sk_said("close", s, ret(lan_sk_close(lan(), s)), 1); }
static int sk_shutdown(int s, int h) { FD_OR_FAIL(s); return (int)sk_said("shutdown", s, ret(lan_sk_shutdown(lan(), s, h)), 0); }
static long sk_sendto(int s, const void *b, unsigned long n, int f, const void *a, unsigned al) { FD_OR_FAIL(s); return traced_send(s, n, f, a, al, sk_said("sendto", s, ret(lan_sk_sendto(lan(), s, b, n, f, a, al)), 0)); }
static long sk_recvfrom(int s, void *b, unsigned long n, int f, void *a, unsigned *al) { FD_OR_FAIL(s); return traced_recv(s, f, a, al, sk_said("recvfrom", s, ret(lan_sk_recvfrom(lan(), s, b, n, f, a, al)), 0)); }
static int sk_name(int s, int peer, void *a, unsigned *n) {
    FD_OR_FAIL(s); int r = (int)sk_said(peer ? "getpeername" : "getsockname", s, ret(lan_sk_name(lan(), s, peer, a, n)), 0);
    if (lan_debug() && r == 0) { char t[40]; ip4(t, sizeof t, a, n ? *n : 0); LANLOG("lan: %s(%d) → %s", peer ? "getpeername" : "getsockname", s, t); }
    return r;
}
static int sk_getopt(int s, int l, int o, void *v, unsigned *n) {
    FD_OR_FAIL(s); int r = (int)sk_said("getsockopt", s, ret(lan_sk_getsockopt(lan(), s, l, o, v, n)), 0);
    if (lan_debug()) LANLOG("lan: getsockopt(%d) level %#x option %#x → %d, value %d", s, l, o, r, v && n && *n >= 4 ? *(const int *)v : -1);
    return r;
}
static int sk_setopt(int s, int l, int o, const void *v, unsigned n) { FD_OR_FAIL(s); if (lan_debug()) LANLOG("lan: setsockopt(%d) level %#x option %#x, %u bytes, value %d", s, l, o, n, v && n >= 4 ? *(const int *)v : -1); return (int)sk_said("setsockopt", s, ret(lan_sk_setsockopt(lan(), s, l, o, v, n)), 0); }
static int sk_fcntl(int s, int cmd, int arg) {   // the flags in nn::socket's numbering (O_NONBLOCK 0x800, O_RDWR 2)
    FD_OR_FAIL(s);
    if (lan_debug()) LANLOG("lan: fcntl(%d) command %d argument %#x", s, cmd, arg);
    if (cmd == SK_F_SETFL) arg = (arg & NN_O_NONBLOCK) ? SK_O_NONBLOCK : 0;
    long r = sk_said("fcntl", s, ret(lan_sk_fcntl(lan(), s, cmd, arg)), 0);
    if (cmd == SK_F_GETFL && r >= 0) r = 2 | ((r & SK_O_NONBLOCK) ? NN_O_NONBLOCK : 0);
    return (int)r;
}
static int sk_poll(void *fds, uint64_t n, int ms) {
    sk_pollfd *p = fds; int any = 0;
    for (uint64_t i = 0; i < n && p; i++) if (mine(p[i].fd)) { any = 1; break; }
    if (!any) { int r = dm_sk_poll(fds, n, ms); if (r < 0) t_err = nn_errno(SK_ENETDOWN); return r; }
    int r = (int)ret(lan_sk_poll(lan(), p, n, ms));
    if (lan_debug() && atomic_fetch_add(&g_poll_lines, 1) < 30) LANLOG("lan: poll %llu fds (first %d events %#x), %d ms → %d (revents %#x)", (unsigned long long)n, n ? p[0].fd : -1, n ? p[0].events : 0, ms, r, n ? p[0].revents : 0);
    return r;
}
static int sk_select(int nfds, void *r, void *w, void *e, const int64_t *tv) {
    int any = 0; uint64_t *sets[3] = {r, w, e};
    for (int fd = LAN_FD_BASE; fd < nfds && fd < 1024 && !any; fd++) for (int k = 0; k < 3; k++) if (sets[k] && (sets[k][fd >> 6] >> (fd & 63) & 1) && mine(fd)) { any = 1; break; }
    if (!any) { int x = dm_sk_select(nfds, r, w, e, tv); if (x < 0) t_err = nn_errno(SK_ENETDOWN); return x; }
    int ms = tv ? (int)(tv[0] * 1000 + tv[1] / 1000) : -1;
    int x = (int)ret(lan_sk_select(lan(), nfds, r, w, e, ms));
    if (lan_debug() && atomic_fetch_add(&g_poll_lines, 1) < 30) LANLOG("lan: select %d fds, %d ms → %d", nfds, ms, x);
    return x;
}
#define SK(m, ret_t, cname, ...) RT_FORCE(m, "local play: sockets on the local network", ret_t, cname, __VA_ARGS__)
// SDK 4 forms (plain sockaddr, int flags)
SK("_ZN2nn6socket6SocketEiii", int, r_sk_Socket4, int d, int t, int p) { return sk_socket(d, t, p); }
SK("_ZN2nn6socket4BindEiPK8sockaddrj", int, r_sk_Bind4, int s, const void *a, unsigned n) { return sk_bind(s, a, n); }
SK("_ZN2nn6socket7ConnectEiPK8sockaddrj", int, r_sk_Connect4, int s, const void *a, unsigned n) { return sk_connect(s, a, n); }
SK("_ZN2nn6socket6AcceptEiP8sockaddrPj", int, r_sk_Accept4, int s, void *a, unsigned *n) { return sk_accept(s, a, n); }
SK("_ZN2nn6socket4RecvEiPvmi", long, r_sk_Recv4, int s, void *b, unsigned long n, int f) { return sk_recvfrom(s, b, n, f, NULL, NULL); }
SK("_ZN2nn6socket8RecvFromEiPvmiP8sockaddrPj", long, r_sk_RecvFrom4, int s, void *b, unsigned long n, int f, void *a, unsigned *al) { return sk_recvfrom(s, b, n, f, a, al); }
SK("_ZN2nn6socket4SendEiPKvmi", long, r_sk_Send4, int s, const void *b, unsigned long n, int f) { return sk_sendto(s, b, n, f, NULL, 0); }
SK("_ZN2nn6socket6SendToEiPKvmiPK8sockaddrj", long, r_sk_SendTo4, int s, const void *b, unsigned long n, int f, const void *a, unsigned al) { return sk_sendto(s, b, n, f, a, al); }
SK("_ZN2nn6socket11GetSockNameEiP8sockaddrPj", int, r_sk_SockName4, int s, void *a, unsigned *n) { return sk_name(s, 0, a, n); }
SK("_ZN2nn6socket11GetPeerNameEiP8sockaddrPj", int, r_sk_PeerName4, int s, void *a, unsigned *n) { return sk_name(s, 1, a, n); }
SK("_ZN2nn6socket10GetSockOptEiiiPvPj", int, r_sk_GetOpt4, int s, int l, int o, void *v, unsigned *n) { return sk_getopt(s, l, o, v, n); }
SK("_ZN2nn6socket10SetSockOptEiiiPKvj", int, r_sk_SetOpt4, int s, int l, int o, const void *v, unsigned n) { return sk_setopt(s, l, o, v, n); }
SK("_ZN2nn6socket5FcntlEiiz", int, r_sk_Fcntl4, int s, int cmd, int arg) { return sk_fcntl(s, cmd, arg); }   // variadic on the Switch: the argument is in a register
SK("_ZN2nn6socket4PollEP6pollfdmi", int, r_sk_Poll4, void *f, uint64_t n, int t) { return sk_poll(f, n, t); }
SK("_ZN2nn6socket6SelectEiP6fd_setS2_S2_P7timeval", int, r_sk_Select4, int n, void *r, void *w, void *e, const int64_t *t) { return sk_select(n, r, w, e, t); }
SK("_ZN2nn6socket8ShutdownEii", int, r_sk_Shutdown, int s, int h) { return sk_shutdown(s, h); }
SK("_ZN2nn6socket6ListenEii", int, r_sk_Listen, int s, int b) { return sk_listen(s, b); }
SK("_ZN2nn6socket5CloseEi", int, r_sk_Close, int s) { return sk_close(s); }
SK("_ZN2nn6socket12GetLastErrnoEv", int, r_sk_Errno, void) { return t_err; }
SK("_ZN2nn6socket12SetLastErrnoEi", void, r_sk_SetErrno, int e) { t_err = e; }
SK("_ZN2nn6socket12GetLastErrorEv", int, r_sk_LastError, void) { return t_err; }
// Later SDKs' forms (nn::socket types)
SK("_ZN2nn6socket6SocketENS0_6FamilyENS0_4TypeENS0_8ProtocolE", int, r_sk_Socket, int d, int t, int p) { return sk_socket(d, t, p); }
SK("_ZN2nn6socket4BindEiPKNS0_8SockAddrEj", int, r_sk_Bind, int s, const void *a, unsigned n) { return sk_bind(s, a, n); }
SK("_ZN2nn6socket7ConnectEiPKNS0_8SockAddrEj", int, r_sk_Connect, int s, const void *a, unsigned n) { return sk_connect(s, a, n); }
SK("_ZN2nn6socket6AcceptEiPNS0_8SockAddrEPj", int, r_sk_Accept, int s, void *a, unsigned *n) { return sk_accept(s, a, n); }
SK("_ZN2nn6socket4RecvEiPvmNS0_7MsgFlagE", long, r_sk_Recv, int s, void *b, unsigned long n, int f) { return sk_recvfrom(s, b, n, f, NULL, NULL); }
SK("_ZN2nn6socket8RecvFromEiPvmNS0_7MsgFlagEPNS0_8SockAddrEPj", long, r_sk_RecvFrom, int s, void *b, unsigned long n, int f, void *a, unsigned *al) { return sk_recvfrom(s, b, n, f, a, al); }
SK("_ZN2nn6socket4SendEiPKvmNS0_7MsgFlagE", long, r_sk_Send, int s, const void *b, unsigned long n, int f) { return sk_sendto(s, b, n, f, NULL, 0); }
SK("_ZN2nn6socket6SendToEiPKvmNS0_7MsgFlagEPKNS0_8SockAddrEj", long, r_sk_SendTo, int s, const void *b, unsigned long n, int f, const void *a, unsigned al) { return sk_sendto(s, b, n, f, a, al); }
SK("_ZN2nn6socket11GetSockNameEiPNS0_8SockAddrEPj", int, r_sk_SockName, int s, void *a, unsigned *n) { return sk_name(s, 0, a, n); }
SK("_ZN2nn6socket11GetPeerNameEiPNS0_8SockAddrEPj", int, r_sk_PeerName, int s, void *a, unsigned *n) { return sk_name(s, 1, a, n); }
SK("_ZN2nn6socket10GetSockOptEiNS0_5LevelENS0_6OptionEPvPj", int, r_sk_GetOpt, int s, int l, int o, void *v, unsigned *n) { return sk_getopt(s, l, o, v, n); }
SK("_ZN2nn6socket10SetSockOptEiNS0_5LevelENS0_6OptionEPKvj", int, r_sk_SetOpt, int s, int l, int o, const void *v, unsigned n) { return sk_setopt(s, l, o, v, n); }
SK("_ZN2nn6socket5FcntlEiNS0_12FcntlCommandEz", int, r_sk_Fcntl, int s, int cmd, int arg) { return sk_fcntl(s, cmd, arg); }
SK("_ZN2nn6socket4PollEPNS0_6PollFdEmi", int, r_sk_Poll, void *f, uint64_t n, int t) { return sk_poll(f, n, t); }
SK("_ZN2nn6socket6SelectEiPNS0_5FdSetES2_S2_PNS0_7TimeValE", int, r_sk_Select, int n, void *r, void *w, void *e, const int64_t *t) { return sk_select(n, r, w, e, t); }
SK("_ZN2nn6socket8ShutdownEiNS0_14ShutdownMethodE", int, r_sk_ShutdownN, int s, int h) { return sk_shutdown(s, h); }
// The C interface
SK("nnsocketSocket", int, r_nsk_Socket, int d, int t, int p) { return sk_socket(d, t, p); }
SK("nnsocketBind", int, r_nsk_Bind, int s, const void *a, unsigned n) { return sk_bind(s, a, n); }
SK("nnsocketConnect", int, r_nsk_Connect, int s, const void *a, unsigned n) { return sk_connect(s, a, n); }
SK("nnsocketListen", int, r_nsk_Listen, int s, int b) { return sk_listen(s, b); }
SK("nnsocketAccept", int, r_nsk_Accept, int s, void *a, unsigned *n) { return sk_accept(s, a, n); }
SK("nnsocketClose", int, r_nsk_Close, int s) { return sk_close(s); }
SK("nnsocketShutdown", int, r_nsk_Shutdown, int s, int h) { return sk_shutdown(s, h); }
SK("nnsocketRecv", long, r_nsk_Recv, int s, void *b, unsigned long n, int f) { return sk_recvfrom(s, b, n, f, NULL, NULL); }
SK("nnsocketRecvFrom", long, r_nsk_RecvFrom, int s, void *b, unsigned long n, int f, void *a, unsigned *al) { return sk_recvfrom(s, b, n, f, a, al); }
SK("nnsocketSend", long, r_nsk_Send, int s, const void *b, unsigned long n, int f) { return sk_sendto(s, b, n, f, NULL, 0); }
SK("nnsocketSendTo", long, r_nsk_SendTo, int s, const void *b, unsigned long n, int f, const void *a, unsigned al) { return sk_sendto(s, b, n, f, a, al); }
SK("nnsocketGetSockName", int, r_nsk_SockName, int s, void *a, unsigned *n) { return sk_name(s, 0, a, n); }
SK("nnsocketGetPeerName", int, r_nsk_PeerName, int s, void *a, unsigned *n) { return sk_name(s, 1, a, n); }
SK("nnsocketGetSockOpt", int, r_nsk_GetOpt, int s, int l, int o, void *v, unsigned *n) { return sk_getopt(s, l, o, v, n); }
SK("nnsocketSetSockOpt", int, r_nsk_SetOpt, int s, int l, int o, const void *v, unsigned n) { return sk_setopt(s, l, o, v, n); }
SK("nnsocketFcntl", int, r_nsk_Fcntl, int s, int cmd, int arg) { return sk_fcntl(s, cmd, arg); }
SK("nnsocketPoll", int, r_nsk_Poll, void *f, uint64_t n, int t) { return sk_poll(f, n, t); }
SK("nnsocketSelect", int, r_nsk_Select, int n, void *r, void *w, void *e, const int64_t *t) { return sk_select(n, r, w, e, t); }
SK("nnsocketGetLastErrno", int, r_nsk_Errno, void) { return t_err; }
SK("nnsocketSetLastErrno", void, r_nsk_SetErrno, int e) { t_err = e; }
