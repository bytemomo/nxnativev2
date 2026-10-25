#!/usr/bin/env python3
# The shader caches of one game in one file: <cache>/shaders.db (SQLite), in place of the folder tree (cas/index, cas/shader, cas/variant, cas/metallib) that held about
# a million small files for Mario Kart 8 Deluxe (28 GB, of which about 0.4 GB was live data: the rest was old entries, the same bytes copied up to six times, and block slack).
#   blob(h, z, n):   content-addressed data, zstd-compressed (h = sha256 of the bytes): each distinct file is stored once, whatever names it has
#   entry(ns, k, v, used): named records (JSON) that point at blobs, with the build that last used them:
#     index:   shader key → its digest and fingerprints (why a key missed)
#     shader:  digest → the translation's outputs per target folder ({"mac": {name: blob}, "ios": {...}}): sources and reflection only; libraries come from mtl
#     variant: variant digest → its source
#     mtl:     Metal compile digest (the MSL with the compiler's identity) → the library
# Builds that compile on the device and on the computer share it: their compilers' identities differ (the xcrun stand-in reports the deferral), so only the mtl entries are apart.
# prune() keeps what the last few builds used; the old folder tree is imported once (import_dirs) and then deleted.
import json, os, sqlite3, threading, hashlib, shutil
try: from compression import zstd   # Python 3.14
except ImportError: from backports import zstd

LEVEL = 3   # zstd: about 10x on MSL, 3.5x on reflection, 1.8x on libraries, at hundreds of MB/s


class Store:
    def __init__(self, path):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        self.path, self.lock = path, threading.Lock()
        new = not os.path.exists(path)
        self.db = sqlite3.connect(path, timeout=120, check_same_thread=False, isolation_level=None)
        if new: self.db.execute('PRAGMA auto_vacuum=INCREMENTAL')   # space freed by prune goes back to the disk
        self.db.execute('PRAGMA journal_mode=WAL'); self.db.execute('PRAGMA synchronous=NORMAL')
        self.db.executescript('''CREATE TABLE IF NOT EXISTS blob(h TEXT PRIMARY KEY, z BLOB NOT NULL, n INTEGER NOT NULL) WITHOUT ROWID;
                                 CREATE TABLE IF NOT EXISTS entry(ns TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL, used INTEGER NOT NULL, PRIMARY KEY(ns, k)) WITHOUT ROWID;
                                 CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL) WITHOUT ROWID;''')
        r = self.db.execute("SELECT v FROM meta WHERE k='run'").fetchone()
        self.run = (int(r[0]) if r else 0) + 1   # this build's number: entries it reads or writes are marked with it
        self.db.execute("INSERT OR REPLACE INTO meta VALUES('run', ?)", (str(self.run),))

    def reconnect(self):   # a forked worker's own connection (dmsh_build.py pmap): one must not be used on both sides of a fork. This build's run number stays
        self.lock = threading.Lock()
        self.db = sqlite3.connect(self.path, timeout=120, check_same_thread=False, isolation_level=None)
        self.db.execute('PRAGMA synchronous=NORMAL')

    # ---- blobs ----
    def put(self, data):
        h = hashlib.sha256(data).hexdigest()
        with self.lock: have = self.db.execute('SELECT 1 FROM blob WHERE h=?', (h,)).fetchone()
        if not have:
            z = zstd.compress(data, LEVEL)
            with self.lock: self.db.execute('INSERT OR IGNORE INTO blob VALUES(?, ?, ?)', (h, z, len(data)))
        return h

    def get(self, h):
        with self.lock: r = self.db.execute('SELECT z FROM blob WHERE h=?', (h,)).fetchone()
        return zstd.decompress(r[0]) if r else None

    def put_file(self, path): return self.put(open(path, 'rb').read())

    def write(self, h, path):   # a blob to a file (written beside it, then moved into place); False when the blob is missing
        data = self.get(h)
        if data is None: return False
        tmp = f'{path}.tmp-{os.getpid()}-{threading.get_ident()}'
        with open(tmp, 'wb') as f: f.write(data)
        os.replace(tmp, path)
        return True

    # ---- entries ----
    def lookup(self, ns, k):
        with self.lock:
            r = self.db.execute('SELECT v, used FROM entry WHERE ns=? AND k=?', (ns, k)).fetchone()
            if r and r[1] != self.run: self.db.execute('UPDATE entry SET used=? WHERE ns=? AND k=?', (self.run, ns, k))
        return json.loads(r[0]) if r else None

    def set(self, ns, k, value):
        v = json.dumps(value, separators=(',', ':'))
        with self.lock: self.db.execute('INSERT OR REPLACE INTO entry VALUES(?, ?, ?, ?)', (ns, k, v, self.run))

    # ---- upkeep ----
    def prune(self, keep):
        """Drops entries no build used in the last keep[ns] builds, then the blobs nothing points at; → (entries, blobs, bytes) removed"""
        with self.lock:
            gone = 0
            for ns, n in keep.items(): gone += self.db.execute('DELETE FROM entry WHERE ns=? AND used<=?', (ns, self.run - n)).rowcount
            live = set()
            for (v,) in self.db.execute('SELECT v FROM entry'): live.update(_blobs(json.loads(v)))
            self.db.execute('CREATE TEMP TABLE IF NOT EXISTS live(h TEXT PRIMARY KEY) WITHOUT ROWID'); self.db.execute('DELETE FROM live')
            self.db.executemany('INSERT OR IGNORE INTO live VALUES(?)', ((h,) for h in live))
            dead = self.db.execute('SELECT count(*), coalesce(sum(length(z)), 0) FROM blob WHERE h NOT IN (SELECT h FROM live)').fetchone()
            self.db.execute('DELETE FROM blob WHERE h NOT IN (SELECT h FROM live)')
            self.db.execute('PRAGMA incremental_vacuum'); self.db.execute('PRAGMA wal_checkpoint(TRUNCATE)')
        return gone, dead[0], dead[1]

    def size(self):
        with self.lock: r = self.db.execute('SELECT count(*), coalesce(sum(n), 0), coalesce(sum(length(z)), 0) FROM blob').fetchone()
        return r   # (blobs, bytes, bytes stored)

    def close(self):
        with self.lock: self.db.execute('PRAGMA wal_checkpoint(TRUNCATE)'); self.db.close()


