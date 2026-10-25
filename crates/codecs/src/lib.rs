//! Native decoders for the nxnative pipeline (the bundle's lib/; `DM_CODECS` names the library, nxcore::bundle). shader_split.py loads it with ctypes and keeps its
//! Python decoder for inputs this one refuses and for a checkout without it.

/// Yaz0 (Nintendo's LZ77: Mario Kart 8 Deluxe's and Super Mario 3D World's .szs). `src`: the whole file, its 16-byte header included; `dst`: the `dst_len` bytes its header
/// announces. Returns `dst_len`, or -1 for input that reads past its end or refers before the output's start: the caller's Python decoder then decides what such input gives.
/// Group header bits, highest first: 1 = one literal byte; 0 = a back-reference of two bytes {length nibble, 12-bit distance - 1}, a length nibble of 0 taking one more byte + 0x12,
/// any other + 2. Copies may overlap their own output (byte by byte, as the format means)
///
/// # Safety
/// `src` points at `src_len` readable bytes and `dst` at `dst_len` writable ones.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dm_yaz0(src: *const u8, src_len: usize, dst: *mut u8, dst_len: usize) -> isize {
    let s = unsafe { std::slice::from_raw_parts(src, src_len) };
    let d = unsafe { std::slice::from_raw_parts_mut(dst, dst_len) };
    yaz0(s, d).map_or(-1, |n| n as isize)
}

fn yaz0(s: &[u8], d: &mut [u8]) -> Option<usize> {
    let n = d.len();
    let (mut si, mut o) = (16usize, 0usize);
    while o < n {
        let h = *s.get(si)?;
        si += 1;
        for bit in 0..8 {
            if h & (0x80 >> bit) != 0 {
                d[o] = *s.get(si)?;
                si += 1;
                o += 1;
            } else {
                let (b1, b2) = (*s.get(si)? as usize, *s.get(si + 1)? as usize);
                si += 2;
                let dist = ((b1 & 15) << 8 | b2) + 1;
                let ln = if b1 >> 4 != 0 {
                    (b1 >> 4) + 2
                } else {
                    let l = *s.get(si)? as usize + 0x12;
                    si += 1;
                    l
                };
                let st = o.checked_sub(dist)?;
                for i in 0..ln.min(n - o) {
                    d[o + i] = d[st + i];
                }
                o += ln.min(n - o);
            }
            if o >= n {
                break;
            }
        }
    }
    Some(n)
}
