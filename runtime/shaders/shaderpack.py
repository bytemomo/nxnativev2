#!/usr/bin/env python3
# The iOS shader libraries of a build (b-<chain>/: <key>_b.metallib, <key>_b.json, the _bf / _bv / _bg* side libraries; 75,000 files for Mario Kart 8 Deluxe) packed into 16 files,
# shaders-<0..f>.pack by the key's first hex digit, which the host maps once each (shaderpack.c). Fewer files in the IPA and on the device, one mapping per pack instead of
# an open per shader; 16 of them so that a LiveContainer update (which sends the files that changed) re-sends a sixteenth for a few changed shaders, not all of them.
# Format: "DMPK", u32 version 1, u32 count, u32 0; count entries {char name[48] (nul-padded), u64 offset, u64 length} sorted by name; the data, each at a 16-byte boundary.
# Other files (inputs.tsv) are copied as they are. Usage: shaderpack.py <b-chain folder> <output folder>
import os, re, struct, sys, shutil

NAME = 48
PACKED = re.compile(r'^[0-9a-f]{16}_.*\.(metallib|json)$')


def write(src, dst):
    os.makedirs(dst, exist_ok=True)
    groups, n = {}, 0
    for name in sorted(os.listdir(src)):
        p = os.path.join(src, name)
        if not os.path.isfile(p): continue
        if PACKED.match(name) and len(name.encode()) < NAME: groups.setdefault(name[0], []).append(name)
        else: shutil.copyfile(p, os.path.join(dst, name))
    for digit, names in sorted(groups.items()):
        table = 16 + len(names) * (NAME + 16)
        off, ents = (table + 15) & ~15, []
        for name in names:
            size = os.path.getsize(os.path.join(src, name)); ents.append((name, off, size)); off = (off + size + 15) & ~15
        tmp = os.path.join(dst, f'shaders-{digit}.pack.tmp')
        with open(tmp, 'wb') as f:
            f.write(b'DMPK' + struct.pack('<III', 1, len(ents), 0))
            for name, o, size in ents: f.write(name.encode().ljust(NAME, b'\0') + struct.pack('<QQ', o, size))
            for name, o, size in ents:
                f.write(b'\0' * (o - f.tell())); f.write(open(os.path.join(src, name), 'rb').read())
        os.replace(tmp, os.path.join(dst, f'shaders-{digit}.pack'))
        n += len(ents)
    return n, len(groups)


if __name__ == '__main__':
    if len(sys.argv) != 3: sys.exit('usage: shaderpack.py <b-chain folder> <output folder>')
    n, packs = write(sys.argv[1], sys.argv[2])
    print(f'shader pack: {n} files in {packs} packs')
