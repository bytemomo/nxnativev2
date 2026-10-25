// Local play between devices: the Switch's local wireless (nn::ldn) and the sockets games open on it (nn::socket), over a transport that carries
// whole frames between devices (lan_nw.m: Bonjour + Apple's peer-to-peer Wi-Fi + TLS; tests/: an in-process one).
//
// The device that creates the network is its hub, as the access point is on a Switch: it gives every station an address on the network's /24,
// keeps the player list, applies the game's accept policy and relays all traffic. A station keeps one connection, to the hub.
// Sockets exist only while the device is on a network (created or joined): UDP and TCP to the network's addresses, broadcasts included.
//
// Portable C (no Apple API): the core is tested on any computer (tests/t_lan.c). Errors of the socket calls are FreeBSD errno values, as the Switch's.
#pragma once
#include <stddef.h>
#include <stdint.h>

// ---------- nn::ldn types (layouts as libnx's ldn.h; sizes checked in lan_core.c) ----------
#pragma pack(push, 1)
typedef struct { int64_t local_comm_id; uint8_t r0[2]; uint16_t scene_id; uint8_t r1[4]; } ldn_intent;                       // 0x10
typedef struct { ldn_intent intent; uint8_t session[16]; } ldn_network_id;                                                      // 0x20
typedef struct { uint8_t bssid[6]; uint8_t ssid_len; char ssid[0x21]; int16_t channel; int8_t link_level; uint8_t network_type; uint8_t r[4]; } ldn_common;   // 0x30
typedef struct { uint32_t ip; uint8_t mac[6]; int8_t node_id; uint8_t connected; char user_name[0x21]; uint8_t platform; int16_t lcv; uint8_t r[0x10]; } ldn_node;   // 0x40
typedef struct {
    ldn_network_id id; ldn_common common;
    uint8_t server_random[16]; uint16_t security_mode; uint8_t accept_policy; uint8_t version; uint8_t r0[2];
    int8_t node_count_max; uint8_t node_count; ldn_node nodes[8]; uint8_t r1[2]; uint16_t adv_size; uint8_t adv[0x180]; uint8_t r2[0x8C]; uint64_t auth_id;
} ldn_network_info;                                                                                                             // 0x480
typedef struct { char user_name[0x21]; uint8_t r[0xF]; } ldn_user_config;                                                       // 0x30
typedef struct { ldn_intent intent; int16_t channel; int8_t node_count_max; uint8_t r0; int16_t lcv; uint8_t r1[0xA]; } ldn_network_config;   // 0x20
typedef struct { uint16_t security_mode; uint16_t passphrase_size; uint8_t passphrase[0x40]; } ldn_security_config;             // 0x44
typedef struct { uint8_t server_random[16]; uint8_t session[16]; } ldn_security_param;                                          // 0x20
typedef struct { ldn_network_id id; uint32_t network_type; uint8_t bssid[6]; uint8_t ssid_len; char ssid[0x21]; uint8_t r[0x10]; uint32_t flags; } ldn_scan_filter;   // 0x60
typedef struct { uint32_t ip; uint8_t mac[6]; uint8_t r[2]; } ldn_address_entry;                                                // 0xC
typedef struct { uint8_t state_change; uint8_t r[7]; } ldn_node_update;                                                         // 0x8
#pragma pack(pop)

enum { LDN_NONE, LDN_INITIALIZED, LDN_AP, LDN_AP_CREATED, LDN_STATION, LDN_STATION_CONNECTED, LDN_ERROR };   // nn::ldn::State
enum { LDN_DR_NONE, LDN_DR_BY_USER, LDN_DR_BY_SYSTEM, LDN_DR_DESTROYED_BY_USER, LDN_DR_DESTROYED_BY_SYSTEM, LDN_DR_REJECTED, LDN_DR_SIGNAL_LOST };   // DisconnectReason
enum { LDN_SF_LOCAL_COMM_ID = 1, LDN_SF_SESSION_ID = 2, LDN_SF_NETWORK_TYPE = 4, LDN_SF_BSSID = 8, LDN_SF_SSID = 16, LDN_SF_SCENE_ID = 32 };   // ScanFilterFlag
enum { LDN_ACCEPT_ALWAYS, LDN_ACCEPT_NEVER, LDN_ACCEPT_BLACKLIST, LDN_ACCEPT_WHITELIST };

// nn::ldn results (module 203)
#define LDN_RES(d) ((uint32_t)(203 | (d) << 9))
#define LDN_OK 0u
#define LDN_DEVICE_NOT_AVAILABLE LDN_RES(16)
#define LDN_INVALID_STATE LDN_RES(32)
#define LDN_NODE_NOT_FOUND LDN_RES(48)
#define LDN_CONNECT_FAILURE LDN_RES(64)
#define LDN_CONNECT_NOT_FOUND LDN_RES(65)
#define LDN_CONNECT_TIMEOUT LDN_RES(66)
#define LDN_CONNECT_REJECTED LDN_RES(67)
#define LDN_INVALID_ARGUMENT LDN_RES(96)
#define LDN_VERSION_TOO_LOW LDN_RES(113)
#define LDN_VERSION_TOO_HIGH LDN_RES(114)
#define LDN_TOO_MANY_PLAYERS LDN_RES(144)

