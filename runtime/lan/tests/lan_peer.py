#!/usr/bin/env python3
# A computer as the other device, for testing local play with one iPhone or iPad: finds the hubs of a game on the Wi-Fi (the Bonjour records lan_nw.m advertises),
# connects with the same TLS key, and asks for the network (F_PROBE → F_NETINFO), as a station's scan does. With --join it then asks to join (F_JOIN): a hub
# answers F_JOINED or F_REFUSED with the nn::ldn result (the game's own passphrase is not known here, so a refusal for it is the expected answer).
# With --address host[:port] it skips Bonjour and asks that address directly (a room over a VPN or another network: lan_nw.m's fixed port, 49540); with
# --invite it asks the room an invite names, with the key it carries (the text the host's Share sends: "nxlan:<host>:<port>/<key>" in it), as a friend's device does.
# Needs: zeroconf (pip; not with --address or --invite), Python 3.13+ with OpenSSL 3 (TLS-PSK).
# Usage: lan_peer.py <title ID> [--password P] [--join] [--seconds N] [--address host[:port] | --invite TEXT]
import argparse, base64, hashlib, hmac, re, socket, ssl, struct, sys, time

SERVICE = '_presence._tcp.local.'
MAGIC, VERSION = 0x314C584E, 1
F_PROBE, F_NETINFO, F_JOIN, F_JOINED, F_REFUSED, F_ROOM = 1, 2, 3, 4, 5, 9
RESULTS = {203 | 16 << 9: 'DeviceNotAvailable', 203 | 32 << 9: 'InvalidState', 203 | 64 << 9: 'ConnectFailure', 203 | 65 << 9: 'ConnectNotFound',
           203 | 66 << 9: 'ConnectTimeout', 203 | 67 << 9: 'ConnectRejected', 203 | 113 << 9: 'VersionTooLow', 203 | 114 << 9: 'VersionTooHigh', 203 | 144 << 9: 'TooManyPlayers'}


def keys(program_id, password):   # as lan_nw.m derive()
    psk = hmac.new(('nxnative-lan-1:' + password).encode(), struct.pack('<Q', program_id), hashlib.sha256).digest()
    return psk, hmac.new(psk, b'tag', hashlib.sha256).hexdigest()[:12]


def frame(sock, data): sock.sendall(struct.pack('<I', len(data)) + data)
def read_exact(sock, n):
    b = b''
    while len(b) < n:
        c = sock.recv(n - len(b))
        if not c: raise ConnectionError('closed by the hub')
        b += c
    return b
def recv_frame(sock):
    n = struct.unpack('<I', read_exact(sock, 4))[0]
    if not 0 < n <= 65536 + 64: raise ValueError(f'frame of {n} bytes')
    return read_exact(sock, n)


def show(info):   # NetworkInfo (lan_core.h ldn_network_info)
    lcid, scene = struct.unpack_from('<qxxH', info, 0)
    ssid_len = info[0x26]; channel = struct.unpack_from('<h', info, 0x48)[0]
    count_max, count = struct.unpack_from('<bB', info, 0x66); adv = struct.unpack_from('<H', info, 0x26A)[0]
    print(f'  local communication ID {lcid & 0xFFFFFFFFFFFFFFFF:016x}, scene {scene}, channel {channel}, players {count}/{count_max}, advertise data {adv} bytes')
    for i in range(8):
        o = 0x68 + 0x40 * i; ip, = struct.unpack_from('<I', info, o); connected = info[o + 0xB]
        if connected: print(f'    player {i}: {info[o + 0xC:o + 0x2D].split(b"\0")[0].decode(errors="replace")!r} at {ip >> 24}.{ip >> 16 & 255}.{ip >> 8 & 255}.{ip & 255}, version {struct.unpack_from("<h", info, o + 0x2E)[0]}')
    if adv: print(f'    advertise data: {info[0x26C:0x26C + adv][:64].hex()}{"…" if adv > 64 else ""}')


try:
    from zeroconf import ServiceBrowser, ServiceListener, Zeroconf
except ImportError:   # --address needs no Bonjour
    ServiceListener = object


class Found(ServiceListener):
    def __init__(self, tag): self.tag, self.hubs = tag, {}
    def add_service(self, zc, type_, name):
        i = zc.get_service_info(type_, name, 3000)
        if not i: return
        t = (i.properties or {}).get(b't', b'').decode()
        print(f'found {name} at {", ".join(i.parsed_addresses())}:{i.port} ({"ours" if t == self.tag else "another game, password or service" if t else "not a local play hub"})')
        if t == self.tag: self.hubs[name] = (i.parsed_addresses(), i.port)
    def update_service(self, zc, type_, name): self.add_service(zc, type_, name)
    def remove_service(self, zc, type_, name): self.hubs.pop(name, None)


