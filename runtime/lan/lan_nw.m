// Local play's transport on Apple devices (lan_core.h's lan_transport), with Network.framework:
//   - a hub is a Bonjour service, found and reached over Wi-Fi and Apple's peer-to-peer Wi-Fi (no router needed: AWDL, as AirDrop). Its type is
//     "_presence._tcp", one of the types LiveContainer declares (iOS lets an app browse and advertise only the types in its Info.plist's NSBonjourServices,
//     and a game in LiveContainer runs as LiveContainer: with a type of our own every call failed with -65555, NoAuth). Other services of that type
//     are skipped: only a record with our tag is taken;
//   - every connection is TLS with a pre-shared key: HMAC-SHA256 over the game's program ID, keyed with "nxnative-lan-1:" and the players' password
//     (lan_ui.m's overlay, may be empty). Only devices running the same game with the same key complete the handshake, and what they send is encrypted;
//     without a password the key follows from the game alone, so it keeps out other games and casual listeners, not someone who reads this file;
//   - the service's TXT record carries only a tag derived from that key (no name, no game ID), so devices skip hubs they could not join;
//   - a hosted room listens on a fixed port (lan_nw_config.port, 49540 unless set; another free one if it is taken), and rooms can be reached by address
//     as well as found with Bonjour: Bonjour is multicast, which a VPN or another network does not carry. Those rooms are the friends of lan_nw_config,
//     each with the key its invite carried (the host's, lan_nw_key) or this device's own; while the game looks for rooms each is tried, and tried again
//     a few seconds after it fails or closes, and what it answered is kept for the overlay (lan_nw_friends);
//   - a device that connects is let in by the interface it comes through: Wi-Fi, peer-to-peer Wi-Fi and wired when nearby play is on, any other (a VPN's
//     tunnel) when sharing over VPN is on, cellular never;
//   - frames are a little-endian u32 length and the frame (at most LAN_FRAME_MAX); a longer one closes the connection, and so do more than 8 MB waiting to be sent.
// Everything runs on one serial queue; the core's calls only queue work on it, and the overlay's questions wait for it (never the other way round).
#import <Foundation/Foundation.h>
#import <Network/Network.h>
#import <Security/Security.h>
#import <Security/CipherSuite.h>
#import <CommonCrypto/CommonHMAC.h>
#include <arpa/inet.h>
#include <stdatomic.h>
#include "lan_core.h"
#include "lan_nw.h"

#define SERVICE "_presence._tcp"   // in LiveContainer's NSBonjourServices (and in the standalone app's)
#define SEND_MAX (8u << 20)
#define RETRY_S 3                  // a friend's room is tried again this long after it failed or closed

static dispatch_queue_t g_q;
static lan_ctx *_Atomic *g_slot;
static void (*g_log)(int red, const char *msg);
static uint8_t g_psk[LAN_KEY_LEN]; static char g_tag[13]; static char g_name[17];
static nw_listener_t g_listener; static nw_browser_t g_browser;
static int g_advertising, g_browsing;   // what the core asked for
static NSMutableDictionary<NSNumber *, nw_connection_t> *g_conns;   // peer handle → connection
static NSMutableDictionary<NSNumber *, NSNumber *> *g_pending;        // peer handle → bytes queued
static NSMutableDictionary<NSNumber *, NSArray *> *g_info;            // peer handle → @[kind, address]
static NSMutableDictionary<NSString *, NSNumber *> *g_svc_ids;        // Bonjour name → service handle
static NSMutableDictionary<NSNumber *, nw_endpoint_t> *g_svcs;        // service handle → endpoint
static NSMutableDictionary<NSNumber *, NSData *> *g_svc_keys;         // service handle → the key to reach it with (none: ours)
static NSMutableDictionary<NSString *, NSNumber *> *g_friend_ids;     // friend's address as given → service handle
static NSMutableDictionary<NSNumber *, NSString *> *g_friend_of;      // service handle → friend's address
static NSMutableDictionary<NSString *, NSNumber *> *g_friend_state;   // friend's address → LAN_FRIEND_*
static _Atomic uint64_t g_next = 1;
static lan_nw_config g_cfg;              // on the queue
static _Atomic int g_port_now;           // the port the hosted room listens on (0: not hosting)

