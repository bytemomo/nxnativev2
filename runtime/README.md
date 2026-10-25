# runtime

This is nxnativev2's own code: what runs inside the game apps, and the build-side pieces that go with it. It's kept here as plain source, not as a patch. The bundle step (`cargo xtask bundle`) copies this folder into nxnative as `dm/rt/`. The common patch adds `dm/rt/*/*.c` and `*.m` to the host build, so `tests/` stays out of it.

The glue in the patch is small:
- The loader (`loader.c`) takes the runtime's takeovers (`__DATA,swforcert`) before the host's own.
- The Makefile builds `dm/rt` and links Network and Security.
- The app's Info.plist gets the Local Network keys.
- The launcher gets text settings and `imports:` conditions.

For development, `target/nxnative-port/dm/rt` is a link to this folder, and it is excluded from git there.

## lan: local play

This is the Switch's local wireless (`nn::ldn`), plus the sockets games open on it (`nn::socket`), between nearby iPhones and iPads.

| File | Role |
|---|---|
| `lan_core.c` / `.h` | The network: nn::ldn's states, the hub, the stations, and sockets on the network. Portable C with no Apple API. |
| `lan_hle.c` | The takeovers: every nn::ldn and nn::socket entry point of the SDKs (SDK 4 forms, later forms, the C interface) calls the core. |
| `lan_nw.m` / `lan_nw.h` | The transport on Apple devices: Network.framework, Bonjour, peer-to-peer Wi-Fi and TLS; a fixed port, sharing by interface, and friends' rooms reached by address with the key of their invite. |
| `lan_ui.m` | The overlay over the game (iOS): the capsule with local play's state, and its panel (the room and its players, the rooms found, friends, invites, settings). |
| `lan_diag.c` / `.h` | Diagnostics for debug builds (`DM_LAN_DEBUG=1`): every call with its time and thread, Pia's jobs and results read from the game's heap, and where the game's threads are. |
| `tests/` | The core with several devices in one process: `tests/run.sh` (any computer; AddressSanitizer and UBSan). `tests/lan_peer.py <title ID>`: this computer as the other device, to test with one iPhone or iPad (needs `zeroconf`; Python 3.13+); `--address host[:port]` or `--invite "<text>"` asks one room directly. |

**How it works**
- **Hub and stations.** The device that creates the network is its hub, as the access point is on a Switch. It advertises the network, gives each player an address on `169.254.x.0/24` (the hub is `.1`), keeps the player list and applies the game's accept policy. It relays everything: each station keeps a single connection to it. If the hub leaves, the network ends for everyone.
- **Sockets.** They exist only while the device is on a network.
  - UDP goes to an address or to the network's broadcast. A broadcast comes back to its sender too, as on a Switch.
  - TCP is carried as open, data and end frames through the hub. A connection with more than 4 MB unread is reset.
- **What games expect.**
  - `nn::socket` numbers its errors as Linux does (`EAGAIN` is 11) and so are its `fcntl` flags (`O_NONBLOCK` is 0x800); the core works in FreeBSD's numbers and `lan_hle.c` converts. Levels, options, message flags and poll events stay BSD's.
  - Nintendo's Pia library (most first-party games' local play) asks `nn::friends` for a play history key for each local player before it creates a session. The host makes one from the user (`hle_friends.c`, in the patch), as a Switch does without any network.
- **Off.** When local play is off (`DM_LAN=0`), every call answers as the host's offline stubs do.

**Security**
- **Encryption.** Every connection is TLS with a pre-shared key: HMAC-SHA256 over the game's program ID, keyed with the players' password (set on the overlay).
- **Who can join.**
  - With a password, only devices running the same game with the same password complete the handshake.
  - Without one, any nearby device with the same game can join, as on a Switch. The key then follows from the game alone, so it keeps out other games and casual listeners, not a determined one.
- **What's visible.** The Bonjour record carries only a tag derived from the key: no names and no game ID.
- **Checking what arrives.** Every frame is checked before the game sees it:
  - sizes are bounded;
  - the hub accepts a station's traffic only from the address it gave that station;
  - network info is validated;
  - the game's own passphrase and version must match to join.
