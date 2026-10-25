// Local play: the core (lan_core.h describes it). One lock per context; guest threads wait on its condition variable, the transport's thread delivers.
#include "lan_core.h"
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

_Static_assert(sizeof(ldn_intent) == 0x10, "IntentId");
_Static_assert(sizeof(ldn_network_id) == 0x20, "NetworkId");
_Static_assert(sizeof(ldn_common) == 0x30, "CommonNetworkInfo");
_Static_assert(sizeof(ldn_node) == 0x40, "NodeInfo");
_Static_assert(sizeof(ldn_network_info) == 0x480, "NetworkInfo");
_Static_assert(sizeof(ldn_user_config) == 0x30, "UserConfig");
_Static_assert(sizeof(ldn_network_config) == 0x20, "NetworkConfig");
_Static_assert(sizeof(ldn_security_config) == 0x44, "SecurityConfig");
_Static_assert(sizeof(ldn_security_param) == 0x20, "SecurityParameter");
_Static_assert(sizeof(ldn_scan_filter) == 0x60, "ScanFilter");
_Static_assert(sizeof(ldn_address_entry) == 0xC, "AddressEntry");
_Static_assert(sizeof(sk_sockaddr_in) == 16, "sockaddr_in");
_Static_assert(sizeof(sk_pollfd) == 8, "pollfd");

// ---------- wire format: frames between the hub and its stations, little endian ----------
#define MAGIC 0x314C584Eu   // "NXL1"
#define VERSION 1
enum {
    F_PROBE = 1,      // station → hub: u32 magic, u16 version. The hub answers with F_NETINFO now and on every change
    F_NETINFO = 2,    // hub → station: NetworkInfo
    F_JOIN = 3,       // station → hub: u32 magic, u16 version, UserConfig, i16 lcv, u8 private, u16 security mode, u16 passphrase size, passphrase[0x40], session[16]
    F_JOINED = 4,     // hub → station: u8 node, u32 address, NetworkInfo
    F_REFUSED = 5,    // hub → station: u32 nn::ldn result
    F_LEAVE = 6,      // either way: u8 DisconnectReason
    F_PING = 7,       // either way: u64 the sender's clock; answered with F_PONG and the same bytes (round trips, for the overlay)
    F_PONG = 8,
    F_ROOM = 9,       // hub → station: the room's name (UTF-8, at most LAN_ROOM_MAX - 1 bytes), after F_NETINFO and when it changes
    F_UDP = 10,       // routed: u32 src, u16 src port, u32 dst, u16 dst port, then the payload
    F_SYN = 11,       // routed TCP: open (the tuple only)
    F_SYNACK = 12,    // routed TCP: the answer, u8 accepted
    F_DATA = 13,      // routed TCP: bytes
    F_FIN = 14,       // routed TCP: end of the sender's data, u8 reset
};
#define TUPLE 13      // type + src, src port, dst, dst port
#define JOIN_LEN (4 + 2 + 0x30 + 2 + 1 + 2 + 2 + 0x40 + 16)
#define TCP_CHUNK 16384
#define RX_MAX (4u << 20)      // a TCP connection's unread bytes; more resets it (no flow control between devices)
#define DGRAM_MAX_Q 512        // queued datagrams per UDP socket; more drops the oldest, as a full socket buffer does
#define DGRAM_MAX_BYTES (4u << 20)
#define MAX_PEERS 32
#define NET_MASK 0xFFFFFF00u

static void wr16(uint8_t *p, uint16_t v) { p[0] = (uint8_t)v; p[1] = (uint8_t)(v >> 8); }
static void wr32(uint8_t *p, uint32_t v) { wr16(p, (uint16_t)v); wr16(p + 2, (uint16_t)(v >> 16)); }
static uint16_t rd16(const uint8_t *p) { return (uint16_t)(p[0] | p[1] << 8); }
static uint32_t rd32(const uint8_t *p) { return rd16(p) | (uint32_t)rd16(p + 2) << 16; }

typedef struct { uint32_t src, dst; uint16_t sport, dport; uint32_t len; uint8_t *data; } dgram;
enum { ST_NEW, ST_LISTEN, ST_CONNECTING, ST_CONNECTED, ST_CLOSED };
typedef struct {
    int used, type, nonblock, broadcast, reuse;
    uint32_t bind_ip; uint16_t port;          // port 0: not bound
    int64_t rcvtimeo_ms, sndtimeo_ms;         // -1: none
    int err;                                  // pending SO_ERROR
    dgram *q; int qhead, qn; size_t qbytes;   // UDP
    int st; uint32_t peer_ip; uint16_t peer_port;   // TCP (UDP: the connected peer)
    int backlog, acq[16], acn, accepted;      // listening: connections not accepted yet (indexes); accepted: taken by accept
    uint8_t *rx; size_t rxn, rxcap; int eof, reset, wr_shut;
} sk;

enum { ROLE_IN = 1, ROLE_MEMBER, ROLE_HUB };   // hub side: connected to us, then a member; station side: a hub we dialled
typedef struct { int used, ready, role, node; uint64_t h, service; ldn_network_info info; int have_info; char room[LAN_ROOM_MAX]; int rtt_ms; } peer;

struct lan_ctx {
    lan_transport t; lan_config cfg;
    pthread_mutex_t mu; pthread_cond_t cv;
    int notify;                               // the game's event is signalled once the lock is released
    int state, reason, browsing; uint64_t browse_since;
    ldn_network_info net; uint32_t my_ip; int my_node;     // the network (created or joined)
    ldn_node_update upd[8];
    uint16_t sec_mode, pass_size; uint8_t pass[0x40];      // hub: what stations must present
    int accept_policy, nfilter; uint8_t filter[8][6];
    uint8_t adv[0x180]; uint16_t adv_size;                 // set before the network exists
    uint64_t hub;                                          // station: the hub joined
    uint64_t join_peer; int join_done; uint32_t join_res;
    peer peers[MAX_PEERS];
    sk sks[LAN_FD_MAX];
    uint16_t next_port;
    char room[LAN_ROOM_MAX];                               // hub: the room's name
    uint64_t pinged;                                       // the last pings (lan_status_get)
    uint8_t scratch[LAN_FRAME_MAX];
};

// ---------- helpers ----------
static uint64_t now_ms(void) { struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts); return (uint64_t)ts.tv_sec * 1000 + (uint64_t)ts.tv_nsec / 1000000; }
static void rnd(void *p, size_t n) {
#if defined(__APPLE__)
    arc4random_buf(p, n);
#else
    uint8_t *b = p;
    while (n) { size_t k = n > 256 ? 256 : n; if (getentropy(b, k)) { for (size_t i = 0; i < k; i++) b[i] = (uint8_t)rand(); } b += k; n -= k; }
#endif
}
static void logf_(lan_ctx *c, int red, const char *fmt, ...) {
    if (!c->cfg.log) return;
    char b[512]; va_list ap; va_start(ap, fmt); vsnprintf(b, sizeof b, fmt, ap); va_end(ap);
    c->cfg.log(c->cfg.user, red, b);
}
#define LOGI(...) logf_(c, 0, __VA_ARGS__)
#define LOGR(...) logf_(c, 1, __VA_ARGS__)
static void lock(lan_ctx *c) { pthread_mutex_lock(&c->mu); }
static void unlock(lan_ctx *c) {
    int n = c->notify; c->notify = 0;
    pthread_cond_broadcast(&c->cv);
    pthread_mutex_unlock(&c->mu);
    if (n && c->cfg.state_changed) c->cfg.state_changed(c->cfg.user);
}
static void changed(lan_ctx *c) { c->notify = 1; }
static void set_state(lan_ctx *c, int s) { if (c->state != s) { LOGI("lan: state %d -> %d", c->state, s); c->state = s; } changed(c); }
// Waits up to ms (or 100 ms at most, so callers look again): returns 0 when the deadline (absolute, now_ms) has passed
static int wait_until(lan_ctx *c, uint64_t deadline) {
    uint64_t now = now_ms(); if (deadline && now >= deadline) return 0;
    uint64_t ms = deadline ? deadline - now : 100; if (ms > 100) ms = 100;
    struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts);
    ts.tv_nsec += (long)(ms % 1000) * 1000000; ts.tv_sec += (time_t)(ms / 1000) + ts.tv_nsec / 1000000000; ts.tv_nsec %= 1000000000;
    pthread_cond_timedwait(&c->cv, &c->mu, &ts);
    return 1;
}
static uint64_t deadline_of(int64_t ms) { return ms < 0 ? 0 : now_ms() + (uint64_t)ms + (ms == 0); }
static uint32_t bcast(lan_ctx *c) { return (c->my_ip & NET_MASK) | 0xFF; }
static int on_net(lan_ctx *c, uint32_t ip) { return ip == 0xFFFFFFFFu || (ip & NET_MASK) == (c->my_ip & NET_MASK); }
static int is_bcast(lan_ctx *c, uint32_t ip) { return ip == 0xFFFFFFFFu || ip == bcast(c); }
int lan_active(lan_ctx *c) { return c->state == LDN_AP_CREATED || c->state == LDN_STATION_CONNECTED; }
static void cstr(char *s, size_t n) { s[n - 1] = 0; }