static void logf_(int red, NSString *fmt, ...) NS_FORMAT_FUNCTION(2, 3);
static void logf_(int red, NSString *fmt, ...) {
    va_list ap; va_start(ap, fmt); NSString *s = [[NSString alloc] initWithFormat:fmt arguments:ap]; va_end(ap);
    if (g_log) g_log(red, s.UTF8String);
}
static lan_ctx *ctx(void) { return atomic_load(g_slot); }
static void denied(nw_error_t err) {   // Bonjour's "policy denied" (kDNSServiceErr_PolicyDenied): iOS refused Local Network access to this app
    if (err && nw_error_get_error_domain(err) == nw_error_domain_dns && nw_error_get_error_code(err) == -65570) lan_ui_blocked();
}

static nw_parameters_t params(const uint8_t key[LAN_KEY_LEN]) {
    NSData *k = [NSData dataWithBytes:key length:LAN_KEY_LEN];
    nw_parameters_configure_protocol_block_t tls = ^(nw_protocol_options_t o) {
        sec_protocol_options_t s = nw_tls_copy_sec_protocol_options(o);
        dispatch_data_t kd = dispatch_data_create(k.bytes, k.length, nil, DISPATCH_DATA_DESTRUCTOR_DEFAULT);
        dispatch_data_t ident = dispatch_data_create("nxnative-lan", 12, nil, DISPATCH_DATA_DESTRUCTOR_DEFAULT);
        sec_protocol_options_add_pre_shared_key(s, kd, ident);
        sec_protocol_options_append_tls_ciphersuite(s, (tls_ciphersuite_t)TLS_PSK_WITH_AES_128_GCM_SHA256);
        sec_protocol_options_set_min_tls_protocol_version(s, tls_protocol_version_TLSv12);
        sec_protocol_options_set_max_tls_protocol_version(s, tls_protocol_version_TLSv12);
    };
    nw_parameters_configure_protocol_block_t tcp = ^(nw_protocol_options_t o) {
        nw_tcp_options_set_no_delay(o, true);   // game packets go out at once
        nw_tcp_options_set_enable_keepalive(o, true); nw_tcp_options_set_keepalive_idle_time(o, 2); nw_tcp_options_set_keepalive_interval(o, 1); nw_tcp_options_set_keepalive_count(o, 4);
        nw_tcp_options_set_connection_timeout(o, 6);
    };
    nw_parameters_t p = nw_parameters_create_secure_tcp(tls, tcp);
    nw_parameters_set_include_peer_to_peer(p, true);
    return p;
}

// ---------- what a connection goes through ----------
static int kind_of(nw_path_t path) {   // the path's first interface, by name where iOS's own names say it (awdl, llw: peer-to-peer; pdp_ip: cellular)
    __block int kind = LAN_IF_UNKNOWN;
    if (!path) return kind;
    nw_path_enumerate_interfaces(path, ^bool(nw_interface_t i) {
        const char *n = nw_interface_get_name(i); nw_interface_type_t t = nw_interface_get_type(i);
        if (!n) n = "";
        if (!strncmp(n, "awdl", 4) || !strncmp(n, "llw", 3)) kind = LAN_IF_P2P;
        else if (t == nw_interface_type_loopback || !strncmp(n, "lo", 2)) kind = LAN_IF_LOCAL;
        else if (t == nw_interface_type_cellular || !strncmp(n, "pdp_ip", 6)) kind = LAN_IF_CELLULAR;
        else if (t == nw_interface_type_wifi) kind = LAN_IF_WIFI;
        else if (t == nw_interface_type_wired) kind = LAN_IF_WIRED;
        else kind = LAN_IF_VPN;   // utun, ipsec, ...: a tunnel
        return false;
    });
    return kind;
}
static NSString *address_of(nw_path_t path) {
    nw_endpoint_t ep = path ? nw_path_copy_effective_remote_endpoint(path) : nil;
    if (!ep) return @"";
    if (nw_endpoint_get_type(ep) == nw_endpoint_type_address) {
        const struct sockaddr *sa = nw_endpoint_get_address(ep); char b[INET6_ADDRSTRLEN] = "";
        if (sa && sa->sa_family == AF_INET) inet_ntop(AF_INET, &((const struct sockaddr_in *)sa)->sin_addr, b, sizeof b);
        else if (sa && sa->sa_family == AF_INET6) inet_ntop(AF_INET6, &((const struct sockaddr_in6 *)sa)->sin6_addr, b, sizeof b);
        return @(b);
    }
    if (nw_endpoint_get_type(ep) == nw_endpoint_type_host) return @(nw_endpoint_get_hostname(ep) ?: "");
    return @"";
}
static const char *kind_name(int k) {
    switch (k) { case LAN_IF_WIFI: return "Wi-Fi"; case LAN_IF_P2P: return "peer-to-peer Wi-Fi"; case LAN_IF_WIRED: return "wired"; case LAN_IF_VPN: return "VPN";
                 case LAN_IF_LOCAL: return "this device"; case LAN_IF_CELLULAR: return "cellular"; default: return "an unknown interface"; }
}
static int allowed(int kind) {   // a device that connected to us: let in by the interface it came through
    if (kind == LAN_IF_LOCAL) return 1;
    if (kind == LAN_IF_CELLULAR) return 0;
    if (kind == LAN_IF_VPN || kind == LAN_IF_UNKNOWN) return g_cfg.vpn;
    return g_cfg.nearby;
}