- **Limits.** A connection that sends a frame over 64 KB, or falls 8 MB behind, is closed.
- **Interfaces.** A device that connects is let in by the interface it comes through: Wi-Fi, peer-to-peer Wi-Fi and wired when **Nearby** is on, any
  other (a VPN's tunnel) when **VPN** is on, cellular never.
- **Invites** carry the room's key, not the password: whoever has an invite can join that room (until the host changes the password), so it is sent
  only to friends.

**Settings** (launcher rows, shown for games that link `nn::ldn::Initialize`):
- `DM_LAN` (Local play, on by default)

**Debugging.** `nxnative build <game> --debug` builds an IPA whose `dm/env.txt` sets `DM_LAN_DEBUG=1` (a line in the app's `Documents/env.txt` does the same without a rebuild). `nxnative diag <game>` then brings back a log with:
- every `nn::ldn` and `nn::socket` call, with its time and thread, and the first packets each way;
- when the game ends its network or shows an error: each Pia job's step and result, Pia's failed results with their codes, and where each of the game's threads is in `main` (IDA offsets);
- for Mario Kart 8 Deluxe 3.0.5 only (matched by build ID): its Pia session, mesh and statistics, and a watch thread that logs the code that fails a session as it happens.

**The overlay.** While the game hosts or looks for rooms, a capsule at the top of the screen says how it goes: the room and its players
("Alex's room · 3 players"), the rooms found ("Searching · 1 room found"), the room joined and the round trip to its host ("In Marco's room ·
24 ms"), a friend's room that does not answer, or, in red, that iOS denied Local Network access (tapped, it opens Settings). It fades after a few
seconds. Tapped, it opens a panel; what is set there is kept for the app and applied at once.
- **Hosting**:
  - the room: its name (**Rename**; empty, the host's player name says it) and its players, each with how it is connected (Nearby or VPN), its
    address and its round trip;
  - **Share on**: **Nearby** (Wi-Fi and peer-to-peer; Bonjour) and **VPN**, with this device's VPN addresses, each with **Share**: an invite, as text
    to send with any app;
  - the password and the port (49540 by default; another free one when it is taken).
- **Looking**:
  - the rooms found, with how each is reached and its round trip (the game lists them too, and joins them itself), or the players of the room joined;
  - **Look on**: **Nearby** and **Friends by address**;
  - the friends by address, each with what it answered: hosting, not hosting (the device answers, no room is open), no answer, other password or
    game (the handshake failed), not an address. A friend gets the host's name once one of their rooms has been seen;
  - **Paste** (Apple's paste button: iOS asks nothing) adds the friends in a copied invite; an address can be typed in too;
  - the password.

**Over a VPN or another network.** Bonjour finds rooms by multicast, which a VPN or another network does not carry. The host taps **Share** on its
VPN address: the invite is text with `nxlan:<host>:<port>/<key>` in it, the key being the room's (made from the game and the password, not the
password itself). The friend copies it, opens the game's local play, and taps **Paste** in the panel: from then on, whenever their game looks for
rooms, the address is tried with that key (again every few seconds while it fails), and the room appears in the game's list as one found nearby
does. A friend can also be added by address alone; their own password's key is used then. `tests/lan_peer.py <title ID> --invite "<text>"` asks the
room of an invite from a computer on the same VPN. Pia's local play expects local-network latency: over the internet it works at the latency the
VPN gives.

On a platform without the overlay (the Mac host) the settings are `DM_LAN_PASSWORD`, `DM_LAN_PORT`, `DM_LAN_ROOM`, `DM_LAN_PEERS` (friends by
address, comma-separated), `DM_LAN_NEARBY=0`, `DM_LAN_VPN=0` and `DM_LAN_FRIENDS=0`. On iOS they are the defaults until set in the panel.

The first time a game hosts or looks for a game, iOS asks to allow Local Network access.

## shaders: the shader caches and the IPA's shader pack

| File | Role |
|---|---|
| `dmsh_store.py` | A game's shader caches in one SQLite file (`shader/cache/shaders.db`): zstd blobs addressed by content, named entries with the build that last used them, pruned after each full build. It replaces the folder tree of about a million files (28 GB for Mario Kart 8 Deluxe). Builds that compile on the computer and on the device share it. The old tree is imported once and deleted. |
| `shaderpack.py` | The IPA's iOS shader libraries in 16 packs (`shaders-<0..f>.pack`, by the key's first hex digit) instead of tens of thousands of files. 16, so that a LiveContainer update re-sends only the packs that changed. |
| `shaderpack.c` | The host's reader: each pack is mapped once; lookups are a binary search. A folder without packs is read file by file, as before. |
| `prepare.m` | **Shaders on device**: before the first start, the launcher compiles every library the build left to the device (its Metal source), a pack at a time on every core, with the same source and options as the host's `lib_from_file`, so the game's compiles are hits in Metal's cache instead of a stutter at each first use. The launcher shows how far it is; Play is never held: pressed early, the pass goes on in the background on two cores while the game runs (shaders the game needs first compile on demand into the same cache, and the pass passes over them). iOS saves no compiled library a program could keep, so its cache stands in for a computer build's `.metallib` files: done once per set of shaders, iOS version and GPU (`Documents/dm-shaders-prepared`), and a launch with the marker times a sample of compiles and prepares again when they are slow (the cache was emptied). The log says what it found, how long it took and how many failed. |

The patch wires them in:
- `dmsh_build.py` and `dmsh_mtlcache.py` use the store.
- `build_ipa_portable.py` packs the libraries.
- `nvn_mtl.m` reads through the pack.