static peer *peer_of(lan_ctx *c, uint64_t h) { for (int i = 0; i < MAX_PEERS; i++) if (c->peers[i].used && c->peers[i].h == h) return &c->peers[i]; return NULL; }
static peer *peer_new(lan_ctx *c, uint64_t h) { for (int i = 0; i < MAX_PEERS; i++) if (!c->peers[i].used) { memset(&c->peers[i], 0, sizeof(peer)); c->peers[i].used = 1; c->peers[i].h = h; c->peers[i].node = -1; c->peers[i].rtt_ms = -1; return &c->peers[i]; } return NULL; }
static void send_to(lan_ctx *c, uint64_t h, const uint8_t *f, uint32_t n) { if (h && c->t.send(c->t.self, h, f, n)) LOGR("lan: a frame of %u bytes could not be queued", n); }
static void peer_drop(lan_ctx *c, peer *p) { uint64_t h = p->h; p->used = 0; c->t.close(c->t.self, h); }

static void browse(lan_ctx *c, int on) {
    if (on == c->browsing) return;
    c->browsing = on; if (on) c->browse_since = now_ms();
    c->t.browse(c->t.self, on);
    if (!on) for (int i = 0; i < MAX_PEERS; i++) if (c->peers[i].used && c->peers[i].role == ROLE_HUB && c->peers[i].h != c->hub) peer_drop(c, &c->peers[i]);   // the probes go with it
}

// ---------- sockets: state the network changes ----------
static void sk_free_data(sk *s) {
    for (int i = 0; i < s->qn; i++) free(s->q[(s->qhead + i) % DGRAM_MAX_Q].data);
    free(s->q); free(s->rx);
}
static void reset_tcp(lan_ctx *c, uint32_t only_ip) {   // a peer or the whole network went away: its connections end (0 = all)
    for (int i = 0; i < LAN_FD_MAX; i++) {
        sk *s = &c->sks[i];
        if (!s->used || s->type != SK_SOCK_STREAM || (s->st != ST_CONNECTED && s->st != ST_CONNECTING) || (only_ip && s->peer_ip != only_ip)) continue;
        if (s->st == ST_CONNECTING) s->err = SK_ENETUNREACH;
        s->reset = 1; s->st = ST_CLOSED;
    }
}

// ---------- the network ----------
static void push_netinfo(lan_ctx *c) {   // hub: to every station and every device looking
    uint8_t f[1 + sizeof(ldn_network_info)]; f[0] = F_NETINFO; memcpy(f + 1, &c->net, sizeof c->net);
    for (int i = 0; i < MAX_PEERS; i++) if (c->peers[i].used && c->peers[i].ready && (c->peers[i].role == ROLE_IN || c->peers[i].role == ROLE_MEMBER)) send_to(c, c->peers[i].h, f, sizeof f);
}
static void send_room(lan_ctx *c, uint64_t h) {   // hub: the room's name, to one device
    size_t n = strlen(c->room); uint8_t f[1 + LAN_ROOM_MAX]; f[0] = F_ROOM; memcpy(f + 1, c->room, n); send_to(c, h, f, (uint32_t)(1 + n));
}
static void node_changes(lan_ctx *c, const ldn_network_info *old, const ldn_network_info *now) {   // NodeLatestUpdate: 1 connected, 2 disconnected
    for (int i = 0; i < 8; i++) {
        int a = old->nodes[i].connected, b = now->nodes[i].connected;
        if (a != b || (a && old->nodes[i].ip != now->nodes[i].ip)) c->upd[i].state_change = b ? 1 : 2;
    }
}
static void node_remove(lan_ctx *c, peer *p, int reason) {   // hub: a station left or was sent away
    if (p->role != ROLE_MEMBER || p->node < 1 || p->node > 7) return;
    ldn_network_info old = c->net; ldn_node *n = &c->net.nodes[p->node];
    uint32_t ip = n->ip;
    LOGI("lan: player %d (%s) left, reason %d", p->node, n->user_name, reason);
    memset(n, 0, sizeof *n); n->node_id = (int8_t)p->node;
    if (c->net.node_count) c->net.node_count--;
    node_changes(c, &old, &c->net);
    reset_tcp(c, ip);
    p->role = ROLE_IN; p->node = -1;
    push_netinfo(c); changed(c);
}
static void leave_network(lan_ctx *c, int state, int reason) {   // the network ends here: back to AccessPoint or Station
    reset_tcp(c, 0);
    memset(&c->net, 0, sizeof c->net); c->my_ip = 0; c->my_node = -1; c->hub = 0;
    memset(c->upd, 0, sizeof c->upd);
    c->reason = reason;
    set_state(c, state);
}
static void station_lost(lan_ctx *c, int reason) {
    LOGI("lan: left the network, reason %d", reason);
    leave_network(c, LDN_STATION, reason);
    browse(c, 1);
}

// ---------- delivery to this device's sockets ----------
static sk *sk_at(lan_ctx *c, int fd) { int i = fd - LAN_FD_BASE; return i >= 0 && i < LAN_FD_MAX && c->sks[i].used ? &c->sks[i] : NULL; }
static int sk_alloc(lan_ctx *c) { for (int i = 0; i < LAN_FD_MAX; i++) if (!c->sks[i].used) { memset(&c->sks[i], 0, sizeof(sk)); c->sks[i].used = 1; c->sks[i].rcvtimeo_ms = c->sks[i].sndtimeo_ms = -1; return i; } return -1; }
static void route(lan_ctx *c, uint64_t from, uint8_t *f, uint32_t n);
static void send_tuple(lan_ctx *c, int type, uint32_t src, uint16_t sport, uint32_t dst, uint16_t dport, const void *data, uint32_t len) {
    uint8_t *f = c->scratch; f[0] = (uint8_t)type;
    wr32(f + 1, src); wr16(f + 5, sport); wr32(f + 7, dst); wr16(f + 11, dport);
    if (len) memcpy(f + TUPLE, data, len);
    route(c, 0, f, TUPLE + len);
}
static int rx_append(sk *s, const uint8_t *d, uint32_t n) {
    if (s->rxn + n > RX_MAX) return -1;
    if (s->rxn + n > s->rxcap) { size_t cap = s->rxcap ? s->rxcap : 16384; while (cap < s->rxn + n) cap *= 2; uint8_t *r = realloc(s->rx, cap); if (!r) return -1; s->rx = r; s->rxcap = cap; }
    memcpy(s->rx + s->rxn, d, n); s->rxn += n; return 0;
}
static void deliver(lan_ctx *c, const uint8_t *f, uint32_t n) {
    int type = f[0]; uint32_t src = rd32(f + 1), dst = rd32(f + 7); uint16_t sport = rd16(f + 5), dport = rd16(f + 11);
    const uint8_t *d = f + TUPLE; uint32_t len = n - TUPLE;
    if (type == F_UDP) {
        int b = is_bcast(c, dst);
        for (int i = 0; i < LAN_FD_MAX; i++) {
            sk *s = &c->sks[i];
            if (!s->used || s->type != SK_SOCK_DGRAM || s->port != dport || (s->bind_ip && s->bind_ip != dst && !b)) continue;
            if (s->peer_ip && (s->peer_ip != src || s->peer_port != sport)) continue;   // a connected UDP socket takes its peer's only
            if (!s->q && !(s->q = calloc(DGRAM_MAX_Q, sizeof(dgram)))) return;
            if (s->qn == DGRAM_MAX_Q || s->qbytes + len > DGRAM_MAX_BYTES) { dgram *o = &s->q[s->qhead]; s->qbytes -= o->len; free(o->data); s->qhead = (s->qhead + 1) % DGRAM_MAX_Q; s->qn--; }
            dgram *g = &s->q[(s->qhead + s->qn) % DGRAM_MAX_Q];
            g->data = malloc(len ? len : 1); if (!g->data) return;
            memcpy(g->data, d, len); g->len = len; g->src = src; g->sport = sport; g->dst = dst; g->dport = dport;
            s->qn++; s->qbytes += len;
            if (!b) break;
        }
        return;
    }
    if (type == F_SYN) {
        sk *l = NULL;
        for (int i = 0; i < LAN_FD_MAX; i++) { sk *s = &c->sks[i]; if (s->used && s->type == SK_SOCK_STREAM && s->st == ST_LISTEN && s->port == dport && (!s->bind_ip || s->bind_ip == dst)) { l = s; break; } }
        int ok = 0;
        if (l && l->acn < l->backlog) {
            int k = sk_alloc(c);
            if (k >= 0) {
                sk *a = &c->sks[k]; a->type = SK_SOCK_STREAM; a->st = ST_CONNECTED; a->bind_ip = dst; a->port = dport; a->peer_ip = src; a->peer_port = sport;
                a->accepted = 0; l->acq[l->acn++] = k; ok = 1;
            }
        }
        uint8_t a = (uint8_t)ok;
        send_tuple(c, F_SYNACK, dst, dport, src, sport, &a, 1);
        return;
    }
    for (int i = 0; i < LAN_FD_MAX; i++) {   // the connection the frame belongs to
        sk *s = &c->sks[i];
        if (!s->used || s->type != SK_SOCK_STREAM || s->port != dport || s->peer_ip != src || s->peer_port != sport) continue;
        if (type == F_SYNACK && s->st == ST_CONNECTING) {
            if (len >= 1 && d[0]) s->st = ST_CONNECTED; else { s->st = ST_CLOSED; s->err = SK_ECONNREFUSED; }
        } else if (type == F_DATA && s->st == ST_CONNECTED && !s->eof) {
            if (rx_append(s, d, len)) { LOGR("lan: a TCP connection had more than %u bytes unread; reset", RX_MAX); s->reset = 1; s->st = ST_CLOSED; uint8_t r = 1; send_tuple(c, F_FIN, s->bind_ip ? s->bind_ip : c->my_ip, s->port, s->peer_ip, s->peer_port, &r, 1); }
        } else if (type == F_FIN) {
            if (len >= 1 && d[0]) { s->reset = 1; s->st = ST_CLOSED; } else s->eof = 1;
        }
        return;
    }
}

