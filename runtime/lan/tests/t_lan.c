// Local play core tests: several devices in one process over an in-process transport (frames, services and connections through one delivery thread, as
// the real transport's queue). Build and run: runtime/lan/tests/run.sh (any computer with a C compiler; AddressSanitizer and UBSan on).
#include "../lan_core.h"
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int g_fail, g_checks;
#define CHECK(cond, ...) do { g_checks++; if (!(cond)) { g_fail++; printf("FAIL %s:%d: %s: ", __FILE__, __LINE__, #cond); printf(__VA_ARGS__); printf("\n"); } } while (0)

// ---------- the in-process transport ----------
#define NDEV 6
enum { EV_SERVICE, EV_PEER, EV_FRAME };
typedef struct ev { struct ev *next; int type, dev, flag; uint64_t a, b; uint8_t *data; uint32_t len; } ev;
typedef struct { int used, closed; int dev[2]; uint64_t h[2]; } conn;
static struct {
    pthread_mutex_t mu; pthread_cond_t cv; ev *head, *tail; int stop, busy;
    lan_ctx *ctx[NDEV]; int advertising[NDEV], browsing[NDEV];
    conn conns[256]; uint64_t next_h;
    int events[NDEV];   // state change events per device
} T = {.mu = PTHREAD_MUTEX_INITIALIZER, .cv = PTHREAD_COND_INITIALIZER};

static void push(int type, int dev, int flag, uint64_t a, uint64_t b, const void *data, uint32_t len) {   // under T.mu
    ev *e = calloc(1, sizeof *e); e->type = type; e->dev = dev; e->flag = flag; e->a = a; e->b = b;
    if (len) { e->data = malloc(len); memcpy(e->data, data, len); e->len = len; }
    if (T.tail) T.tail->next = e; else T.head = e;
    T.tail = e; pthread_cond_broadcast(&T.cv);
}
static void *pump(void *x) {
    (void)x;
    pthread_mutex_lock(&T.mu);
    for (;;) {
        while (!T.head && !T.stop) pthread_cond_wait(&T.cv, &T.mu);
        if (!T.head) break;
        ev *e = T.head; T.head = e->next; if (!T.head) T.tail = NULL;
        T.busy = 1; pthread_mutex_unlock(&T.mu);
        lan_ctx *c = T.ctx[e->dev];
        if (c) {
            if (e->type == EV_SERVICE) lan_on_service(c, e->a, e->flag);
            else if (e->type == EV_PEER) lan_on_peer(c, e->a, e->flag, e->b);
            else lan_on_frame(c, e->a, e->data, e->len);
        }
        free(e->data); free(e);
        pthread_mutex_lock(&T.mu); T.busy = 0; pthread_cond_broadcast(&T.cv);
    }
    pthread_mutex_unlock(&T.mu);
    return NULL;
}
static void settle(void) {   // until every queued event is delivered
    for (int i = 0; i < 3; i++) {
        pthread_mutex_lock(&T.mu); while (T.head || T.busy) pthread_cond_wait(&T.cv, &T.mu); pthread_mutex_unlock(&T.mu);
        usleep(20000);
    }
}
static void t_advertise(void *self, int on) {
    int d = (int)(intptr_t)self;
    pthread_mutex_lock(&T.mu);
    if (T.advertising[d] != on) { T.advertising[d] = on; for (int i = 0; i < NDEV; i++) if (i != d && T.browsing[i]) push(EV_SERVICE, i, on, (uint64_t)d + 1, 0, NULL, 0); }
    pthread_mutex_unlock(&T.mu);
}
static void t_browse(void *self, int on) {
    int d = (int)(intptr_t)self;
    pthread_mutex_lock(&T.mu);
    T.browsing[d] = on;
    if (on) for (int i = 0; i < NDEV; i++) if (i != d && T.advertising[i]) push(EV_SERVICE, d, 1, (uint64_t)i + 1, 0, NULL, 0);
    pthread_mutex_unlock(&T.mu);
}
static uint64_t t_connect(void *self, uint64_t service) {
    int d = (int)(intptr_t)self, hub = (int)service - 1; uint64_t mine = 0;
    pthread_mutex_lock(&T.mu);
    if (hub >= 0 && hub < NDEV && T.advertising[hub])
        for (int i = 0; i < 256; i++) if (!T.conns[i].used) {
            conn *k = &T.conns[i]; k->used = 1; k->closed = 0; k->dev[0] = d; k->dev[1] = hub; k->h[0] = mine = ++T.next_h; k->h[1] = ++T.next_h;
            push(EV_PEER, hub, 1, k->h[1], 0, NULL, 0); push(EV_PEER, d, 1, k->h[0], service, NULL, 0);
            break;
        }
    pthread_mutex_unlock(&T.mu);
    return mine;
}
static conn *conn_of(uint64_t h, int *side) { for (int i = 0; i < 256; i++) for (int s = 0; s < 2; s++) if (T.conns[i].used && T.conns[i].h[s] == h) { *side = s; return &T.conns[i]; } return NULL; }
static int t_send(void *self, uint64_t peer, const void *frame, uint32_t len) {
    (void)self; int side; pthread_mutex_lock(&T.mu);
    conn *k = conn_of(peer, &side);
    if (k && !k->closed) push(EV_FRAME, k->dev[!side], 0, k->h[!side], 0, frame, len);
    pthread_mutex_unlock(&T.mu); return 0;
}
static void t_close(void *self, uint64_t peer) {
    (void)self; int side; pthread_mutex_lock(&T.mu);
    conn *k = conn_of(peer, &side);
    if (k && !k->closed) { k->closed = 1; push(EV_PEER, k->dev[0], 0, k->h[0], 0, NULL, 0); push(EV_PEER, k->dev[1], 0, k->h[1], 0, NULL, 0); k->used = 0; }
    pthread_mutex_unlock(&T.mu);
}
static void on_state(void *user) { int d = (int)(intptr_t)user; __atomic_add_fetch(&T.events[d], 1, __ATOMIC_RELAXED); }
static void on_log(void *user, int red, const char *msg) { printf("  [dev %d]%s %s\n", (int)(intptr_t)user, red ? " [RED]" : "", msg); }
static lan_ctx *device(int d) {
    lan_transport t = {(void *)(intptr_t)d, t_advertise, t_browse, t_connect, t_send, t_close};
    lan_config cfg = {0x0100152000022000ull, on_state, on_log, (void *)(intptr_t)d};
    return T.ctx[d] = lan_new(&t, &cfg);
}