def _blobs(v):   # the blob hashes an entry's value names (any string of 64 hex digits under it)
    if isinstance(v, dict): v = list(v.values())
    if isinstance(v, list):
        for x in v: yield from _blobs(x)
    elif isinstance(v, str) and len(v) == 64 and all(c in '0123456789abcdef' for c in v): yield v


def import_dirs(store, cas, log=print, variant_lib=None):
    """The folder tree of earlier builds (cas/: index, shader, variant, metallib) into the store, then deleted: the compiled libraries keyed as before, the live per-shader
    entries (an index points at them) with their sources and reflection, the variants' sources. A variant's libraries were compiled beside it, not through the compile
    cache: variant_lib(target, source, library path) files them there (13,000 compiles under wine for Mario Kart 8 Deluxe otherwise)"""
    if not os.path.isdir(cas): return
    n = {'mtl': 0, 'shader': 0, 'variant': 0}
    lib = os.path.join(cas, 'metallib')
    for target in sorted(os.listdir(lib)) if os.path.isdir(lib) else []:
        for d, _, fs in os.walk(os.path.join(lib, target)):
            for f in fs:
                if f.endswith('.metallib') and len(f) == 73:
                    store.set('mtl', f[:-9], {'lib': store.put_file(os.path.join(d, f))}); n['mtl'] += 1
    idx = os.path.join(cas, 'index')
    for f in sorted(os.listdir(idx)) if os.path.isdir(idx) else []:
        try: meta = json.load(open(os.path.join(idx, f)))
        except (OSError, ValueError): continue
        root, value = os.path.join(cas, 'shader', meta.get('digest', '')), {}
        for label in ('mac', 'ios'):
            d = os.path.join(root, label)
            if os.path.isdir(d): value[label] = {x: store.put_file(os.path.join(d, x)) for x in sorted(os.listdir(d)) if x.endswith(('.metal', '.json'))}
        if value.get('mac'):
            store.set('shader', meta['digest'], value); store.set('index', f[:-5], meta); n['shader'] += 1
    var = os.path.join(cas, 'variant')
    for d in sorted(os.listdir(var)) if os.path.isdir(var) else []:
        p = os.path.join(var, d, 'metal')
        if not os.path.isfile(p): continue
        store.set('variant', d, {'metal': store.put_file(p)}); n['variant'] += 1
        for target in ('mac', 'ios'):
            lib = os.path.join(var, d, target + '.metallib')
            if variant_lib and os.path.isfile(lib): variant_lib(target, open(p, 'rb').read(), lib)
    log(f'shader store: imported the folder cache {cas} ({n["mtl"]} compiled libraries, {n["shader"]} shaders, {n["variant"]} variants); deleting it')
    shutil.rmtree(cas, ignore_errors=True)