// ---------- friends: rooms by address ----------
static int friend_failure(nw_error_t err) {   // what a failed connection to a friend's room says
    if (!err) return LAN_FRIEND_NO_ANSWER;
    nw_error_domain_t d = nw_error_get_error_domain(err); int code = nw_error_get_error_code(err);
    if (d == nw_error_domain_tls) return LAN_FRIEND_KEY_DIFFERS;   // the handshake: another password or another game
    if (d == nw_error_domain_dns) return LAN_FRIEND_BAD_ADDRESS;   // the name does not resolve
    if (d == nw_error_domain_posix) {
        if (code == ECONNREFUSED) return LAN_FRIEND_NOT_HOSTING;   // the device answers, nothing listens: no room open
        if (code == ECONNRESET || code == ECONNABORTED) return LAN_FRIEND_KEY_DIFFERS;   // our listener drops a handshake it cannot complete
    }
    return LAN_FRIEND_NO_ANSWER;
}
static void friend_set(uint64_t service, int state) {
    NSString *a = g_friend_of[@(service)];
    if (a) g_friend_state[a] = @(state);
}
static void friend_retry(uint64_t service) {   // on the queue: the friend's room, again in a few seconds while the game still looks
    if (!g_friend_of[@(service)]) return;
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, RETRY_S * NSEC_PER_SEC), g_q, ^{
        if (!g_browsing || !g_svcs[@(service)] || !g_friend_of[@(service)]) return;
        int st = g_friend_state[g_friend_of[@(service)]].intValue;
        if (st == LAN_FRIEND_HOSTING) return;   // connected again meanwhile
        lan_on_service(ctx(), service, 1);       // the core dials it again (its last connection is gone)
    });
}