def probe(addrs, port, psk, join):
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
    ctx.minimum_version = ctx.maximum_version = ssl.TLSVersion.TLSv1_2; ctx.set_ciphers('PSK-AES128-GCM-SHA256')
    ctx.set_psk_client_callback(lambda hint: ('nxnative-lan', psk))
    for a in sorted(addrs, key=lambda a: ':' in a):   # IPv4 first
        try: raw = socket.create_connection((a, port), timeout=5)
        except OSError as e: print(f'  {a}: cannot connect ({e})'); continue
        try:
            s = ctx.wrap_socket(raw); print(f'  {a}: TLS {s.version()} {s.cipher()[0]}: the key matches')
            frame(s, struct.pack('<BIH', F_PROBE, MAGIC, VERSION))
            f = recv_frame(s)
            if f[0] != F_NETINFO or len(f) != 1 + 0x480: print(f'  unexpected answer: type {f[0]}, {len(f)} bytes'); return
            show(f[1:])
            s.settimeout(1)
            try:   # a hub that names its room sends the name next (F_ROOM; older ones do not)
                r = recv_frame(s)
                if r[0] == F_ROOM: print(f'  room name: {r[1:].decode(errors="replace")!r}' if len(r) > 1 else '  room not named: the host\'s player name says it')
            except (TimeoutError, socket.timeout): pass
            s.settimeout(5)
            if join:
                user = b'computer'.ljust(0x30, b'\0'); lcv = struct.unpack_from('<h', f[1:], 0x68 + 0x2E)[0]
                body = struct.pack('<IH', MAGIC, VERSION) + user + struct.pack('<hBHH', lcv, 0, 1, 0) + b'\0' * 0x40 + f[1:][0x10:0x20]
                frame(s, bytes([F_JOIN]) + body)
                while True:
                    r = recv_frame(s)
                    if r[0] in (F_NETINFO, F_ROOM): continue
                    if r[0] == F_JOINED: print(f'  joined as player {r[1]}'); return
                    if r[0] == F_REFUSED: res = struct.unpack_from('<I', r, 1)[0]; print(f'  refused: {RESULTS.get(res, hex(res))}'); return
            return
        except ssl.SSLError as e: print(f'  {a}: TLS failed ({e.reason}): a different game or password')
        except (OSError, ValueError, ConnectionError) as e: print(f'  {a}: {e}')
        finally: raw.close()


def main():
    ap = argparse.ArgumentParser(); ap.add_argument('title'); ap.add_argument('--password', default=''); ap.add_argument('--join', action='store_true'); ap.add_argument('--seconds', type=float, default=8)
    ap.add_argument('--address', help='host[:port] of a room, asked directly instead of found with Bonjour')
    ap.add_argument('--invite', help="an invite's text: the room it names, asked with its key")
    a = ap.parse_args()
    psk, tag = keys(int(a.title, 16), a.password)
    if a.invite:
        m = re.search(r'nxlan:(\[[0-9A-Fa-f:.]+\]|[A-Za-z0-9.\-]+):([0-9]{1,5})(?:/([A-Za-z0-9_\-]{43}))?', a.invite)
        if not m: print('no nxlan:<host>:<port>[/<key>] in the invite'); return 2
        host, port = m.group(1).strip('[]'), int(m.group(2))
        if m.group(3): psk = base64.urlsafe_b64decode(m.group(3) + '=')
        print(f'asking the room at {host}:{port} with {"the invite\'s key" if m.group(3) else "this password\'s key"}')
        probe([host], port, psk, a.join)
        return 0
    if a.address:
        host, _, port = a.address.rpartition(':') if a.address.count(':') == 1 else (a.address, '', '')
        print(f'asking the room at {host or a.address}:{port or 49540} directly ({"with" if a.password else "no"} password)')
        probe([host or a.address], int(port or 49540), psk, a.join)
        return 0
    print(f'looking for {SERVICE} hubs with tag {tag} for {a.title} ({"with" if a.password else "no"} password), {a.seconds:g} s')
    zc = Zeroconf(); f = Found(tag); ServiceBrowser(zc, SERVICE, f)
    time.sleep(a.seconds)
    for name, (addrs, port) in list(f.hubs.items()): print(f'hub {name}:'); probe(addrs, port, psk, a.join)
    if not f.hubs: print('no hub of this game found')
    zc.close()
    return 0 if f.hubs else 1


if __name__ == '__main__': sys.exit(main())