// A routed frame from this device (from = 0) or from a peer: where it goes. The hub checks a station's source address, so no station can speak for another
static void route(lan_ctx *c, uint64_t from, uint8_t *f, uint32_t n) {
    if (n < TUPLE || !lan_active(c)) return;
    uint32_t src = rd32(f + 1), dst = rd32(f + 7);
    int to_me = dst == c->my_ip || (dst >> 24) == 127, b = f[0] == F_UDP && is_bcast(c, dst);
    if (c->state == LDN_AP_CREATED) {
        peer *p = from ? peer_of(c, from) : NULL;
        if (from && (!p || p->role != ROLE_MEMBER || c->net.nodes[p->node].ip != src)) return;
        if (to_me || b) deliver(c, f, n);
        if (to_me) return;
        for (int i = 0; i < MAX_PEERS; i++) {
            peer *q = &c->peers[i];
            if (!q->used || q->role != ROLE_MEMBER || q->h == from) continue;
            if (b || c->net.nodes[q->node].ip == dst) { send_to(c, q->h, f, n); if (!b) return; }
        }
        if (!b && f[0] == F_SYN) { uint8_t no = 0; uint8_t r[TUPLE + 1]; r[0] = F_SYNACK; wr32(r + 1, dst); wr16(r + 5, rd16(f + 11)); wr32(r + 7, src); wr16(r + 11, rd16(f + 5)); r[TUPLE] = no; route(c, 0, r, sizeof r); }   // nobody there: refused
        return;
    }
    if (from) { if (from == c->hub && (to_me || b)) deliver(c, f, n); return; }
    if (to_me) { deliver(c, f, n); return; }
    if (b) deliver(c, f, n);   // a broadcast comes back to its sender, as on a Switch's network
    send_to(c, c->hub, f, n);
}

// ---------- frames from peers ----------
static void refuse(lan_ctx *c, peer *p, uint32_t res) { uint8_t f[5]; f[0] = F_REFUSED; wr32(f + 1, res); send_to(c, p->h, f, sizeof f); LOGI("lan: a player was refused (%#x)", res); }
static void hub_join(lan_ctx *c, peer *p, const uint8_t *f, uint32_t n) {
    if (n < 1 + JOIN_LEN || p->role != ROLE_IN) return;
    const uint8_t *q = f + 1;
    if (rd32(q) != MAGIC || rd16(q + 4) != VERSION) { refuse(c, p, LDN_CONNECT_REJECTED); return; }
    ldn_user_config user; memcpy(&user, q + 6, sizeof user); cstr(user.user_name, sizeof user.user_name);
    int16_t lcv = (int16_t)rd16(q + 0x36); int priv = q[0x38]; uint16_t mode = rd16(q + 0x39), psz = rd16(q + 0x3B);
    const uint8_t *pass = q + 0x3D, *session = q + 0x7D;
    if (priv && memcmp(session, c->net.id.session, 16)) { refuse(c, p, LDN_CONNECT_NOT_FOUND); return; }
    if (mode != c->sec_mode || psz != c->pass_size || psz > 0x40 || memcmp(pass, c->pass, psz)) { refuse(c, p, LDN_CONNECT_REJECTED); return; }
    if (c->accept_policy == LDN_ACCEPT_NEVER) { refuse(c, p, LDN_CONNECT_REJECTED); return; }
    if (lcv < c->net.nodes[0].lcv) { refuse(c, p, LDN_VERSION_TOO_LOW); return; }
    if (lcv > c->net.nodes[0].lcv) { refuse(c, p, LDN_VERSION_TOO_HIGH); return; }
    int max = c->net.node_count_max > 0 && c->net.node_count_max <= 8 ? c->net.node_count_max : 8, node = -1;
    if (c->net.node_count < max) for (int i = 1; i < max; i++) if (!c->net.nodes[i].connected) { node = i; break; }
    if (node < 0) { refuse(c, p, LDN_TOO_MANY_PLAYERS); return; }
    ldn_network_info old = c->net; ldn_node *nd = &c->net.nodes[node];
    memset(nd, 0, sizeof *nd);
    nd->ip = (c->my_ip & NET_MASK) | (uint32_t)(node + 1); rnd(nd->mac, 6); nd->mac[0] = (nd->mac[0] & 0xFE) | 0x02;
    nd->node_id = (int8_t)node; nd->connected = 1; memcpy(nd->user_name, user.user_name, sizeof nd->user_name); nd->lcv = lcv;
    c->net.node_count++; node_changes(c, &old, &c->net);
    p->role = ROLE_MEMBER; p->node = node;
    uint8_t r[1 + 1 + 4 + sizeof(ldn_network_info)]; r[0] = F_JOINED; r[1] = (uint8_t)node; wr32(r + 2, nd->ip); memcpy(r + 6, &c->net, sizeof c->net);
    send_to(c, p->h, r, sizeof r);
    LOGI("lan: player %d (%s) joined", node, nd->user_name);
    push_netinfo(c); changed(c);
}
static int netinfo_ok(ldn_network_info *in) {   // what a hub sends is checked before the game sees it
    if (in->node_count > 8 || in->node_count_max > 8 || in->adv_size > sizeof in->adv || in->common.ssid_len > 32) return 0;
    cstr(in->common.ssid, sizeof in->common.ssid);
    for (int i = 0; i < 8; i++) cstr(in->nodes[i].user_name, sizeof in->nodes[i].user_name);
    return 1;
}
void lan_on_frame(lan_ctx *c, uint64_t h, const uint8_t *f, uint32_t n) {
    if (!n || n > LAN_FRAME_MAX) return;
    lock(c);
    peer *p = peer_of(c, h);
    if (!p) { unlock(c); return; }
    int type = f[0];
    if (type == F_PING && n == 9) { uint8_t r[9]; memcpy(r, f, 9); r[0] = F_PONG; send_to(c, h, r, 9); unlock(c); return; }
    if (type == F_PONG && n == 9) {
        uint64_t sent = (uint64_t)rd32(f + 1) | (uint64_t)rd32(f + 5) << 32, now = now_ms();
        if (sent <= now && now - sent < 60000) p->rtt_ms = (int)(now - sent);
        unlock(c); return;
    }
    if (type >= F_UDP && type <= F_FIN) {
        if (n >= TUPLE && n <= sizeof c->scratch) { uint8_t *copy = malloc(n); if (copy) { memcpy(copy, f, n); route(c, h, copy, n); free(copy); } }
    } else if (p->role == ROLE_IN || p->role == ROLE_MEMBER) {   // hub side
        if (c->state != LDN_AP_CREATED) { peer_drop(c, p); }
        else if (type == F_PROBE && n >= 7 && rd32(f + 1) == MAGIC) { uint8_t r[1 + sizeof(ldn_network_info)]; r[0] = F_NETINFO; memcpy(r + 1, &c->net, sizeof c->net); send_to(c, h, r, sizeof r); send_room(c, h); }
        else if (type == F_JOIN) hub_join(c, p, f, n);
        else if (type == F_LEAVE) { node_remove(c, p, n >= 2 ? f[1] : LDN_DR_BY_USER); peer_drop(c, p); }   // the station is gone: its connection too
    } else if (p->role == ROLE_HUB) {   // station side
        if (type == F_NETINFO && n == 1 + sizeof(ldn_network_info)) {
            ldn_network_info in; memcpy(&in, f + 1, sizeof in);
            if (netinfo_ok(&in)) {
                p->info = in; p->have_info = 1;
                if (h == c->hub && c->state == LDN_STATION_CONNECTED) { node_changes(c, &c->net, &in); c->net = in; changed(c); }
            }
        } else if (type == F_ROOM && n <= LAN_ROOM_MAX) {
            memcpy(p->room, f + 1, n - 1); p->room[n - 1] = 0;
            for (char *q = p->room; *q; q++) if ((unsigned char)*q < 0x20) *q = ' ';   // shown as text: no control characters
            changed(c);
        } else if (type == F_JOINED && n == 6 + sizeof(ldn_network_info) && c->join_peer == h && !c->join_done) {
            ldn_network_info in; memcpy(&in, f + 6, sizeof in);
            if (f[1] >= 1 && f[1] <= 7 && netinfo_ok(&in)) { c->my_node = f[1]; c->my_ip = rd32(f + 2); c->net = in; p->info = in; c->join_res = LDN_OK; }
            else c->join_res = LDN_CONNECT_FAILURE;
            c->join_done = 1;
        } else if (type == F_REFUSED && n == 5 && c->join_peer == h && !c->join_done) {
            uint32_t r = rd32(f + 1);
            c->join_res = r == LDN_CONNECT_REJECTED || r == LDN_TOO_MANY_PLAYERS || r == LDN_VERSION_TOO_LOW || r == LDN_VERSION_TOO_HIGH || r == LDN_CONNECT_NOT_FOUND ? r : LDN_CONNECT_FAILURE;
            c->join_done = 1;
        } else if (type == F_LEAVE && h == c->hub && c->state == LDN_STATION_CONNECTED) {
            int r = n >= 2 && f[1] <= LDN_DR_SIGNAL_LOST ? f[1] : LDN_DR_DESTROYED_BY_USER;
            c->hub = 0; peer_drop(c, p); station_lost(c, r);
        }
    }
    unlock(c);
}