// FreeBSD errno values, the core's own (lan_hle.c gives the game nn::socket's numbering, which is Linux's)
enum { SK_EBADF = 9, SK_ENOMEM = 12, SK_EFAULT = 14, SK_EINVAL = 22, SK_EMFILE = 24, SK_EPIPE = 32, SK_EAGAIN = 35, SK_EINPROGRESS = 36, SK_EALREADY = 37,
       SK_ENOTSOCK = 38, SK_EDESTADDRREQ = 39, SK_EMSGSIZE = 40, SK_EPROTOTYPE = 41, SK_ENOPROTOOPT = 42, SK_EPROTONOSUPPORT = 43, SK_EOPNOTSUPP = 45,
       SK_EAFNOSUPPORT = 47, SK_EADDRINUSE = 48, SK_EADDRNOTAVAIL = 49, SK_ENETDOWN = 50, SK_ENETUNREACH = 51, SK_ECONNABORTED = 53, SK_ECONNRESET = 54,
       SK_ENOBUFS = 55, SK_EISCONN = 56, SK_ENOTCONN = 57, SK_ETIMEDOUT = 60, SK_ECONNREFUSED = 61, SK_EHOSTUNREACH = 65 };
// FreeBSD socket constants
enum { SK_AF_INET = 2, SK_SOCK_STREAM = 1, SK_SOCK_DGRAM = 2, SK_SOCK_NONBLOCK = 0x20000000, SK_SOCK_CLOEXEC = 0x10000000, SK_IPPROTO_TCP = 6, SK_IPPROTO_UDP = 17 };
enum { SK_MSG_PEEK = 0x2, SK_MSG_WAITALL = 0x40, SK_MSG_DONTWAIT = 0x80 };
enum { SK_POLLIN = 0x1, SK_POLLPRI = 0x2, SK_POLLOUT = 0x4, SK_POLLERR = 0x8, SK_POLLHUP = 0x10, SK_POLLNVAL = 0x20, SK_POLLRDNORM = 0x40, SK_POLLRDBAND = 0x80, SK_POLLWRBAND = 0x100 };
enum { SK_SOL_SOCKET = 0xffff, SK_SO_REUSEADDR = 0x4, SK_SO_KEEPALIVE = 0x8, SK_SO_BROADCAST = 0x20, SK_SO_LINGER = 0x80, SK_SO_REUSEPORT = 0x200,
       SK_SO_SNDBUF = 0x1001, SK_SO_RCVBUF = 0x1002, SK_SO_SNDTIMEO = 0x1005, SK_SO_RCVTIMEO = 0x1006, SK_SO_ERROR = 0x1007, SK_SO_TYPE = 0x1008 };
enum { SK_F_GETFD = 1, SK_F_SETFD = 2, SK_F_GETFL = 3, SK_F_SETFL = 4, SK_O_NONBLOCK = 0x4 };
#pragma pack(push, 1)
typedef struct { uint8_t len, family; uint16_t port_be; uint32_t addr_be; uint8_t zero[8]; } sk_sockaddr_in;   // FreeBSD sockaddr_in
typedef struct { int32_t fd; int16_t events, revents; } sk_pollfd;
#pragma pack(pop)

// ---------- the transport ----------
// Carries whole frames between devices. The core may call these while holding its lock: they must queue and return, and never call back into the
// core on the calling thread. Peers and services are the transport's handles (never 0).
typedef struct {
    void *self;
    void (*advertise)(void *self, int on);                    // be found as a network's hub (and accept connections), or stop
    void (*browse)(void *self, int on);                       // look for hubs: lan_on_service for each one found or lost
    uint64_t (*connect)(void *self, uint64_t service);        // open a connection to a hub found; 0 = cannot. lan_on_peer reports it ready or closed
    int (*send)(void *self, uint64_t peer, const void *frame, uint32_t len);   // 0 = queued
    void (*close)(void *self, uint64_t peer);                 // lan_on_peer(closed) follows
} lan_transport;

typedef struct lan_ctx lan_ctx;
typedef struct {
    uint64_t program_id;                                      // the game's: the default local communication ID
    void (*state_changed)(void *user);                        // the state, the network or its players changed (the game's state change event)
    void (*log)(void *user, int red, const char *msg);
    void *user;
} lan_config;

lan_ctx *lan_new(const lan_transport *t, const lan_config *cfg);
void lan_free(lan_ctx *c);

// transport → core, from any thread (one at a time for a peer)
void lan_on_service(lan_ctx *c, uint64_t service, int found);
void lan_on_peer(lan_ctx *c, uint64_t peer, int ready, uint64_t service);   // ready: 1 open, 0 closed; service: the hub dialled, 0 = it connected to us
void lan_on_frame(lan_ctx *c, uint64_t peer, const uint8_t *frame, uint32_t len);
#define LAN_FRAME_MAX (65536 + 64)