// ---------- connections ----------
static void conn_gone(uint64_t h) {   // once per connection: the core hears it closed
    NSNumber *k = @(h);
    if (!g_conns[k]) return;
    [g_conns removeObjectForKey:k]; [g_pending removeObjectForKey:k]; [g_info removeObjectForKey:k];
    lan_on_peer(ctx(), h, 0, 0);
}
static void receive(nw_connection_t c, uint64_t h) {
    nw_connection_receive(c, 4, 4, ^(dispatch_data_t head, nw_content_context_t cx, bool done, nw_error_t err) {
        (void)cx;
        if (err || !head || dispatch_data_get_size(head) != 4) { if (!err && done) logf_(0, @"lan: a device closed its connection"); nw_connection_cancel(c); return; }
        __block uint32_t len = 0; __block unsigned got = 0;   // little endian, over however many pieces the data comes in
        dispatch_data_apply(head, ^bool(dispatch_data_t r, size_t off, const void *p, size_t n) {
            (void)r; (void)off; for (size_t i = 0; i < n && got < 4; i++) len |= (uint32_t)((const uint8_t *)p)[i] << (8 * got++); return true;
        });
        if (!len || len > LAN_FRAME_MAX) { logf_(1, @"lan: a device sent a frame of %u bytes; connection closed", len); nw_connection_cancel(c); return; }
        nw_connection_receive(c, len, len, ^(dispatch_data_t body, nw_content_context_t cx2, bool done2, nw_error_t err2) {
            (void)cx2; (void)done2;
            if (err2 || !body || dispatch_data_get_size(body) != len) { nw_connection_cancel(c); return; }
            const void *p = NULL; size_t n = 0;
            dispatch_data_t flat = dispatch_data_create_map(body, &p, &n);
            if (g_conns[@(h)]) lan_on_frame(ctx(), h, p, (uint32_t)n);
            (void)flat;
            receive(c, h);
        });
    });
}
static void start(nw_connection_t c, uint64_t h, uint64_t service) {   // on the queue
    g_conns[@(h)] = c;
    nw_connection_set_queue(c, g_q);
    __block int was_ready = 0;
    nw_connection_set_state_changed_handler(c, ^(nw_connection_state_t st, nw_error_t err) {
        if (st == nw_connection_state_ready) {
            if (!g_conns[@(h)]) return;
            nw_path_t path = nw_connection_copy_current_path(c);
            int kind = kind_of(path); NSString *addr = address_of(path);
            g_info[@(h)] = @[@(kind), addr];
            if (!service && !allowed(kind)) {   // a device that connected to us, through an interface the room is not shared on
                logf_(0, @"lan: a device at %@ came through %s, which this room is not shared on; refused", addr, kind_name(kind));
                nw_connection_cancel(c); return;
            }
            was_ready = 1;
            if (service) friend_set(service, LAN_FRIEND_HOSTING);
            lan_on_peer(ctx(), h, 1, service); receive(c, h);
        } else if (st == nw_connection_state_waiting || st == nw_connection_state_failed) {
            int code = err ? nw_error_get_error_code(err) : 0;
            if (service && g_friend_of[@(service)]) {
                int why = friend_failure(err); friend_set(service, why);
                logf_(0, @"lan: the room at %@ %s (%d)", g_friend_of[@(service)], why == LAN_FRIEND_NOT_HOSTING ? "is not open" : why == LAN_FRIEND_KEY_DIFFERS ? "has another password or game"
                      : why == LAN_FRIEND_BAD_ADDRESS ? "is not a known address" : "does not answer", code);
            } else if (st == nw_connection_state_waiting) logf_(0, @"lan: a device cannot be reached (%d); giving up", code);
            else logf_(1, @"lan: a connection failed (%d): a different game or password?", code);
            nw_connection_cancel(c);
            if (st == nw_connection_state_failed) conn_gone(h);
        } else if (st == nw_connection_state_cancelled) {
            if (service && was_ready) friend_set(service, LAN_FRIEND_TRYING);
            conn_gone(h);
            if (service) friend_retry(service);
        }
    });
    nw_connection_start(c);
}