void lan_on_peer(lan_ctx *c, uint64_t h, int ready, uint64_t service) {
    lock(c);
    peer *p = peer_of(c, h);
    if (ready) {
        if (!p && !service) {   // a device connected to us
            if (c->state != LDN_AP_CREATED || !(p = peer_new(c, h))) { c->t.close(c->t.self, h); unlock(c); return; }
            p->role = ROLE_IN;
        }
        if (p) {
            p->ready = 1;
            if (p->role == ROLE_HUB) { uint8_t f[7]; f[0] = F_PROBE; wr32(f + 1, MAGIC); wr16(f + 5, VERSION); send_to(c, h, f, sizeof f); }
        }
    } else if (p) {
        if (p->role == ROLE_MEMBER) node_remove(c, p, LDN_DR_SIGNAL_LOST);
        int was_hub = h == c->hub;
        if (c->join_peer == h && !c->join_done) { c->join_res = LDN_CONNECT_FAILURE; c->join_done = 1; }
        p->used = 0;
        if (was_hub && c->state == LDN_STATION_CONNECTED) { c->hub = 0; station_lost(c, LDN_DR_SIGNAL_LOST); }
    }
    unlock(c);
}

void lan_on_service(lan_ctx *c, uint64_t service, int found) {
    lock(c);
    peer *p = NULL;
    for (int i = 0; i < MAX_PEERS; i++) if (c->peers[i].used && c->peers[i].role == ROLE_HUB && c->peers[i].service == service) { p = &c->peers[i]; break; }
    if (found && !p && c->browsing) {
        uint64_t h = c->t.connect(c->t.self, service);
        if (h && (p = peer_new(c, h))) { p->role = ROLE_HUB; p->service = service; }
        else if (h) c->t.close(c->t.self, h);
    } else if (!found && p && p->h != c->hub) peer_drop(c, p);
    unlock(c);
}

// ---------- nn::ldn ----------
lan_ctx *lan_new(const lan_transport *t, const lan_config *cfg) {
    lan_ctx *c = calloc(1, sizeof *c); if (!c) return NULL;
    c->t = *t; c->cfg = *cfg; c->my_node = -1; c->next_port = 49152;
    pthread_mutex_init(&c->mu, NULL); pthread_cond_init(&c->cv, NULL);
    return c;
}
void lan_free(lan_ctx *c) {
    if (!c) return;
    lan_ldn_finalize(c);
    for (int i = 0; i < LAN_FD_MAX; i++) if (c->sks[i].used) sk_free_data(&c->sks[i]);
    pthread_mutex_destroy(&c->mu); pthread_cond_destroy(&c->cv); free(c);
}
uint32_t lan_ldn_initialize(lan_ctx *c) { lock(c); if (c->state == LDN_NONE) set_state(c, LDN_INITIALIZED); unlock(c); return LDN_OK; }
int lan_ldn_state(lan_ctx *c) { lock(c); int s = c->state; unlock(c); return s; }
int lan_ldn_disconnect_reason(lan_ctx *c) { lock(c); int r = c->reason; unlock(c); return r; }