// ---------- nn::ldn ----------
uint32_t lan_ldn_initialize(lan_ctx *c);
void lan_ldn_finalize(lan_ctx *c);
int lan_ldn_state(lan_ctx *c);
uint32_t lan_ldn_open_ap(lan_ctx *c);
uint32_t lan_ldn_close_ap(lan_ctx *c);
uint32_t lan_ldn_create(lan_ctx *c, const ldn_network_config *cfg, const ldn_security_config *sec, const ldn_security_param *param, const ldn_user_config *user);
uint32_t lan_ldn_destroy(lan_ctx *c);
uint32_t lan_ldn_set_advertise(lan_ctx *c, const void *data, size_t size);
uint32_t lan_ldn_set_accept_policy(lan_ctx *c, int policy);
uint32_t lan_ldn_add_accept_filter(lan_ctx *c, const uint8_t mac[6]);
uint32_t lan_ldn_reject(lan_ctx *c, uint32_t ip);
uint32_t lan_ldn_open_station(lan_ctx *c);
uint32_t lan_ldn_close_station(lan_ctx *c);
uint32_t lan_ldn_scan(lan_ctx *c, ldn_network_info *out, int *count, int max, const ldn_scan_filter *filter);
uint32_t lan_ldn_connect(lan_ctx *c, const ldn_network_info *info, const ldn_network_config *cfg, const ldn_security_config *sec, const ldn_security_param *param,
                         const ldn_user_config *user, int lcv);   // info: Connect; cfg + param: ConnectPrivate
uint32_t lan_ldn_disconnect(lan_ctx *c);
uint32_t lan_ldn_network_info(lan_ctx *c, ldn_network_info *out, ldn_node_update *updates, int nupdates);
uint32_t lan_ldn_ipv4(lan_ctx *c, uint32_t *ip, uint32_t *mask);
uint32_t lan_ldn_security_param(lan_ctx *c, ldn_security_param *out);
uint32_t lan_ldn_network_config(lan_ctx *c, ldn_network_config *out);
int lan_ldn_disconnect_reason(lan_ctx *c);
int lan_active(lan_ctx *c);   // on a network (created or joined): sockets work

// ---------- what the overlay shows (lan_ui.m) ----------
#define LAN_ROOM_MAX 64
void lan_set_room_name(lan_ctx *c, const char *name);   // the hosted room's name (UTF-8; empty: the host's player name says it), sent to who looks or joins
typedef struct { char name[0x21]; uint64_t peer; int me, rtt_ms; } lan_status_player;   // peer: the transport's connection to it (0: none of ours); rtt -1: not known yet
typedef struct { char host[0x21]; char room[LAN_ROOM_MAX]; int count, max, rtt_ms, joined; uint64_t peer, service; } lan_status_room;
typedef struct {
    int state;                          // LDN_*
    char room[LAN_ROOM_MAX];            // hosting: ours; joined: the host's (empty: not named)
    int nplayers; lan_status_player players[8];   // on a network (created or joined)
    int nrooms; lan_status_room rooms[16];        // looking or joined: the rooms found (joined: the one we are in among them)
} lan_status;
// A snapshot; also measures the round trips to the devices it lists (a ping each, once a second at most: older devices do not answer, so theirs stays -1)
void lan_status_get(lan_ctx *c, lan_status *out);

// ---------- nn::socket on the network ----------
// Descriptors are lan_is_fd's; every call returns >= 0 or -errno. Timeouts in milliseconds, -1 = none
#define LAN_FD_BASE 8
#define LAN_FD_MAX 128
int lan_is_fd(lan_ctx *c, int fd);
int lan_sk_socket(lan_ctx *c, int domain, int type, int proto);
int lan_sk_bind(lan_ctx *c, int fd, const void *addr, uint32_t len);
int lan_sk_connect(lan_ctx *c, int fd, const void *addr, uint32_t len);
int lan_sk_listen(lan_ctx *c, int fd, int backlog);
int lan_sk_accept(lan_ctx *c, int fd, void *addr, uint32_t *len);
int lan_sk_close(lan_ctx *c, int fd);
int lan_sk_shutdown(lan_ctx *c, int fd, int how);
long lan_sk_sendto(lan_ctx *c, int fd, const void *buf, size_t n, int flags, const void *addr, uint32_t len);
long lan_sk_recvfrom(lan_ctx *c, int fd, void *buf, size_t n, int flags, void *addr, uint32_t *len);
int lan_sk_name(lan_ctx *c, int fd, int peer, void *addr, uint32_t *len);
int lan_sk_setsockopt(lan_ctx *c, int fd, int level, int opt, const void *val, uint32_t len);
int lan_sk_getsockopt(lan_ctx *c, int fd, int level, int opt, void *val, uint32_t *len);
int lan_sk_fcntl(lan_ctx *c, int fd, int cmd, int arg);
int lan_sk_poll(lan_ctx *c, sk_pollfd *fds, uint64_t n, int timeout_ms);
int lan_sk_select(lan_ctx *c, int nfds, uint64_t *r, uint64_t *w, uint64_t *e, int timeout_ms);