// ---------- the transport ----------
static void listen_on(int port);
static void listen_off(void) { if (g_listener) { nw_listener_cancel(g_listener); g_listener = nil; atomic_store(&g_port_now, 0); } }
static void t_advertise(void *self, int on) {
    (void)self;
    dispatch_async(g_q, ^{
        g_advertising = on;
        if (!on) { if (g_listener) { listen_off(); logf_(0, @"lan: the room is closed to other devices"); } return; }
        if (!g_listener) listen_on(g_cfg.port);
    });
}
// On the queue: a listener on that port (0: any), announced with Bonjour when nearby play is on; a fixed port that is taken falls back to any
static void listen_on(int port) {
    if (!g_cfg.nearby && !g_cfg.vpn) { logf_(0, @"lan: the room is shared on nothing (nearby and VPN off): no other device can join"); return; }
    nw_listener_t l = nil;
    if (port > 0 && port < 65536) { char ps[8]; snprintf(ps, sizeof ps, "%d", port); l = nw_listener_create_with_port(ps, params(g_psk)); }
    if (!l) { l = nw_listener_create(params(g_psk)); port = 0; }
    if (!l) { logf_(1, @"lan: cannot listen for players"); return; }
    if (g_cfg.nearby) {
        uint8_t r[8]; arc4random_buf(r, sizeof r); for (int i = 0; i < 8; i++) snprintf(g_name + 2 * i, 3, "%02x", r[i]);
        nw_advertise_descriptor_t ad = nw_advertise_descriptor_create_bonjour_service(g_name, SERVICE, NULL);
        nw_txt_record_t txt = nw_txt_record_create_dictionary();
        nw_txt_record_set_key(txt, "t", (const uint8_t *)g_tag, strlen(g_tag));
        nw_advertise_descriptor_set_txt_record_object(ad, txt);
        nw_listener_set_advertise_descriptor(l, ad);
    }
    nw_listener_set_queue(l, g_q);
    nw_listener_set_state_changed_handler(l, ^(nw_listener_state_t st, nw_error_t err) {
        if (st == nw_listener_state_ready) {
            atomic_store(&g_port_now, (int)nw_listener_get_port(l));
            logf_(0, @"lan: the room listens on port %d, shared %s", nw_listener_get_port(l), g_cfg.nearby && g_cfg.vpn ? "nearby and over VPN" : g_cfg.nearby ? "nearby (not over VPN)" : "over VPN only (not nearby)");
        } else if (st == nw_listener_state_failed || st == nw_listener_state_waiting) {
            int code = err ? nw_error_get_error_code(err) : 0;
            if (port && l == g_listener && err && nw_error_get_error_domain(err) == nw_error_domain_posix && code == EADDRINUSE) {
                logf_(0, @"lan: port %d is taken; the room listens on another one", port);
                listen_off(); listen_on(0); return;
            }
            logf_(1, @"lan: cannot be found by other devices (%d): is Local Network allowed for this app in Settings › Privacy?", code); denied(err);
        }
    });
    nw_listener_set_new_connection_handler(l, ^(nw_connection_t c) { start(c, atomic_fetch_add(&g_next, 1), 0); });
    g_listener = l;
    nw_listener_start(l);
}
static int tag_matches(nw_txt_record_t txt) {
    if (!txt) return 0;
    __block int ok = 0;
    nw_txt_record_access_key(txt, "t", ^bool(const char *key, nw_txt_record_find_key_t found, const uint8_t *v, size_t n) {
        (void)key; ok = found == nw_txt_record_find_key_non_empty_value && n == strlen(g_tag) && !memcmp(v, g_tag, n); return true;
    });
    return ok;
}
static void service_found(nw_browse_result_t r) {
    nw_endpoint_t ep = nw_browse_result_copy_endpoint(r);
    const char *name = ep ? nw_endpoint_get_bonjour_service_name(ep) : NULL;
    if (!name || !strcmp(name, g_name)) return;
    NSString *key = @(name);
    if (!tag_matches(nw_browse_result_copy_txt_record_object(r))) return;
    NSNumber *id = g_svc_ids[key];
    if (!id) { id = @(atomic_fetch_add(&g_next, 1)); g_svc_ids[key] = id; }
    g_svcs[id] = ep;
    lan_on_service(ctx(), id.unsignedLongLongValue, 1);
}
static void service_lost(nw_browse_result_t r) {
    nw_endpoint_t ep = nw_browse_result_copy_endpoint(r);
    const char *name = ep ? nw_endpoint_get_bonjour_service_name(ep) : NULL;
    NSNumber *id = name ? g_svc_ids[@(name)] : nil;
    if (!id) return;
    [g_svc_ids removeObjectForKey:@(name)]; [g_svcs removeObjectForKey:id];
    lan_on_service(ctx(), id.unsignedLongLongValue, 0);
}
// "host", "host:port", "[v6]:port" or a bare v6 address → host and port; NO when it is not an address
static BOOL split_address(NSString *e, NSString **host, int *port) {
    NSString *h = e, *ps = nil;
    if ([e hasPrefix:@"["]) {
        NSRange close = [e rangeOfString:@"]"]; if (close.location == NSNotFound) return NO;
        h = [e substringWithRange:NSMakeRange(1, close.location - 1)];
        if (close.location + 2 < e.length && [e characterAtIndex:close.location + 1] == ':') ps = [e substringFromIndex:close.location + 2];
    } else if ([e componentsSeparatedByString:@":"].count == 2) {   // host:port (a bare v6 address has more colons)
        NSArray *hp = [e componentsSeparatedByString:@":"]; h = hp[0]; ps = hp[1];
    }
    int pn = ps ? ps.intValue : LAN_DEFAULT_PORT;
    if (!h.length || pn <= 0 || pn > 65535 || [h rangeOfCharacterFromSet:NSCharacterSet.whitespaceCharacterSet].location != NSNotFound) return NO;
    *host = h; *port = pn; return YES;
}
// On the queue: the friends' rooms, each a service of its own; the core asks each for its network as it does a Bonjour one
static void friends_on(void) {
    if (!g_cfg.friends) return;
    for (int i = 0; i < g_cfg.nfriends && i < LAN_FRIENDS_MAX; i++) {
        const lan_nw_friend *f = &g_cfg.friend_list[i];
        NSString *a = [@(f->addr) stringByTrimmingCharactersInSet:NSCharacterSet.whitespaceCharacterSet];
        if (!a.length || g_friend_ids[a]) continue;
        NSString *host; int port;
        if (!split_address(a, &host, &port)) { g_friend_state[a] = @(LAN_FRIEND_BAD_ADDRESS); logf_(1, @"lan: \"%@\" is not an address (host or host:port)", a); continue; }
        NSNumber *id = @(atomic_fetch_add(&g_next, 1));
        g_friend_ids[a] = id; g_friend_of[id] = a; g_friend_state[a] = @(LAN_FRIEND_TRYING);
        g_svcs[id] = nw_endpoint_create_host(host.UTF8String, [NSString stringWithFormat:@"%d", port].UTF8String);
        if (f->has_key) g_svc_keys[id] = [NSData dataWithBytes:f->key length:LAN_KEY_LEN];
        lan_on_service(ctx(), id.unsignedLongLongValue, 1);
    }
    if (g_friend_ids.count) logf_(0, @"lan: looking for %lu friend%s' rooms by address", (unsigned long)g_friend_ids.count, g_friend_ids.count == 1 ? "" : "s");
}
static void friends_off(void) {
    for (NSNumber *id in g_friend_of.allKeys) { [g_svcs removeObjectForKey:id]; [g_svc_keys removeObjectForKey:id]; lan_on_service(ctx(), id.unsignedLongLongValue, 0); }
    [g_friend_ids removeAllObjects]; [g_friend_of removeAllObjects]; [g_friend_state removeAllObjects];   // not looking: no state (LAN_FRIEND_IDLE)
}
static void nearby_on(void) {
    if (!g_cfg.nearby || g_browser) return;
    nw_browse_descriptor_t d = nw_browse_descriptor_create_bonjour_service(SERVICE, NULL);
    nw_browse_descriptor_set_include_txt_record(d, true);
    nw_parameters_t p = nw_parameters_create(); nw_parameters_set_include_peer_to_peer(p, true);
    nw_browser_t b = nw_browser_create(d, p);
    nw_browser_set_queue(b, g_q);
    nw_browser_set_state_changed_handler(b, ^(nw_browser_state_t st, nw_error_t err) {
        if (st == nw_browser_state_failed || st == nw_browser_state_waiting)
        { logf_(1, @"lan: cannot look for nearby devices (%d): is Local Network allowed for this app in Settings › Privacy?", err ? nw_error_get_error_code(err) : 0); denied(err); }
    });
    nw_browser_set_browse_results_changed_handler(b, ^(nw_browse_result_t old, nw_browse_result_t now, bool complete) {
        (void)complete;
        nw_browse_result_change_t ch = nw_browse_result_get_changes(old, now);
        if (ch & nw_browse_result_change_result_removed) service_lost(old);
        else if (ch & (nw_browse_result_change_result_added | nw_browse_result_change_txt_record_changed)) service_found(now);
    });
    g_browser = b;
    nw_browser_start(b);
    logf_(0, @"lan: looking for nearby devices");
}
static void nearby_off(void) {
    if (g_browser) { nw_browser_cancel(g_browser); g_browser = nil; }
    for (NSNumber *id in g_svc_ids.allValues) { [g_svcs removeObjectForKey:id]; lan_on_service(ctx(), id.unsignedLongLongValue, 0); }
    [g_svc_ids removeAllObjects];
}
static void t_browse(void *self, int on) {
    (void)self;
    dispatch_async(g_q, ^{
        if (on == g_browsing) return;
        g_browsing = on;
        if (!on) { nearby_off(); friends_off(); return; }
        friends_on(); nearby_on();
        if (!g_cfg.nearby && !g_cfg.friends) logf_(0, @"lan: looking on nothing (nearby and friends off): no room can be found");
    });
}
static uint64_t t_connect(void *self, uint64_t service) {
    (void)self;
    uint64_t h = atomic_fetch_add(&g_next, 1);
    dispatch_async(g_q, ^{
        nw_endpoint_t ep = g_svcs[@(service)];
        if (!ep) { lan_on_peer(ctx(), h, 0, 0); return; }   // the hub went away meanwhile
        NSData *k = g_svc_keys[@(service)];
        start(nw_connection_create(ep, params(k ? k.bytes : g_psk)), h, service);
    });
    return h;
}
static int t_send(void *self, uint64_t peer, const void *frame, uint32_t len) {
    (void)self;
    uint8_t *b = malloc((size_t)len + 4); if (!b) return -1;
    b[0] = (uint8_t)len; b[1] = (uint8_t)(len >> 8); b[2] = (uint8_t)(len >> 16); b[3] = (uint8_t)(len >> 24); memcpy(b + 4, frame, len);
    dispatch_data_t d = dispatch_data_create(b, (size_t)len + 4, nil, DISPATCH_DATA_DESTRUCTOR_FREE);
    dispatch_async(g_q, ^{
        nw_connection_t c = g_conns[@(peer)];
        if (!c) return;
        uint64_t pend = g_pending[@(peer)].unsignedLongLongValue + len + 4;
        if (pend > SEND_MAX) { logf_(1, @"lan: a device does not keep up (%llu bytes waiting); connection closed", (unsigned long long)pend); nw_connection_cancel(c); return; }
        g_pending[@(peer)] = @(pend);
        nw_connection_send(c, d, NW_CONNECTION_DEFAULT_MESSAGE_CONTEXT, true, ^(nw_error_t err) {
            (void)err; NSNumber *k = @(peer); if (g_pending[k]) g_pending[k] = @(g_pending[k].unsignedLongLongValue - (len + 4));
        });
    });
    return 0;
}
static void t_close(void *self, uint64_t peer) {
    (void)self;
    dispatch_async(g_q, ^{ nw_connection_t c = g_conns[@(peer)]; if (c) nw_connection_cancel(c); });
}