static void hub_close(lan_ctx *c, int reason) {   // the hub ends its network
    uint8_t f[2] = {F_LEAVE, (uint8_t)reason};
    for (int i = 0; i < MAX_PEERS; i++) if (c->peers[i].used && (c->peers[i].role == ROLE_MEMBER || c->peers[i].role == ROLE_IN)) { if (c->peers[i].role == ROLE_MEMBER) send_to(c, c->peers[i].h, f, 2); peer_drop(c, &c->peers[i]); }
    c->t.advertise(c->t.self, 0);
}
static void station_close(lan_ctx *c, int reason) {   // the station leaves its hub
    if (c->hub) { uint8_t f[2] = {F_LEAVE, (uint8_t)reason}; send_to(c, c->hub, f, 2); peer *p = peer_of(c, c->hub); c->hub = 0; if (p) peer_drop(c, p); }
}
void lan_ldn_finalize(lan_ctx *c) {
    lock(c);
    if (c->state == LDN_AP_CREATED) hub_close(c, LDN_DR_DESTROYED_BY_USER);
    if (c->state == LDN_STATION_CONNECTED) station_close(c, LDN_DR_BY_USER);
    browse(c, 0);
    for (int i = 0; i < MAX_PEERS; i++) if (c->peers[i].used) peer_drop(c, &c->peers[i]);
    leave_network(c, LDN_NONE, LDN_DR_NONE);
    unlock(c);
}
uint32_t lan_ldn_open_ap(lan_ctx *c) {
    lock(c); uint32_t r = LDN_OK;
    if (c->state != LDN_INITIALIZED) r = LDN_INVALID_STATE;
    else { browse(c, 0); c->adv_size = 0; c->accept_policy = LDN_ACCEPT_ALWAYS; c->nfilter = 0; set_state(c, LDN_AP); }
    unlock(c); return r;
}
uint32_t lan_ldn_destroy(lan_ctx *c) {
    lock(c); uint32_t r = LDN_OK;
    if (c->state != LDN_AP_CREATED) r = LDN_INVALID_STATE;
    else { hub_close(c, LDN_DR_DESTROYED_BY_USER); leave_network(c, LDN_AP, LDN_DR_NONE); }
    unlock(c); return r;
}
uint32_t lan_ldn_close_ap(lan_ctx *c) {
    lock(c); uint32_t r = LDN_OK;
    if (c->state == LDN_AP_CREATED) { hub_close(c, LDN_DR_DESTROYED_BY_USER); leave_network(c, LDN_INITIALIZED, LDN_DR_NONE); }
    else if (c->state == LDN_AP) set_state(c, LDN_INITIALIZED);
    else r = LDN_INVALID_STATE;
    unlock(c); return r;
}
uint32_t lan_ldn_create(lan_ctx *c, const ldn_network_config *cfg, const ldn_security_config *sec, const ldn_security_param *param, const ldn_user_config *user) {
    if (!cfg || !sec || !user) return LDN_INVALID_ARGUMENT;
    if (cfg->node_count_max > 8 || cfg->node_count_max < 0 || cfg->lcv < 0 || sec->passphrase_size > 0x40) return LDN_INVALID_ARGUMENT;
    lock(c);
    if (c->state != LDN_AP) { unlock(c); return LDN_INVALID_STATE; }
    browse(c, 0);
    ldn_network_info *n = &c->net; memset(n, 0, sizeof *n);
    n->id.intent = cfg->intent;
    if (n->id.intent.local_comm_id == -1) n->id.intent.local_comm_id = (int64_t)c->cfg.program_id;   // the application's own
    if (param) { memcpy(n->id.session, param->session, 16); memcpy(n->server_random, param->server_random, 16); }
    else { rnd(n->id.session, 16); rnd(n->server_random, 16); }
    rnd(n->common.bssid, 6); n->common.bssid[0] = (n->common.bssid[0] & 0xFE) | 0x02;
    { uint8_t r[16]; rnd(r, 16); for (int i = 0; i < 16; i++) snprintf(n->common.ssid + 2 * i, 3, "%02x", r[i]); n->common.ssid_len = 32; }
    n->common.channel = cfg->channel ? cfg->channel : 6; n->common.link_level = 3; n->common.network_type = 2;
    n->security_mode = sec->security_mode; n->accept_policy = (uint8_t)c->accept_policy;
    n->node_count_max = cfg->node_count_max ? cfg->node_count_max : 8;
    for (int i = 0; i < 8; i++) n->nodes[i].node_id = (int8_t)i;
    uint8_t x; rnd(&x, 1); x = (uint8_t)(1 + x % 254);   // 169.254.x.0/24, the hub at .1
    c->my_ip = 0xA9FE0000u | (uint32_t)x << 8 | 1; c->my_node = 0;
    ldn_node *me = &n->nodes[0]; me->ip = c->my_ip; rnd(me->mac, 6); me->mac[0] = (me->mac[0] & 0xFE) | 0x02;
    me->connected = 1; memcpy(me->user_name, user->user_name, sizeof me->user_name); cstr(me->user_name, sizeof me->user_name); me->lcv = cfg->lcv;
    n->node_count = 1;
    n->adv_size = c->adv_size; memcpy(n->adv, c->adv, c->adv_size);
    c->sec_mode = sec->security_mode; c->pass_size = sec->passphrase_size; memcpy(c->pass, sec->passphrase, sec->passphrase_size);
    memset(c->upd, 0, sizeof c->upd); c->upd[0].state_change = 1;
    c->reason = LDN_DR_NONE;
    c->t.advertise(c->t.self, 1);
    LOGI("lan: network created (%d players at most, local communication ID %016llx), this device at %u.%u.%u.%u", n->node_count_max,
         (unsigned long long)n->id.intent.local_comm_id, c->my_ip >> 24, c->my_ip >> 16 & 255, c->my_ip >> 8 & 255, c->my_ip & 255);
    set_state(c, LDN_AP_CREATED);
    unlock(c); return LDN_OK;
}
uint32_t lan_ldn_set_advertise(lan_ctx *c, const void *data, size_t size) {
    if (size > sizeof c->adv || (size && !data)) return LDN_INVALID_ARGUMENT;
    lock(c); uint32_t r = LDN_OK;
    if (c->state != LDN_AP && c->state != LDN_AP_CREATED) r = LDN_INVALID_STATE;
    else {
        memcpy(c->adv, data, size); c->adv_size = (uint16_t)size;
        if (c->state == LDN_AP_CREATED) { memset(c->net.adv, 0, sizeof c->net.adv); memcpy(c->net.adv, data, size); c->net.adv_size = (uint16_t)size; push_netinfo(c); changed(c); }
    }
    unlock(c); return r;
}
uint32_t lan_ldn_set_accept_policy(lan_ctx *c, int policy) {
    if (policy < 0 || policy > LDN_ACCEPT_WHITELIST) return LDN_INVALID_ARGUMENT;
    lock(c); uint32_t r = LDN_OK;
    if (c->state != LDN_AP && c->state != LDN_AP_CREATED) r = LDN_INVALID_STATE;
    else { c->accept_policy = policy; c->net.accept_policy = (uint8_t)policy; if (c->state == LDN_AP_CREATED) push_netinfo(c); }
    unlock(c); return r;
}
uint32_t lan_ldn_add_accept_filter(lan_ctx *c, const uint8_t mac[6]) {   // the lists name devices by MAC address, which are made up here: kept, not applied
    lock(c); if (c->nfilter < 8) memcpy(c->filter[c->nfilter++], mac, 6); unlock(c); return LDN_OK;
}
uint32_t lan_ldn_reject(lan_ctx *c, uint32_t ip) {
    lock(c); uint32_t r = LDN_NODE_NOT_FOUND;
    if (c->state != LDN_AP_CREATED) r = LDN_INVALID_STATE;
    else for (int i = 0; i < MAX_PEERS; i++) {
        peer *p = &c->peers[i];
        if (p->used && p->role == ROLE_MEMBER && c->net.nodes[p->node].ip == ip) {
            uint8_t f[2] = {F_LEAVE, LDN_DR_REJECTED}; send_to(c, p->h, f, 2);
            node_remove(c, p, LDN_DR_REJECTED); peer_drop(c, p); r = LDN_OK; break;
        }
    }
    unlock(c); return r;
}
uint32_t lan_ldn_open_station(lan_ctx *c) {
    lock(c); uint32_t r = LDN_OK;
    if (c->state != LDN_INITIALIZED) r = LDN_INVALID_STATE;
    else { c->reason = LDN_DR_NONE; set_state(c, LDN_STATION); browse(c, 1); }
    unlock(c); return r;
}
uint32_t lan_ldn_disconnect(lan_ctx *c) {
    lock(c); uint32_t r = LDN_OK;
    if (c->state != LDN_STATION_CONNECTED) r = LDN_INVALID_STATE;
    else { station_close(c, LDN_DR_BY_USER); leave_network(c, LDN_STATION, LDN_DR_BY_USER); browse(c, 1); }
    unlock(c); return r;
}
uint32_t lan_ldn_close_station(lan_ctx *c) {
    lock(c); uint32_t r = LDN_OK;
    if (c->state == LDN_STATION_CONNECTED) { station_close(c, LDN_DR_BY_USER); leave_network(c, LDN_STATION, LDN_DR_BY_USER); }
    if (c->state != LDN_STATION) r = LDN_INVALID_STATE;
    else { browse(c, 0); set_state(c, LDN_INITIALIZED); }
    unlock(c); return r;
}
static int matches(lan_ctx *c, const ldn_network_info *n, const ldn_scan_filter *f) {
    if (!n->node_count || !n->nodes[0].connected) return 0;
    uint32_t fl = f ? f->flags : 0;
    if (fl & LDN_SF_LOCAL_COMM_ID) { int64_t want = f->id.intent.local_comm_id == -1 ? (int64_t)c->cfg.program_id : f->id.intent.local_comm_id; if (n->id.intent.local_comm_id != want) return 0; }
    if ((fl & LDN_SF_SESSION_ID) && memcmp(n->id.session, f->id.session, 16)) return 0;
    if ((fl & LDN_SF_NETWORK_TYPE) && f->network_type != 3 && f->network_type != n->common.network_type) return 0;
    if ((fl & LDN_SF_BSSID) && memcmp(n->common.bssid, f->bssid, 6)) return 0;
    if ((fl & LDN_SF_SSID) && (f->ssid_len != n->common.ssid_len || memcmp(f->ssid, n->common.ssid, f->ssid_len > 32 ? 32 : f->ssid_len))) return 0;
    if ((fl & LDN_SF_SCENE_ID) && n->id.intent.scene_id != f->id.intent.scene_id) return 0;
    return 1;
}
static int scan_now(lan_ctx *c, ldn_network_info *out, int max, const ldn_scan_filter *f) {
    int k = 0;
    for (int i = 0; i < MAX_PEERS && (!out || k < max); i++) {
        peer *p = &c->peers[i];
        if (p->used && p->role == ROLE_HUB && p->have_info && matches(c, &p->info, f)) { if (out) out[k] = p->info; k++; }
    }
    return k;
}
uint32_t lan_ldn_scan(lan_ctx *c, ldn_network_info *out, int *count, int max, const ldn_scan_filter *f) {
    if (!out || !count || max <= 0) return LDN_INVALID_ARGUMENT;
    lock(c);
    if (c->state != LDN_STATION && c->state != LDN_INITIALIZED && c->state != LDN_AP) { unlock(c); return LDN_INVALID_STATE; }
    browse(c, 1);
    // A scan takes a moment on a Switch too: the first one waits for the hubs nearby to answer, later ones return what is known
    uint64_t full = c->browse_since + 1200, some = c->browse_since + 400;
    for (;;) {
        uint64_t now = now_ms(); int k = scan_now(c, NULL, 0, f);
        if (now >= full || (k && now >= some)) break;
        wait_until(c, k ? some : full);
    }
    *count = scan_now(c, out, max, f);
    unlock(c); return LDN_OK;
}
uint32_t lan_ldn_connect(lan_ctx *c, const ldn_network_info *info, const ldn_network_config *cfg, const ldn_security_config *sec, const ldn_security_param *param,
                         const ldn_user_config *user, int lcv) {
    if (!sec || !user || (!info && (!cfg || !param))) return LDN_INVALID_ARGUMENT;
    if (sec->passphrase_size > 0x40) return LDN_INVALID_ARGUMENT;
    if (lcv < 0) return LDN_VERSION_TOO_LOW;
    if (lcv > 0x7FFF) return LDN_VERSION_TOO_HIGH;
    lock(c);
    if (c->state != LDN_STATION) { unlock(c); return LDN_INVALID_STATE; }
    const uint8_t *session = info ? info->id.session : param->session;
    peer *p = NULL; uint64_t until = now_ms() + (info ? 1500 : 4000);   // a private network was never scanned: give the search a few seconds
    for (;;) {
        for (int i = 0; i < MAX_PEERS; i++) if (c->peers[i].used && c->peers[i].role == ROLE_HUB && c->peers[i].have_info && !memcmp(c->peers[i].info.id.session, session, 16)) { p = &c->peers[i]; break; }
        if (p || !wait_until(c, until)) break;
    }
    if (!p) { unlock(c); LOGI("lan: connect: the network is not in reach"); return LDN_CONNECT_NOT_FOUND; }
    uint8_t f[1 + JOIN_LEN] = {F_JOIN}; uint8_t *q = f + 1;
    wr32(q, MAGIC); wr16(q + 4, VERSION); memcpy(q + 6, user, sizeof *user); wr16(q + 0x36, (uint16_t)lcv); q[0x38] = info ? 0 : 1;
    wr16(q + 0x39, sec->security_mode); wr16(q + 0x3B, sec->passphrase_size); memcpy(q + 0x3D, sec->passphrase, sec->passphrase_size); memcpy(q + 0x7D, session, 16);
    uint64_t h = p->h; c->join_peer = h; c->join_done = 0; c->join_res = LDN_CONNECT_TIMEOUT;
    send_to(c, h, f, sizeof f);
    uint64_t deadline = now_ms() + 5000;
    while (!c->join_done && wait_until(c, deadline)) {}
    uint32_t r = c->join_done ? c->join_res : LDN_CONNECT_TIMEOUT;
    c->join_peer = 0;
    if (r == LDN_OK && c->state == LDN_STATION) {
        c->hub = h; c->reason = LDN_DR_NONE;
        memset(c->upd, 0, sizeof c->upd); for (int i = 0; i < 8; i++) if (c->net.nodes[i].connected) c->upd[i].state_change = 1;
        browse(c, 0);   // the other hubs' probes close; this one stays
        LOGI("lan: joined as player %d at %u.%u.%u.%u", c->my_node, c->my_ip >> 24, c->my_ip >> 16 & 255, c->my_ip >> 8 & 255, c->my_ip & 255);
        set_state(c, LDN_STATION_CONNECTED);
    } else if (r == LDN_OK) r = LDN_CONNECT_FAILURE;
    unlock(c); return r;
}
void lan_set_room_name(lan_ctx *c, const char *name) {
    lock(c);
    snprintf(c->room, sizeof c->room, "%s", name ? name : "");
    if (c->state == LDN_AP_CREATED) for (int i = 0; i < MAX_PEERS; i++) if (c->peers[i].used && c->peers[i].ready && (c->peers[i].role == ROLE_IN || c->peers[i].role == ROLE_MEMBER)) send_room(c, c->peers[i].h);
    unlock(c);
}
void lan_status_get(lan_ctx *c, lan_status *out) {
    memset(out, 0, sizeof *out);
    lock(c);
    out->state = c->state;
    uint64_t now = now_ms(); int ping = now - c->pinged >= 1000;
    if (ping) c->pinged = now;
    if (lan_active(c)) {
        for (int i = 0; i < 8; i++) {
            const ldn_node *nd = &c->net.nodes[i];
            if (!nd->connected) continue;
            lan_status_player *pl = &out->players[out->nplayers++];
            memcpy(pl->name, nd->user_name, sizeof pl->name); pl->name[sizeof pl->name - 1] = 0;
            pl->me = i == c->my_node; pl->rtt_ms = -1;
            peer *p = NULL;
            if (c->state == LDN_AP_CREATED) { for (int k = 0; k < MAX_PEERS; k++) if (c->peers[k].used && c->peers[k].role == ROLE_MEMBER && c->peers[k].node == i) { p = &c->peers[k]; break; } }
            else if (i == 0 && c->hub) p = peer_of(c, c->hub);   // a station reaches the others through the hub only
            if (p) { pl->peer = p->h; pl->rtt_ms = p->rtt_ms; }
        }
        if (c->state == LDN_AP_CREATED) snprintf(out->room, sizeof out->room, "%s", c->room);
    }
    for (int i = 0; i < MAX_PEERS && out->nrooms < 16; i++) {
        peer *p = &c->peers[i];
        if (!p->used || p->role != ROLE_HUB || !p->have_info || !p->info.node_count) continue;
        lan_status_room *r = &out->rooms[out->nrooms++];
        memcpy(r->host, p->info.nodes[0].user_name, sizeof r->host); r->host[sizeof r->host - 1] = 0;
        snprintf(r->room, sizeof r->room, "%s", p->room);
        r->count = p->info.node_count; r->max = p->info.node_count_max; r->rtt_ms = p->rtt_ms; r->peer = p->h; r->service = p->service;
        r->joined = p->h == c->hub && c->state == LDN_STATION_CONNECTED;
        if (r->joined) snprintf(out->room, sizeof out->room, "%s", p->room);
    }
    if (ping) {   // the devices listed: members (hub), the hub joined and the rooms found (station)
        uint8_t f[9]; f[0] = F_PING; wr32(f + 1, (uint32_t)now); wr32(f + 5, (uint32_t)(now >> 32));
        for (int i = 0; i < MAX_PEERS; i++) {
            peer *p = &c->peers[i];
            if (p->used && p->ready && (p->role == ROLE_MEMBER || (p->role == ROLE_HUB && p->have_info))) send_to(c, p->h, f, sizeof f);
        }
    }
    unlock(c);
}
uint32_t lan_ldn_network_info(lan_ctx *c, ldn_network_info *out, ldn_node_update *updates, int nupdates) {
    lock(c); uint32_t r = LDN_OK;
    if (!lan_active(c)) r = LDN_INVALID_STATE;
    else {
        if (out) *out = c->net;
        if (updates) { for (int i = 0; i < nupdates && i < 8; i++) updates[i] = c->upd[i]; memset(c->upd, 0, sizeof c->upd); }
    }
    unlock(c); return r;
}
uint32_t lan_ldn_ipv4(lan_ctx *c, uint32_t *ip, uint32_t *mask) {
    lock(c); uint32_t r = LDN_OK;
    if (!lan_active(c)) r = LDN_INVALID_STATE;
    else { if (ip) *ip = c->my_ip; if (mask) *mask = NET_MASK; }
    unlock(c); return r;
}
uint32_t lan_ldn_security_param(lan_ctx *c, ldn_security_param *out) {
    lock(c); uint32_t r = LDN_OK;
    if (!lan_active(c)) r = LDN_INVALID_STATE;
    else if (out) { memcpy(out->server_random, c->net.server_random, 16); memcpy(out->session, c->net.id.session, 16); }
    unlock(c); return r;
}
uint32_t lan_ldn_network_config(lan_ctx *c, ldn_network_config *out) {
    lock(c); uint32_t r = LDN_OK;
    if (!lan_active(c)) r = LDN_INVALID_STATE;
    else if (out) { memset(out, 0, sizeof *out); out->intent = c->net.id.intent; out->channel = c->net.common.channel; out->node_count_max = c->net.node_count_max; out->lcv = c->net.nodes[0].lcv; }
    unlock(c); return r;
}