// ---------- helpers ----------
static ldn_network_config netcfg(int max, int lcv) { ldn_network_config n = {0}; n.intent.local_comm_id = -1; n.intent.scene_id = 7; n.node_count_max = (int8_t)max; n.lcv = (int16_t)lcv; return n; }
static ldn_security_config sec(const char *pass) { ldn_security_config s = {0}; s.security_mode = 1; s.passphrase_size = (uint16_t)strlen(pass); memcpy(s.passphrase, pass, strlen(pass)); return s; }
static ldn_user_config user(const char *name) { ldn_user_config u = {0}; snprintf(u.user_name, sizeof u.user_name, "%s", name); return u; }
static sk_sockaddr_in addr(uint32_t ip, uint16_t port) {
    sk_sockaddr_in a = {0}; a.len = 16; a.family = SK_AF_INET; a.port_be = (uint16_t)(port >> 8 | (port & 0xFF) << 8);
    uint8_t *b = (uint8_t *)&a.addr_be; b[0] = (uint8_t)(ip >> 24); b[1] = (uint8_t)(ip >> 16); b[2] = (uint8_t)(ip >> 8); b[3] = (uint8_t)ip; return a;
}
static uint32_t ip_of(const sk_sockaddr_in *a) { const uint8_t *b = (const uint8_t *)&a->addr_be; return (uint32_t)b[0] << 24 | (uint32_t)b[1] << 16 | (uint32_t)b[2] << 8 | b[3]; }
static void set_rcvtimeo(lan_ctx *c, int fd, int ms) { int64_t tv[2] = {ms / 1000, (ms % 1000) * 1000}; lan_sk_setsockopt(c, fd, SK_SOL_SOCKET, SK_SO_RCVTIMEO, tv, 16); }
static int join(lan_ctx *c, const char *name, const char *pass, int lcv) {
    ldn_network_info found[4]; int n = 0; ldn_scan_filter f = {0}; f.flags = LDN_SF_LOCAL_COMM_ID; f.id.intent.local_comm_id = -1;
    uint32_t r = lan_ldn_scan(c, found, &n, 4, &f);
    if (r || n < 1) return -1;
    ldn_security_config s = sec(pass); ldn_user_config u = user(name);
    return (int)lan_ldn_connect(c, &found[0], NULL, &s, NULL, &u, lcv);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    pthread_t th; pthread_create(&th, NULL, pump, NULL);
    lan_ctx *A = device(0), *B = device(1), *C = device(2), *D = device(3), *E = device(4);

    printf("== the hub creates a network\n");
    CHECK(lan_ldn_initialize(A) == LDN_OK, "init");
    CHECK(lan_sk_socket(A, SK_AF_INET, SK_SOCK_DGRAM, 0) == -SK_ENETDOWN, "no sockets before a network");
    CHECK(lan_ldn_open_ap(A) == LDN_OK, "open ap");
    CHECK(lan_ldn_set_advertise(A, "hello", 5) == LDN_OK, "advertise");
    ldn_network_config nc = netcfg(4, 3); ldn_security_config s = sec("secret"); ldn_user_config ua = user("Hub");
    CHECK(lan_ldn_create(A, &nc, &s, NULL, &ua) == LDN_OK, "create");
    CHECK(lan_ldn_state(A) == LDN_AP_CREATED, "state %d", lan_ldn_state(A));
    uint32_t ipA = 0, mask = 0; lan_ldn_ipv4(A, &ipA, &mask);
    CHECK((ipA & 0xFFFF0000u) == 0xA9FE0000u && (ipA & 0xFF) == 1 && mask == 0xFFFFFF00u, "hub address %08x", ipA);

    printf("== stations scan and join\n");
    lan_ldn_initialize(B); lan_ldn_open_station(B);
    ldn_network_info found[4]; int n = 0; ldn_scan_filter f = {0}; f.flags = LDN_SF_LOCAL_COMM_ID | LDN_SF_SCENE_ID; f.id.intent.local_comm_id = -1; f.id.intent.scene_id = 7;
    CHECK(lan_ldn_scan(B, found, &n, 4, &f) == LDN_OK && n == 1, "scan found %d", n);
    CHECK(n == 1 && found[0].adv_size == 5 && !memcmp(found[0].adv, "hello", 5) && !strcmp(found[0].nodes[0].user_name, "Hub"), "advertise data and the hub's name");
    CHECK(n == 1 && found[0].id.intent.local_comm_id == 0x0100152000022000ll, "default local communication ID");
    f.id.intent.scene_id = 8; int n2 = 0; lan_ldn_scan(B, found + 1, &n2, 3, &f); CHECK(n2 == 0, "other scene: %d", n2);
    ldn_user_config ub = user("Bee");
    CHECK(lan_ldn_connect(B, &found[0], NULL, &s, NULL, &ub, 3) == LDN_OK, "B joins");
    CHECK(lan_ldn_state(B) == LDN_STATION_CONNECTED, "B connected");
    lan_ldn_initialize(C); lan_ldn_open_station(C);
    CHECK(join(C, "Cee", "secret", 3) == LDN_OK, "C joins");
    settle();
    ldn_network_info ni; lan_ldn_network_info(A, &ni, NULL, 0); CHECK(ni.node_count == 3, "hub sees %d players", ni.node_count);
    lan_ldn_network_info(B, &ni, NULL, 0); CHECK(ni.node_count == 3 && !strcmp(ni.nodes[2].user_name, "Cee"), "B sees %d players", ni.node_count);
    uint32_t ipB = 0, ipC = 0; lan_ldn_ipv4(B, &ipB, NULL); lan_ldn_ipv4(C, &ipC, NULL);
    CHECK((ipB & 0xFFFFFF00u) == (ipA & 0xFFFFFF00u) && ipB != ipA && ipC != ipB, "addresses %08x %08x %08x", ipA, ipB, ipC);
    CHECK(T.events[0] > 0 && T.events[1] > 0, "state change events");

    printf("== refusals\n");
    lan_ldn_initialize(D); lan_ldn_open_station(D);
    CHECK(join(D, "Dee", "wrong", 3) == (int)LDN_CONNECT_REJECTED, "wrong passphrase");
    CHECK(join(D, "Dee", "secret", 2) == (int)LDN_VERSION_TOO_LOW, "older version");
    CHECK(join(D, "Dee", "secret", 4) == (int)LDN_VERSION_TOO_HIGH, "newer version");
    CHECK(join(D, "Dee", "secret", 3) == LDN_OK, "D joins (the 4th of 4)");
    lan_ldn_initialize(E); lan_ldn_open_station(E);
    CHECK(join(E, "Eee", "secret", 3) == (int)LDN_TOO_MANY_PLAYERS, "a 5th is too many");

    printf("== UDP: unicast through the hub, broadcast to everyone\n");
    int ua_ = lan_sk_socket(A, SK_AF_INET, SK_SOCK_DGRAM, SK_IPPROTO_UDP), ub_ = lan_sk_socket(B, SK_AF_INET, SK_SOCK_DGRAM, 0), uc = lan_sk_socket(C, SK_AF_INET, SK_SOCK_DGRAM, 0);
    sk_sockaddr_in any = addr(0, 7000);
    CHECK(lan_sk_bind(A, ua_, &any, 16) == 0 && lan_sk_bind(B, ub_, &any, 16) == 0 && lan_sk_bind(C, uc, &any, 16) == 0, "bind 7000");
    CHECK(lan_sk_bind(B, lan_sk_socket(B, SK_AF_INET, SK_SOCK_DGRAM, 0), &any, 16) == -SK_EADDRINUSE, "port taken");
    sk_sockaddr_in toA = addr(ipA, 7000), toC = addr(ipC, 7000), bc = addr((ipA & 0xFFFFFF00u) | 0xFF, 7000);
    CHECK(lan_sk_sendto(B, ub_, "ping", 4, 0, &toA, 16) == 4, "B → A");
    char buf[256]; sk_sockaddr_in from; uint32_t fl = 16; set_rcvtimeo(A, ua_, 2000);
    long k = lan_sk_recvfrom(A, ua_, buf, sizeof buf, 0, &from, &fl);
    CHECK(k == 4 && !memcmp(buf, "ping", 4) && ip_of(&from) == ipB, "A got %ld from %08x", k, ip_of(&from));
    CHECK(lan_sk_sendto(B, ub_, "toC", 3, 0, &toC, 16) == 3, "B → C");
    set_rcvtimeo(C, uc, 2000); fl = 16; k = lan_sk_recvfrom(C, uc, buf, sizeof buf, 0, &from, &fl);
    CHECK(k == 3 && ip_of(&from) == ipB, "C got %ld from B through the hub", k);
    CHECK(lan_sk_sendto(C, uc, "all", 3, 0, &bc, 16) == 3, "C broadcasts");
    set_rcvtimeo(B, ub_, 2000);
    fl = 16; CHECK(lan_sk_recvfrom(A, ua_, buf, sizeof buf, 0, &from, &fl) == 3 && ip_of(&from) == ipC, "A got the broadcast");
    fl = 16; CHECK(lan_sk_recvfrom(B, ub_, buf, sizeof buf, 0, &from, &fl) == 3 && ip_of(&from) == ipC, "B got the broadcast");
    fl = 16; CHECK(lan_sk_recvfrom(C, uc, buf, sizeof buf, 0, &from, &fl) == 3 && ip_of(&from) == ipC, "C got its own broadcast");
    CHECK(lan_sk_recvfrom(B, ub_, buf, sizeof buf, SK_MSG_DONTWAIT, NULL, NULL) == -SK_EAGAIN, "nothing more");
    sk_sockaddr_in off = addr(0x0A000001, 7000);
    CHECK(lan_sk_sendto(B, ub_, "x", 1, 0, &off, 16) == -SK_ENETUNREACH, "outside the network");

    printf("== TCP: B connects to C's listener, both ways, then closes\n");
    int lc = lan_sk_socket(C, SK_AF_INET, SK_SOCK_STREAM, 0); sk_sockaddr_in p8000 = addr(0, 8000);
    CHECK(lan_sk_bind(C, lc, &p8000, 16) == 0 && lan_sk_listen(C, lc, 4) == 0, "C listens");
    int tb = lan_sk_socket(B, SK_AF_INET, SK_SOCK_STREAM, 0); sk_sockaddr_in toC8 = addr(ipC, 8000);
    CHECK(lan_sk_connect(B, tb, &toC8, 16) == 0, "B connects");
    fl = 16; int tc = lan_sk_accept(C, lc, &from, &fl);
    CHECK(tc >= LAN_FD_BASE && ip_of(&from) == ipB, "C accepts %d", tc);
    static uint8_t big[100000], got[100000]; for (int i = 0; i < (int)sizeof big; i++) big[i] = (uint8_t)(i * 31);
    CHECK(lan_sk_sendto(B, tb, big, sizeof big, 0, NULL, 0) == (long)sizeof big, "B sends 100000 bytes");
    size_t have = 0; set_rcvtimeo(C, tc, 2000);
    while (have < sizeof got) { long r = lan_sk_recvfrom(C, tc, got + have, sizeof got - have, 0, NULL, NULL); if (r <= 0) break; have += (size_t)r; }
    CHECK(have == sizeof big && !memcmp(big, got, sizeof big), "C got %zu bytes intact", have);
    CHECK(lan_sk_sendto(C, tc, "back", 4, 0, NULL, 0) == 4, "C answers");
    set_rcvtimeo(B, tb, 2000); CHECK(lan_sk_recvfrom(B, tb, buf, sizeof buf, 0, NULL, NULL) == 4, "B got the answer");
    lan_sk_close(C, tc);
    CHECK(lan_sk_recvfrom(B, tb, buf, sizeof buf, 0, NULL, NULL) == 0, "B sees the end");
    int tr = lan_sk_socket(B, SK_AF_INET, SK_SOCK_STREAM, 0); sk_sockaddr_in toC9 = addr(ipC, 9000);
    CHECK(lan_sk_connect(B, tr, &toC9, 16) == -SK_ECONNREFUSED, "nobody listens on 9000");
    int tn = lan_sk_socket(B, SK_AF_INET, SK_SOCK_STREAM | SK_SOCK_NONBLOCK, 0); sk_sockaddr_in toA8 = addr(ipA, 8100);
    int la = lan_sk_socket(A, SK_AF_INET, SK_SOCK_STREAM, 0); sk_sockaddr_in p8100 = addr(0, 8100); lan_sk_bind(A, la, &p8100, 16); lan_sk_listen(A, la, 2);
    CHECK(lan_sk_connect(B, tn, &toA8, 16) == -SK_EINPROGRESS, "non-blocking connect");
    sk_pollfd pf = {tn, SK_POLLOUT, 0}; CHECK(lan_sk_poll(B, &pf, 1, 2000) == 1 && (pf.revents & SK_POLLOUT), "poll: connected");
    int err = -1; uint32_t el = 4; lan_sk_getsockopt(B, tn, SK_SOL_SOCKET, SK_SO_ERROR, &err, &el); CHECK(err == 0, "SO_ERROR %d", err);
    uint64_t rs[16] = {0}; rs[la >> 6] |= 1ull << (la & 63);
    CHECK(lan_sk_select(A, la + 1, rs, NULL, NULL, 2000) == 1, "select: a connection to accept");

    printf("== malformed frames from a station are ignored\n");
    {
        uint8_t junk[2048]; srand(7);
        for (int i = 0; i < 20000; i++) {
            uint32_t len = 1 + (uint32_t)rand() % sizeof junk; for (uint32_t j = 0; j < len; j++) junk[j] = (uint8_t)rand();
            if (i % 4 == 0) junk[0] = (uint8_t)(1 + rand() % 14);
            if (junk[0] == 6) junk[0] = 7;   // a real leave (the station's own connection) would end its membership
            pthread_mutex_lock(&T.mu);
            for (int q = 0; q < 256; q++) if (T.conns[q].used && T.conns[q].dev[1] == 0 && !T.conns[q].closed) { push(EV_FRAME, 0, 0, T.conns[q].h[1], 0, junk, len); break; }   // to the hub, as from a station
            pthread_mutex_unlock(&T.mu);
        }
        settle();
        lan_ldn_network_info(A, &ni, NULL, 0); CHECK(ni.node_count == 4, "the hub still has %d players", ni.node_count);
    }

    printf("== the overlay's status: players, rooms found, round trips, the room's name\n");
    {
        lan_set_room_name(A, "Hub's den");
        lan_ldn_finalize(E); lan_ldn_initialize(E); lan_ldn_open_station(E);
        int nf = 0; ldn_network_info fe[4]; lan_ldn_scan(E, fe, &nf, 4, &f);   // E looks without joining
        lan_status st; lan_status_get(A, &st);
        CHECK(st.state == LDN_AP_CREATED && st.nplayers == 4 && !strcmp(st.room, "Hub's den"), "hub: state %d, %d players, room \"%s\"", st.state, st.nplayers, st.room);
        CHECK(st.players[0].me && !strcmp(st.players[0].name, "Hub") && !st.players[0].peer && st.players[1].peer && !st.players[1].me, "hub: itself first, then members with their connection");
        lan_status_get(E, &st);
        CHECK(st.state == LDN_STATION && st.nrooms == 1 && !strcmp(st.rooms[0].host, "Hub") && !strcmp(st.rooms[0].room, "Hub's den") && st.rooms[0].count == 4 && st.rooms[0].max == 4 && !st.rooms[0].joined,
              "looking: %d rooms, \"%s\" by %s, %d/%d", st.nrooms, st.nrooms ? st.rooms[0].room : "", st.nrooms ? st.rooms[0].host : "", st.nrooms ? st.rooms[0].count : 0, st.nrooms ? st.rooms[0].max : 0);
        settle(); usleep(1100 * 1000);
        lan_status_get(A, &st); lan_status_get(E, &st); lan_status_get(C, &st);   // the second call a second later sends the pings
        settle();
        lan_status_get(A, &st);
        int known = 0; for (int i = 1; i < st.nplayers; i++) known += st.players[i].rtt_ms >= 0;
        CHECK(known == 3 && st.players[0].rtt_ms == -1, "hub: round trips to %d of 3 members", known);
        lan_status_get(E, &st); CHECK(st.nrooms == 1 && st.rooms[0].rtt_ms >= 0, "looking: round trip to the room %d", st.nrooms ? st.rooms[0].rtt_ms : -2);
        lan_status_get(C, &st);
        CHECK(st.state == LDN_STATION_CONNECTED && !strcmp(st.room, "Hub's den") && st.nplayers == 4 && st.players[0].peer && st.players[0].rtt_ms >= 0, "joined: room \"%s\", host's round trip %d", st.room, st.players[0].rtt_ms);
        int me = -1; for (int i = 0; i < st.nplayers; i++) if (st.players[i].me) me = i;
        CHECK(me >= 0 && !strcmp(st.players[me].name, "Cee"), "joined: itself among the players");
        int joined = 0; for (int i = 0; i < st.nrooms; i++) joined += st.rooms[i].joined;
        CHECK(joined == 1, "joined: the room it is in is marked");
        lan_set_room_name(A, "Renamed"); settle();
        lan_status_get(C, &st); CHECK(!strcmp(st.room, "Renamed"), "a new name reaches the players: \"%s\"", st.room);
        lan_ldn_close_station(E);
    }

    printf("== the hub sends a player away, then ends the network\n");
    CHECK(lan_ldn_reject(A, ipB) == LDN_OK, "reject B");
    settle();
    CHECK(lan_ldn_state(B) == LDN_STATION && lan_ldn_disconnect_reason(B) == LDN_DR_REJECTED, "B: state %d reason %d", lan_ldn_state(B), lan_ldn_disconnect_reason(B));
    CHECK(lan_sk_sendto(B, ub_, "x", 1, 0, &toA, 16) == -SK_ENETDOWN, "B's socket is off the network");
    CHECK(lan_ldn_destroy(A) == LDN_OK, "destroy");
    settle();
    CHECK(lan_ldn_state(C) == LDN_STATION && lan_ldn_disconnect_reason(C) == LDN_DR_DESTROYED_BY_USER, "C: state %d reason %d", lan_ldn_state(C), lan_ldn_disconnect_reason(C));
    CHECK(lan_ldn_state(A) == LDN_AP, "hub back to AccessPoint");

    printf("== a private network: joined by its session, without a scan\n");
    ldn_security_param sp; memset(&sp, 0x5A, sizeof sp);
    CHECK(lan_ldn_create(A, &nc, &s, &sp, &ua) == LDN_OK, "create private");
    ldn_network_config pc = netcfg(4, 3);
    CHECK(lan_ldn_connect(C, NULL, &pc, &s, &sp, &ub, 3) == LDN_OK, "connect private");
    ldn_security_param other; memset(&other, 0x11, sizeof other);
    lan_ldn_finalize(D); lan_ldn_initialize(D); lan_ldn_open_station(D);
    CHECK(lan_ldn_connect(D, NULL, &pc, &s, &other, &ub, 3) == LDN_CONNECT_NOT_FOUND, "another session is not found");

    printf("== malformed frames from a hub are ignored by its station\n");
    {
        uint8_t junk[2048]; srand(11);
        for (int i = 0; i < 20000; i++) {
            uint32_t len = 1 + (uint32_t)rand() % sizeof junk; for (uint32_t j = 0; j < len; j++) junk[j] = (uint8_t)rand();
            if (i % 3 == 0) junk[0] = (uint8_t)(1 + rand() % 14);
            if (junk[0] == 6) junk[0] = 2;   // a real leave would end the station's membership
            if (i % 50 == 0) { junk[0] = 2; len = 1 + sizeof(ldn_network_info); }   // network info of the right size with nonsense inside
            pthread_mutex_lock(&T.mu);
            for (int q = 0; q < 256; q++) if (T.conns[q].used && T.conns[q].dev[0] == 2 && !T.conns[q].closed) { push(EV_FRAME, 2, 0, T.conns[q].h[0], 0, junk, len); break; }   // to C, as from its hub
            pthread_mutex_unlock(&T.mu);
        }
        settle();
        CHECK(lan_ldn_state(C) == LDN_STATION_CONNECTED, "C is still connected");
        lan_ldn_network_info(C, &ni, NULL, 0);
        CHECK(ni.node_count <= 8 && ni.adv_size <= sizeof ni.adv && ni.common.ssid_len <= 32, "what C keeps is sane");
    }

    for (int i = 0; i < 5; i++) lan_ldn_finalize(T.ctx[i]);
    settle();
    pthread_mutex_lock(&T.mu); T.stop = 1; pthread_cond_broadcast(&T.cv); pthread_mutex_unlock(&T.mu);
    pthread_join(th, NULL);
    for (int i = 0; i < 5; i++) { lan_free(T.ctx[i]); T.ctx[i] = NULL; }
    printf("%d checks, %d failed\n", g_checks, g_fail);
    return g_fail != 0;
}