static uint64_t g_program;
static void derive(const char *password) {   // the key: the game and the password; the tag: a digest of the key, so the record tells nothing about either
    NSString *k = [@"nxnative-lan-1:" stringByAppendingString:@(password ?: "")];
    uint8_t pid[8]; for (int i = 0; i < 8; i++) pid[i] = (uint8_t)(g_program >> (8 * i));
    CCHmac(kCCHmacAlgSHA256, k.UTF8String, strlen(k.UTF8String), pid, sizeof pid, g_psk);
    uint8_t tag[32]; CCHmac(kCCHmacAlgSHA256, g_psk, sizeof g_psk, "tag", 3, tag);
    for (int i = 0; i < 6; i++) snprintf(g_tag + 2 * i, 3, "%02x", tag[i]);
}
const lan_transport *lan_nw_new(lan_ctx *_Atomic *slot, uint64_t program_id, const lan_nw_config *cfg, void (*log)(int red, const char *msg)) {
    static lan_transport t;
    @autoreleasepool {   // called on a game thread, which has no pool of its own
    g_slot = slot; g_log = log; g_program = program_id; g_cfg = *cfg;
    g_conns = [NSMutableDictionary new]; g_pending = [NSMutableDictionary new]; g_info = [NSMutableDictionary new];
    g_svc_ids = [NSMutableDictionary new]; g_svcs = [NSMutableDictionary new]; g_svc_keys = [NSMutableDictionary new];
    g_friend_ids = [NSMutableDictionary new]; g_friend_of = [NSMutableDictionary new]; g_friend_state = [NSMutableDictionary new];
    derive(g_cfg.password);
    g_q = dispatch_queue_create("dm.lan", DISPATCH_QUEUE_SERIAL);
    t = (lan_transport){NULL, t_advertise, t_browse, t_connect, t_send, t_close};
    }
    return &t;
}
int lan_nw_port(void) { return atomic_load(&g_port_now); }
void lan_nw_key(uint8_t out[LAN_KEY_LEN]) {
    if (!g_q) { memset(out, 0, LAN_KEY_LEN); return; }
    dispatch_sync(g_q, ^{ memcpy(out, g_psk, LAN_KEY_LEN); });
}
int lan_nw_peer_info(uint64_t peer, lan_nw_peer *out) {
    memset(out, 0, sizeof *out);
    if (!g_q || !peer) return 0;
    __block int ok = 0;
    dispatch_sync(g_q, ^{
        NSArray *i = g_info[@(peer)]; if (!i) return;
        out->kind = [i[0] intValue]; snprintf(out->addr, sizeof out->addr, "%s", [i[1] UTF8String] ?: ""); ok = 1;
    });
    return ok;
}
int lan_nw_friends(lan_nw_friend_state *out, int max) {
    if (!g_q) return 0;
    __block int n = 0;
    dispatch_sync(g_q, ^{
        for (NSString *a in g_friend_state) {
            if (n >= max) break;
            snprintf(out[n].addr, sizeof out[n].addr, "%s", a.UTF8String ?: "");
            out[n].state = g_friend_state[a].intValue; out[n].service = g_friend_ids[a].unsignedLongLongValue; n++;
        }
    });
    return n;
}
static int same_friends(const lan_nw_config *a, const lan_nw_config *b) {
    if (a->nfriends != b->nfriends || a->friends != b->friends) return 0;
    for (int i = 0; i < a->nfriends && i < LAN_FRIENDS_MAX; i++) {
        const lan_nw_friend *x = &a->friend_list[i], *y = &b->friend_list[i];
        if (strcmp(x->addr, y->addr) || x->has_key != y->has_key || (x->has_key && memcmp(x->key, y->key, LAN_KEY_LEN))) return 0;
    }
    return 1;
}
// New settings: only what they change is announced or looked for again (a new password is a new key: the room is announced again and the rooms found
// with the old one are lost, and found again if they changed too; a new port or sharing reopens the room; new friends are tried); connections made before keep theirs
void lan_nw_configure(const lan_nw_config *cfg) {
    if (!g_q) return;
    lan_nw_config *c = malloc(sizeof *c); if (!c) return;
    *c = *cfg;
    dispatch_async(g_q, ^{
        lan_nw_config old = g_cfg; g_cfg = *c; free(c);
        int key = strcmp(old.password, g_cfg.password) != 0;
        if (key) derive(g_cfg.password);
        // VPN sharing alone needs no new listener (each connection is checked as it comes), unless the room is shared on nothing now, or was
        int want = g_cfg.nearby || g_cfg.vpn;
        int reopen = g_advertising && (key || old.port != g_cfg.port || old.nearby != g_cfg.nearby || !g_listener != !want);
        int nearby = g_browsing && (key || old.nearby != g_cfg.nearby);
        int friends = g_browsing && (key || !same_friends(&old, &g_cfg));
        logf_(0, @"lan: settings changed (port %d, nearby %s, VPN %s, friends %s, %d friend%s)%s", g_cfg.port, g_cfg.nearby ? "on" : "off", g_cfg.vpn ? "on" : "off",
              g_cfg.friends ? "on" : "off", g_cfg.nfriends, g_cfg.nfriends == 1 ? "" : "s", reopen || nearby || friends ? "; announcing and looking again" : "");
        if (reopen) { listen_off(); if (want) listen_on(g_cfg.port); }
        if (nearby) { nearby_off(); nearby_on(); }
        if (friends) { friends_off(); friends_on(); }
    });
}