// ---------- sockets ----------
int lan_is_fd(lan_ctx *c, int fd) { lock(c); int r = sk_at(c, fd) != NULL; unlock(c); return r; }
static int parse_addr(const void *a, uint32_t len, uint32_t *ip, uint16_t *port) {
    if (!a || len < 8) return -SK_EINVAL;
    const sk_sockaddr_in *s = a;
    if (s->family != SK_AF_INET) return -SK_EAFNOSUPPORT;
    *port = (uint16_t)((s->port_be & 0xFF) << 8 | s->port_be >> 8);
    const uint8_t *b = (const uint8_t *)&s->addr_be; *ip = (uint32_t)b[0] << 24 | (uint32_t)b[1] << 16 | (uint32_t)b[2] << 8 | b[3];
    return 0;
}
static void put_addr(void *a, uint32_t *len, uint32_t ip, uint16_t port) {
    if (!a || !len) return;
    sk_sockaddr_in s = {0}; s.len = sizeof s; s.family = SK_AF_INET; s.port_be = (uint16_t)(port >> 8 | (port & 0xFF) << 8);
    uint8_t *b = (uint8_t *)&s.addr_be; b[0] = (uint8_t)(ip >> 24); b[1] = (uint8_t)(ip >> 16); b[2] = (uint8_t)(ip >> 8); b[3] = (uint8_t)ip;
    uint32_t n = *len < sizeof s ? *len : sizeof s; memcpy(a, &s, n); *len = sizeof s;
}
static int port_used(lan_ctx *c, int type, uint16_t port, uint32_t ip, const sk *self) {
    for (int i = 0; i < LAN_FD_MAX; i++) { sk *s = &c->sks[i]; if (s->used && s != self && s->type == type && s->port == port && !(s->type == SK_SOCK_STREAM && s->st == ST_CONNECTED && s->accepted >= 0 && s->peer_ip) && (!s->bind_ip || !ip || s->bind_ip == ip) && !(s->reuse && self->reuse)) return 1; }
    return 0;
}
static int autobind(lan_ctx *c, sk *s) {
    if (s->port) return 0;
    for (int k = 0; k < 16384; k++) { uint16_t p = c->next_port++; if (c->next_port < 49152) c->next_port = 49152; if (!port_used(c, s->type, p, 0, s)) { s->port = p; return 0; } }
    return -SK_EADDRINUSE;
}
int lan_sk_socket(lan_ctx *c, int domain, int type, int proto) {
    int nb = (type & SK_SOCK_NONBLOCK) != 0; type &= ~(SK_SOCK_NONBLOCK | SK_SOCK_CLOEXEC);
    if (domain != SK_AF_INET) return -SK_EAFNOSUPPORT;
    if (type != SK_SOCK_STREAM && type != SK_SOCK_DGRAM) return -SK_EPROTOTYPE;
    if (proto && !(type == SK_SOCK_STREAM && proto == SK_IPPROTO_TCP) && !(type == SK_SOCK_DGRAM && proto == SK_IPPROTO_UDP)) return -SK_EPROTONOSUPPORT;
    lock(c);
    if (!lan_active(c)) { unlock(c); return -SK_ENETDOWN; }
    int i = sk_alloc(c);
    if (i < 0) { unlock(c); return -SK_EMFILE; }
    c->sks[i].type = type; c->sks[i].nonblock = nb; c->sks[i].accepted = -1;
    unlock(c); return i + LAN_FD_BASE;
}
int lan_sk_bind(lan_ctx *c, int fd, const void *addr, uint32_t len) {
    uint32_t ip; uint16_t port; int e = parse_addr(addr, len, &ip, &port); if (e) return e;
    lock(c); sk *s = sk_at(c, fd); int r = 0;
    if (!s) r = -SK_EBADF;
    else if (s->port) r = -SK_EINVAL;
    else if (ip && ip != c->my_ip && !is_bcast(c, ip) && (ip >> 24) != 127) r = -SK_EADDRNOTAVAIL;
    else if (port && port_used(c, s->type, port, ip, s)) r = -SK_EADDRINUSE;
    else { s->bind_ip = is_bcast(c, ip) || (ip >> 24) == 127 ? 0 : ip; if (port) s->port = port; else r = autobind(c, s); }
    unlock(c); return r;
}
static int wait_ready(lan_ctx *c, int64_t timeout_ms, int (*ready)(lan_ctx *, sk *), sk *s) {   // 0 ready, -EAGAIN at the timeout
    uint64_t dl = deadline_of(timeout_ms);
    while (!ready(c, s)) if (!wait_until(c, dl)) return -SK_EAGAIN;
    return 0;
}
static int r_connected(lan_ctx *c, sk *s) { (void)c; return s->st != ST_CONNECTING; }
int lan_sk_connect(lan_ctx *c, int fd, const void *addr, uint32_t len) {
    uint32_t ip; uint16_t port; int e = parse_addr(addr, len, &ip, &port); if (e) return e;
    lock(c); sk *s = sk_at(c, fd); int r = 0;
    if (!s) r = -SK_EBADF;
    else if (!lan_active(c)) r = -SK_ENETDOWN;
    else if (!on_net(c, ip) && (ip >> 24) != 127) r = -SK_ENETUNREACH;
    else if (s->type == SK_SOCK_DGRAM) { if (!(r = autobind(c, s))) { s->peer_ip = ip; s->peer_port = port; } }
    else if (s->st == ST_CONNECTED) r = -SK_EISCONN;
    else if (s->st == ST_CONNECTING) r = -SK_EALREADY;
    else if (s->st != ST_NEW) r = -SK_EINVAL;
    else if (!(r = autobind(c, s))) {
        s->peer_ip = (ip >> 24) == 127 ? c->my_ip : ip; s->peer_port = port; s->st = ST_CONNECTING; s->err = 0;
        send_tuple(c, F_SYN, s->bind_ip ? s->bind_ip : c->my_ip, s->port, s->peer_ip, port, NULL, 0);
        if (s->nonblock) r = s->st == ST_CONNECTING ? -SK_EINPROGRESS : 0;
        else if (wait_ready(c, 5000, r_connected, s)) { s->st = ST_CLOSED; r = -SK_ETIMEDOUT; }
        if (r == 0 && s->st != ST_CONNECTED) { r = -(s->err ? s->err : SK_ECONNREFUSED); s->err = 0; }
    }
    unlock(c); return r;
}
int lan_sk_listen(lan_ctx *c, int fd, int backlog) {
    lock(c); sk *s = sk_at(c, fd); int r = 0;
    if (!s) r = -SK_EBADF;
    else if (s->type != SK_SOCK_STREAM) r = -SK_EOPNOTSUPP;
    else if (s->st != ST_NEW && s->st != ST_LISTEN) r = -SK_EINVAL;
    else if (!(r = autobind(c, s))) { s->st = ST_LISTEN; s->backlog = backlog < 1 ? 1 : backlog > 16 ? 16 : backlog; }
    unlock(c); return r;
}
static int r_acceptq(lan_ctx *c, sk *s) { (void)c; return s->acn > 0 || !s->used || s->st != ST_LISTEN; }
int lan_sk_accept(lan_ctx *c, int fd, void *addr, uint32_t *len) {
    lock(c); sk *s = sk_at(c, fd); int r;
    if (!s) r = -SK_EBADF;
    else if (s->st != ST_LISTEN) r = -SK_EINVAL;
    else if (!s->acn && s->nonblock) r = -SK_EAGAIN;
    else if ((r = wait_ready(c, s->rcvtimeo_ms, r_acceptq, s)) == 0) {
        if (!s->acn) r = -SK_ECONNABORTED;
        else {
            int k = s->acq[0]; memmove(s->acq, s->acq + 1, (size_t)--s->acn * sizeof s->acq[0]);
            c->sks[k].accepted = 1; c->sks[k].nonblock = s->nonblock;
            put_addr(addr, len, c->sks[k].peer_ip, c->sks[k].peer_port); r = k + LAN_FD_BASE;
        }
    }
    unlock(c); return r;
}
static void sk_end(lan_ctx *c, sk *s) {   // close: the peer hears the end
    if (s->type == SK_SOCK_STREAM && (s->st == ST_CONNECTED || s->st == ST_CONNECTING) && lan_active(c)) { uint8_t rst = 0; send_tuple(c, F_FIN, s->bind_ip ? s->bind_ip : c->my_ip, s->port, s->peer_ip, s->peer_port, &rst, 1); }
    if (s->st == ST_LISTEN) for (int i = 0; i < s->acn; i++) { sk *a = &c->sks[s->acq[i]]; sk_end(c, a); sk_free_data(a); a->used = 0; }
    sk_free_data(s); memset(s, 0, sizeof *s);
}
int lan_sk_close(lan_ctx *c, int fd) { lock(c); sk *s = sk_at(c, fd); int r = 0; if (!s) r = -SK_EBADF; else sk_end(c, s); unlock(c); return r; }
int lan_sk_shutdown(lan_ctx *c, int fd, int how) {
    lock(c); sk *s = sk_at(c, fd); int r = 0;
    if (!s) r = -SK_EBADF;
    else if (s->type == SK_SOCK_STREAM && s->st != ST_CONNECTED) r = -SK_ENOTCONN;
    else if ((how == 1 || how == 2) && s->type == SK_SOCK_STREAM && !s->wr_shut) { s->wr_shut = 1; uint8_t rst = 0; send_tuple(c, F_FIN, s->bind_ip ? s->bind_ip : c->my_ip, s->port, s->peer_ip, s->peer_port, &rst, 1); }
    if (!r && s && (how == 0 || how == 2)) s->eof = 1;
    unlock(c); return r;
}
long lan_sk_sendto(lan_ctx *c, int fd, const void *buf, size_t n, int flags, const void *addr, uint32_t len) {
    (void)flags;
    if (n && !buf) return -SK_EFAULT;
    lock(c); sk *s = sk_at(c, fd); long r;
    if (!s) { unlock(c); return -SK_EBADF; }
    if (!lan_active(c)) { unlock(c); return -SK_ENETDOWN; }
    uint32_t src = s->bind_ip ? s->bind_ip : c->my_ip;
    if (s->type == SK_SOCK_DGRAM) {
        uint32_t ip = s->peer_ip; uint16_t port = s->peer_port;
        if (addr && len) { int e = parse_addr(addr, len, &ip, &port); if (e) { unlock(c); return e; } }
        if (!ip && !port) r = -SK_EDESTADDRREQ;
        else if (n > 65507) r = -SK_EMSGSIZE;
        else if (!on_net(c, ip) && (ip >> 24) != 127) r = -SK_ENETUNREACH;
        else if ((r = autobind(c, s)) == 0) { send_tuple(c, F_UDP, src, s->port, (ip >> 24) == 127 ? c->my_ip : ip, port, buf, (uint32_t)n); r = (long)n; }
    } else {
        if (s->st != ST_CONNECTED || s->reset) r = s->reset ? -SK_ECONNRESET : -SK_ENOTCONN;
        else if (s->wr_shut) r = -SK_EPIPE;
        else { for (size_t o = 0; o < n; o += TCP_CHUNK) { size_t k = n - o < TCP_CHUNK ? n - o : TCP_CHUNK; send_tuple(c, F_DATA, src, s->port, s->peer_ip, s->peer_port, (const uint8_t *)buf + o, (uint32_t)k); } r = (long)n; }
    }
    unlock(c); return r;
}
static int r_readable(lan_ctx *c, sk *s) { (void)c; return !s->used || (s->type == SK_SOCK_DGRAM ? s->qn > 0 : s->rxn > 0 || s->eof || s->reset || s->st == ST_CLOSED); }
long lan_sk_recvfrom(lan_ctx *c, int fd, void *buf, size_t n, int flags, void *addr, uint32_t *len) {
    if (n && !buf) return -SK_EFAULT;
    lock(c); sk *s = sk_at(c, fd); long r = 0;
    if (!s) { unlock(c); return -SK_EBADF; }
    if (s->type == SK_SOCK_STREAM && s->st != ST_CONNECTED && s->st != ST_CLOSED) { unlock(c); return -SK_ENOTCONN; }
    int nb = s->nonblock || (flags & SK_MSG_DONTWAIT);
    if (!r_readable(c, s)) {
        if (nb) { unlock(c); return -SK_EAGAIN; }
        if ((r = wait_ready(c, s->rcvtimeo_ms, r_readable, s))) { unlock(c); return r; }
        if (!s->used) { unlock(c); return -SK_EBADF; }
    }
    if (s->type == SK_SOCK_DGRAM) {
        dgram *g = &s->q[s->qhead]; size_t k = g->len < n ? g->len : n;
        memcpy(buf, g->data, k); put_addr(addr, len, g->src, g->sport); r = (long)k;   // a longer datagram is cut, as UDP's recv does
        if (!(flags & SK_MSG_PEEK)) { s->qbytes -= g->len; free(g->data); g->data = NULL; s->qhead = (s->qhead + 1) % DGRAM_MAX_Q; s->qn--; }
    } else {
        if (!s->rxn) r = s->reset && !s->eof ? -SK_ECONNRESET : 0;   // 0: the peer ended its data
        else {
            size_t k = s->rxn < n ? s->rxn : n; memcpy(buf, s->rx, k);
            if (!(flags & SK_MSG_PEEK)) { memmove(s->rx, s->rx + k, s->rxn - k); s->rxn -= k; }
            put_addr(addr, len, s->peer_ip, s->peer_port); r = (long)k;
        }
    }
    unlock(c); return r;
}
int lan_sk_name(lan_ctx *c, int fd, int peer, void *addr, uint32_t *len) {
    lock(c); sk *s = sk_at(c, fd); int r = 0;
    if (!s) r = -SK_EBADF;
    else if (peer) { if (!s->peer_ip || (s->type == SK_SOCK_STREAM && s->st != ST_CONNECTED)) r = -SK_ENOTCONN; else put_addr(addr, len, s->peer_ip, s->peer_port); }
    else put_addr(addr, len, s->bind_ip ? s->bind_ip : (s->type == SK_SOCK_STREAM && s->st == ST_CONNECTED ? c->my_ip : 0), s->port);
    unlock(c); return r;
}
static int64_t tv_ms(const void *v, uint32_t len) {   // the Switch's timeval: two 64-bit fields
    if (len < 16) return -1;
    int64_t sec, usec; memcpy(&sec, v, 8); memcpy(&usec, (const uint8_t *)v + 8, 8);
    int64_t ms = sec * 1000 + usec / 1000; return ms <= 0 ? -1 : ms;
}
int lan_sk_setsockopt(lan_ctx *c, int fd, int level, int opt, const void *val, uint32_t len) {
    lock(c); sk *s = sk_at(c, fd); int r = 0;
    if (!s) r = -SK_EBADF;
    else if (level == SK_SOL_SOCKET && val && len >= 4) {
        int v; memcpy(&v, val, 4);
        if (opt == SK_SO_RCVTIMEO) s->rcvtimeo_ms = tv_ms(val, len);
        else if (opt == SK_SO_SNDTIMEO) s->sndtimeo_ms = tv_ms(val, len);
        else if (opt == SK_SO_BROADCAST) s->broadcast = v != 0;
        else if (opt == SK_SO_REUSEADDR || opt == SK_SO_REUSEPORT) s->reuse = v != 0;
    }   // the rest (buffer sizes, keepalive, no delay, multicast) is accepted and has no effect here
    unlock(c); return r;
}
int lan_sk_getsockopt(lan_ctx *c, int fd, int level, int opt, void *val, uint32_t *len) {
    lock(c); sk *s = sk_at(c, fd); int r = 0;
    if (!s) r = -SK_EBADF;
    else if (!val || !len || *len < 4) r = -SK_EINVAL;
    else {
        int v = 0;
        if (level == SK_SOL_SOCKET) {
            if (opt == SK_SO_ERROR) { v = s->err; s->err = 0; }
            else if (opt == SK_SO_TYPE) v = s->type;
            else if (opt == SK_SO_RCVBUF || opt == SK_SO_SNDBUF) v = 0x40000;
            else if (opt == SK_SO_BROADCAST) v = s->broadcast;
            else if (opt == SK_SO_REUSEADDR) v = s->reuse;
        }
        memcpy(val, &v, 4); *len = 4;
    }
    unlock(c); return r;
}
int lan_sk_fcntl(lan_ctx *c, int fd, int cmd, int arg) {
    lock(c); sk *s = sk_at(c, fd); int r = 0;
    if (!s) r = -SK_EBADF;
    else if (cmd == SK_F_GETFL) r = s->nonblock ? SK_O_NONBLOCK : 0;
    else if (cmd == SK_F_SETFL) s->nonblock = (arg & SK_O_NONBLOCK) != 0;
    else if (cmd == SK_F_GETFD || cmd == SK_F_SETFD) r = 0;
    else r = -SK_EINVAL;
    unlock(c); return r;
}
static int16_t revents(lan_ctx *c, sk *s, int16_t ev) {
    int16_t r = 0;
    if (s->type == SK_SOCK_DGRAM) { if (s->qn) r |= SK_POLLIN | SK_POLLRDNORM; if (lan_active(c)) r |= SK_POLLOUT; }
    else if (s->st == ST_LISTEN) { if (s->acn) r |= SK_POLLIN | SK_POLLRDNORM; }
    else {
        if (s->rxn || s->eof || s->reset) r |= SK_POLLIN | SK_POLLRDNORM;
        if (s->st == ST_CONNECTED && !s->wr_shut) r |= SK_POLLOUT;
        if (s->reset || (s->st == ST_CLOSED && s->err)) r |= SK_POLLHUP | (s->err ? SK_POLLERR : 0);
        if (s->st == ST_CLOSED && s->err) r |= SK_POLLOUT;   // a failed non-blocking connect reports itself writable, as BSD's
    }
    return (int16_t)(r & (ev | SK_POLLERR | SK_POLLHUP | SK_POLLNVAL));
}
int lan_sk_poll(lan_ctx *c, sk_pollfd *fds, uint64_t n, int timeout_ms) {
    if (n && !fds) return -SK_EFAULT;
    lock(c);
    uint64_t dl = deadline_of(timeout_ms);
    for (;;) {
        int k = 0;
        for (uint64_t i = 0; i < n; i++) {
            if (fds[i].fd < 0) { fds[i].revents = 0; continue; }
            sk *s = sk_at(c, fds[i].fd);
            fds[i].revents = s ? revents(c, s, fds[i].events) : SK_POLLNVAL;
            if (fds[i].revents) k++;
        }
        if (k || timeout_ms == 0 || !wait_until(c, dl)) { unlock(c); return k; }
    }
}
int lan_sk_select(lan_ctx *c, int nfds, uint64_t *rs, uint64_t *ws, uint64_t *es, int timeout_ms) {
    if (nfds > 1024) nfds = 1024;
    uint64_t want[3][16] = {{0}}; uint64_t *sets[3] = {rs, ws, es};
    for (int k = 0; k < 3; k++) if (sets[k]) memcpy(want[k], sets[k], 128);
    lock(c);
    uint64_t dl = deadline_of(timeout_ms);
    for (;;) {
        int total = 0; uint64_t got[3][16] = {{0}};
        for (int fd = 0; fd < nfds; fd++) {
            int w = fd >> 6; uint64_t bit = 1ull << (fd & 63);
            if (!((want[0][w] | want[1][w] | want[2][w]) & bit)) continue;
            sk *s = sk_at(c, fd);
            if (!s) { unlock(c); return -SK_EBADF; }
            int16_t ev = revents(c, s, SK_POLLIN | SK_POLLOUT);
            if ((want[0][w] & bit) && (ev & (SK_POLLIN | SK_POLLHUP))) { got[0][w] |= bit; total++; }
            if ((want[1][w] & bit) && (ev & SK_POLLOUT)) { got[1][w] |= bit; total++; }
            if ((want[2][w] & bit) && (ev & SK_POLLERR)) { got[2][w] |= bit; total++; }
        }
        if (total || timeout_ms == 0 || !wait_until(c, dl)) {
            for (int k = 0; k < 3; k++) if (sets[k]) memcpy(sets[k], got[k], 128);
            unlock(c); return total;
        }
    }
}
