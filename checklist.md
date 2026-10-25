# Checklist

What is done, what's being worked on, and what's missing. Newest work at the top of each section.

## Local play (runtime/lan)

- [x] Core: nn::ldn states, hub relay, sockets (UDP, broadcast, TCP) — 60 tests with AddressSanitizer and UBSan
- [x] Host takeovers for every SDK form of nn::ldn and nn::socket; "off" behaves as the offline host
- [x] Transport: Bonjour, peer-to-peer Wi-Fi, TLS with a pre-shared key from the game and the password
- [x] Password on an overlay over the game (lock capsule), applied live; launcher keeps only the Local play switch
- [x] Bonjour type `_presence._tcp`: games in LiveContainer could not browse or advertise our own type (-65555, NoAuth)
- [x] Root cause of "local play crashes": no crash; every Bonjour call failed with -65555 (NoAuth) because the game runs as LiveContainer, which does not declare our service type; Mario Kart then showed its error. Fixed with `_presence._tcp`
- [x] Second-time hosting crash: a stale SystemEvent binding after the game destroyed the event (`hle_bind_system_event_fresh`); keyboard collapse on the overlay (2 s debounce)
- [x] Root cause of 2618-0006 on "create private group": nn::socket's errno and fcntl flags are Linux's (EAGAIN 11, O_NONBLOCK 0x800); Pia mapped our FreeBSD EAGAIN (35) to a fatal 68616 and its receive thread failed. Fixed in `lan_hle.c` (`nn_errno`)
- [x] Root cause of "A communication error has occurred" after creating a room: Pia's Mesh::Create asks `nn::friends::GetPlayHistoryRegistrationKey` for each local player, which the host refused (68616, then CreateMeshJob 27749). The host now makes the key from the user (`hle_friends.c`); AddPlayHistory succeeds. Mario Kart 8 Deluxe hosts a room that stays open
- [x] Diagnostics for debug builds: `lan_diag.c`, `DM_LAN_DEBUG=1`, `nxnative build <game> --debug` (the temporary probes are gone from `lan_hle.c`)
- [x] With the iPad hosting, `lan_peer.py` from the computer: finds the room, TLS with the game's key, reads its network (1/8 players, Mario Kart's advertise data). Joining is refused for the game's passphrase, which only the game knows (expected)
- [x] `runtime/lan/tests/lan_peer.py`: this computer as the second device (finds the hub on the Wi-Fi, TLS with the same key, asks for the network, can try to join)
- [ ] Test on two devices with Mario Kart 8 Deluxe: host, join, race (the join path through Pia is untested)
- [ ] Other games with local play (Pia games should take the same path; untested)
- [x] The app (GUI): Settings › Builds › Debug builds, as the CLI's `--debug`
- [ ] If it still fails: check Settings › Privacy › Local Network › LiveContainer is on
- [x] Rooms over a VPN or another network: a fixed port (49540), rooms joined by address, this device's addresses on the panel; the panel holds the password, port, addresses and nearby search
- [ ] Over a VPN with Mario Kart 8 Deluxe: the friend joins by address (untested)
- [ ] LAN play through `nn::nifm` (games that use the network instead of local wireless)
- [ ] The Mac host build (lan_ui.m has stubs; lan_nw.m builds on macOS, not yet tried there)

## Shader cache: smaller and faster (research: 31 GB for Mario Kart, about 1.4 GB needed)

- [x] 1. Variant source mismatch fixed (a cache hit hands `fetch_finish` the source without `main0_u`, as a miss does); the compile failure cache is kept in `shader/cache/xcrun_failed`
- [x] 2. After each full build: other chain versions' folders removed (already in the compile cache); the store prunes what the last builds did not use
- [x] 3. No Mac libraries on Linux / Windows (only the iOS ones); content-addressed storage instead of copies
- [x] 4. One store for computer and device compiles (the compiler identity keeps their libraries apart); `cas-device` deleted on the first build
- [x] 5. `runtime/shaders/dmsh_store.py`: one SQLite file per game, zstd blobs; the old folder tree imported once and deleted
- [x] 6. `c1_full.tsv` / `cb_extent.tsv` kept per shader keys (2.5 min less per full build); the stage key itself was fine (the 12 min rerun was a switch from device to computer compiles)
- [x] 7. `runtime/shaders/shaderpack.{py,c}`: the IPA's libraries in 16 packs the host maps (round trip tested)
- [x] 8. Extracted game files deleted after each build (`--no-keep-extracted`); the icon is kept beside the build, and the app reads version and icon from the IPA when needed
- [x] Hollow Knight: import, then a fully cached rebuild (2,450 hits, 0 compiles, tables kept); shader folder 334 MB → 127 MB; IPA 8,061 → 2,192 entries
- [x] Mario Kart: 9.5 GB imported into a 0.9 GB store; all 13,711 variants (8,914 before), 0 compiles; shader folder 31 GB → 3.4 GB; IPA 82,648 → 7,771 entries
- [ ] On the device: the packed shaders load (Hollow Knight and Mario Kart from today's IPAs)
## App

- [x] Launcher (on the device): settings in cards by group with icons and an ⓘ explanation each (settings.tsv group / icon / help), chips and Play on the left, Local Network access shown (Allowed / Blocked with Open Settings); the in-game capsule turns red when iOS denies it
- [ ] Launcher: tried on the device

- [x] UI: flat rows, More memory check box, less text
- [x] Frame rate row in the launcher for every game (30 / 60 / 120 / 150)
- [x] Diff-based LiveContainer updates over USB
- [ ] Windows: test the latest dist on a Windows machine
- [ ] README: local play, frame rate, shader cache changes

## Updates and releases

- [x] The client and the bundle update on their own from GitHub releases (nxcore::update): a signed manifest (ed25519, the key built into the client), sizes and SHA-256 checked, versions only go up, a bundle may need a newer client first
- [x] Bundles carry a version (bundle.json, the executable's trailer); the newest complete one is active, so a slim client runs on an installed bundle
- [x] `cargo xtask keygen` / `release [--publish]`: both platforms' apps (with and without their bundle), bundles, CycloneDX SBOMs, the signed manifest
- [x] CI: `.github/workflows/release.yml` for `[ci]` commits on main (Swift object on macOS, the rest on Ubuntu, SBOMs and assets attested)
- [x] The app: Settings › Updates (check, install, check at start); the CLI: `nxnative update [--check]`
- [ ] Make the repository public (the client reads releases anonymously); set the secret NXNATIVEV2_SIGNING_KEY for CI
- [ ] The first published release, and an update from it on Linux and Windows
- [ ] CI's first run (the macOS runner's Xcode 26, the Ubuntu runner's build tools)

- [x] Incremental releases: each component's inputs hashed, only what changed built and uploaded, the manifest the latest of everything (per-asset tags), the bundle API, the transition for existing apps; CI plans first and compiles the Swift object only for a new bundle
- [ ] The transition release (the next `just publish`: full), then an app-only and a bundle-only release on GitHub
- [ ] Pruning old releases (keep the newest 5 and what the latest manifest points to)

## Housekeeping

- [ ] Commit: runtime/, the patch, xtask's runtime copy, checklist.md (not committed yet)
- [ ] `~/.cache/uv` (25 GB): `uv cache clean` once the IDA plugin is closed
