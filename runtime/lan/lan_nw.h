// Local play's transport settings (lan_nw.m) and the overlay that edits them (lan_ui.m; on a platform without one, environment variables).
#pragma once
#include <stdint.h>
#include "lan_core.h"

#define LAN_DEFAULT_PORT 49540
#define LAN_FRIENDS_MAX 16
#define LAN_KEY_LEN 32

typedef struct {
    char addr[256];               // "host", "host:port" or "[v6]:port"
    uint8_t key[LAN_KEY_LEN];     // the room key from an invite (lan_nw_key on the host): its rooms are reached with it, whatever this device's password
    int has_key;                  // 0: this device's own key
} lan_nw_friend;

typedef struct {
    char password[256];   // the key's password: the same on every device that plays together (may be empty)
    int port;             // the port a hosted room listens on: a fixed one, so others can type the address (another free port if it is taken)
    int nearby;           // find and be found with Bonjour, and let devices in over Wi-Fi and peer-to-peer Wi-Fi
    int vpn;              // let devices in over a VPN (any interface that is not Wi-Fi, peer-to-peer, wired or cellular; cellular never)
    int friends;          // look for the rooms of the friends below while the game looks for rooms
    int nfriends; lan_nw_friend friend_list[LAN_FRIENDS_MAX];   // rooms reached by address, not by Bonjour (a VPN, another network)
    char room[LAN_ROOM_MAX];   // the hosted room's name (lan_set_room_name)
} lan_nw_config;

const lan_transport *lan_nw_new(lan_ctx *_Atomic *slot, uint64_t program_id, const lan_nw_config *cfg, void (*log)(int red, const char *msg));
void lan_nw_configure(const lan_nw_config *cfg);   // applies at once: only what changed is announced or looked for again (connections made before stay)
int lan_nw_port(void);                              // the port a hosted room listens on now; 0 when not hosting
void lan_nw_key(uint8_t out[LAN_KEY_LEN]);          // this device's room key now (the game and the password): what an invite carries

enum { LAN_IF_UNKNOWN, LAN_IF_WIFI, LAN_IF_P2P, LAN_IF_WIRED, LAN_IF_VPN, LAN_IF_LOCAL, LAN_IF_CELLULAR };   // how a connection reaches the other device
typedef struct { int kind; char addr[64]; } lan_nw_peer;
int lan_nw_peer_info(uint64_t peer, lan_nw_peer *out);   // 1 when the connection is known; addr: the other device's address ("" through Bonjour before it resolves)

enum { LAN_FRIEND_IDLE, LAN_FRIEND_TRYING, LAN_FRIEND_HOSTING, LAN_FRIEND_NOT_HOSTING, LAN_FRIEND_NO_ANSWER, LAN_FRIEND_KEY_DIFFERS, LAN_FRIEND_BAD_ADDRESS };
typedef struct { char addr[256]; int state; uint64_t service; } lan_nw_friend_state;
int lan_nw_friends(lan_nw_friend_state *out, int max);   // what each friend's address answered last (IDLE: not looking now)

// lan_ui.m
void lan_ui_config(lan_nw_config *out);   // the settings kept for this app (Documents' UserDefaults; elsewhere DM_LAN_PASSWORD, DM_LAN_PORT, DM_LAN_NEARBY, DM_LAN_VPN, DM_LAN_PEERS)
void lan_ui_state(int state);             // the game's local wireless state: the overlay shows while it hosts or looks
void lan_ui_blocked(void);                // iOS denied Local Network access
// lan_hle.c
lan_ctx *lan_current(void);               // local play's context; NULL until the game first uses it
