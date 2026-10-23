//! Sharing a game with LiveContainer over Wi-Fi: a small HTTP server on this computer, reachable from the device on the same network.
//! Its page (opened in Safari, e.g. by the app's QR code) has two links LiveContainer handles itself:
//! - `livecontainer://certificate?cert=<base64 .p12>&password=` imports the account's signing certificate (LiveContainer → Settings)
//! - `livecontainer://install?url=<this server>/<game>.ipa` makes LiveContainer download the IPA from here and install it (it signs it)
//! Every path starts with a random token: only someone with the link can download the IPA or the certificate.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};

pub const DEFAULT_PORT: u16 = 8642;

pub struct Share {
    pub name: String,
    pub ipa: PathBuf,
    /// The .p12 (empty password); none when not signed in
    pub certificate: Option<Vec<u8>>,
}

pub struct Server {
    pub url: String,
    stop: Arc<AtomicBool>,
    addr: SocketAddr,
    /// bytes of the IPA sent so far (all downloads)
    pub sent: Arc<AtomicU64>,
}

impl Server {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr); // wakes the accept loop
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// This computer's address on the local network (the interface of the default route)
pub fn lan_ip() -> IpAddr {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("192.0.2.1:9")?; // TEST-NET: nothing is sent, the kernel only picks the route
            s.local_addr()
        })
        .map(|a| a.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn token() -> String {
    let mut b = [0u8; 12];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        b.copy_from_slice(&n.to_le_bytes()[..12]);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Starts serving `share` on `port` (0 = any free port) and returns the page's address
pub fn start(share: Share, port: u16) -> Result<Server> {
    let listener = TcpListener::bind(("0.0.0.0", port)).or_else(|_| TcpListener::bind(("0.0.0.0", 0))).context("open a port for the share server")?;
    let addr = listener.local_addr()?;
    let tok = token();
    let base = format!("http://{}:{}/{tok}", lan_ip(), addr.port());
    let file = format!("{}.ipa", crate::export::file_name(&share.name).replace(' ', "-"));
    let ipa_url = format!("{base}/{file}");
    let cert_link = share.certificate.as_ref().map(|p12| format!("livecontainer://certificate?cert={}&password=", url_encode(&base64(p12))));
    let page = page(&share.name, &ipa_url, cert_link.as_deref());
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU64::new(0));
    let (stop2, sent2) = (stop.clone(), sent.clone());
    tracing::info!(url = %base, file = %file, "share server: started");
    let share = Arc::new(share);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let Ok(conn) = conn else { continue };
            let (share, page, tok, file, sent) = (share.clone(), page.clone(), tok.clone(), file.clone(), sent2.clone());
            std::thread::spawn(move || {
                if let Err(e) = handle(conn, &share, &page, &tok, &file, &sent) {
                    tracing::debug!("share server: {e:#}");
                }
            });
        }
        tracing::info!("share server stopped");
    });
    Ok(Server { url: format!("{base}/"), stop, addr: SocketAddr::new(lan_ip(), addr.port()), sent })
}

fn page(name: &str, ipa_url: &str, cert_link: Option<&str>) -> String {
    let name = html_escape(name);
    let install = format!("livecontainer://install?url={}", url_encode(ipa_url));
    let cert = match cert_link {
        Some(l) => format!(r#"<p><a class="b" href="{}">1 · Import the certificate into LiveContainer</a></p><p class="n">Once per Apple ID: LiveContainer signs the games with it. It must be the Apple ID LiveContainer itself was installed with.</p>"#, html_escape(l)),
        None => r#"<p class="n">No certificate here (not signed in in nxnativev2): import one in LiveContainer yourself.</p>"#.into(),
    };
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{name}</title>
<style>body{{font-family:-apple-system,sans-serif;max-width:560px;margin:40px auto;padding:0 20px;background:#111;color:#eee}}a.b{{display:block;padding:16px;border-radius:12px;background:#0a84ff;color:#fff;text-decoration:none;text-align:center;font-weight:600;margin:12px 0}}a.s{{color:#0a84ff}}.n{{color:#999;font-size:14px}}</style></head>
<body><h2>{name}</h2>{cert}
<p><a class="b" href="{install}">2 · Install in LiveContainer</a></p><p class="n">LiveContainer downloads the game from this computer (keep nxnativev2 open) and installs it; the first launch signs it.</p>
<p class="n">Or <a class="s" href="{ipa}">download the IPA</a> (Files › Downloads), then install it in LiveContainer from there.</p></body></html>"#,
        ipa = html_escape(ipa_url)
    )
}

fn handle(mut conn: TcpStream, share: &Share, page: &str, tok: &str, file: &str, sent: &AtomicU64) -> Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut range_start = 0u64;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        // "Range: bytes=N-" (a resumed download)
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("range: bytes=") {
            range_start = v.trim().trim_end_matches('-').split('-').next().and_then(|n| n.parse().ok()).unwrap_or(0);
        }
    }
    let mut parts = first.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let head = method == "HEAD";
    let rest = path.strip_prefix('/').and_then(|p| p.strip_prefix(tok)).unwrap_or("\u{0}");
    match rest {
        "" | "/" => respond(&mut conn, "200 OK", "text/html; charset=utf-8", page.as_bytes(), head),
        r if r.trim_start_matches('/') == file => {
            let mut f = std::fs::File::open(&share.ipa)?;
            let len = f.metadata()?.len();
            let start = range_start.min(len);
            let status = if start > 0 { "206 Partial Content" } else { "200 OK" };
            let mut hdr = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nContent-Disposition: attachment; filename=\"{file}\"\r\nConnection: close\r\n",
                len - start
            );
            if start > 0 {
                hdr.push_str(&format!("Content-Range: bytes {start}-{}/{len}\r\n", len.saturating_sub(1)));
            }
            hdr.push_str("\r\n");
            conn.write_all(hdr.as_bytes())?;
            if head {
                return Ok(());
            }
            tracing::info!(from = %conn.peer_addr().map(|a| a.to_string()).unwrap_or_default(), start, "share server: sending the IPA");
            std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(start))?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                conn.write_all(&buf[..n])?;
                sent.fetch_add(n as u64, Ordering::Relaxed);
            }
            tracing::info!("share server: IPA sent");
            Ok(())
        }
        _ => respond(&mut conn, "404 Not Found", "text/plain", b"not found", head),
    }
}

fn respond(conn: &mut TcpStream, status: &str, ty: &str, body: &[u8], head: bool) -> Result<()> {
    conn.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: {ty}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes())?;
    if !head {
        conn.write_all(body)?;
    }
    Ok(())
}

/// A QR code of `text` as RGBA pixels (black on white, with a quiet zone), `scale` pixels per module
pub fn qr_rgba(text: &str, scale: u32) -> Option<(u32, Vec<u8>)> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let n = code.width() as u32;
    let quiet = 4;
    let size = (n + 2 * quiet) * scale;
    let mut px = vec![255u8; (size * size * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            if code[(x as usize, y as usize)] == qrcode::Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let (px_x, px_y) = ((x + quiet) * scale + dx, (y + quiet) * scale + dy);
                        let i = ((px_y * size + px_x) * 4) as usize;
                        px[i..i + 3].copy_from_slice(&[0, 0, 0]);
                    }
                }
            }
        }
    }
    Some((size, px))
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches() {
        assert_eq!(super::base64(b"Man"), "TWFu");
        assert_eq!(super::base64(b"Ma"), "TWE=");
        assert_eq!(super::base64(b"M"), "TQ==");
    }
}
