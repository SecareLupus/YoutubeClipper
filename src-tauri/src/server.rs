//! Tiny local HTTP server so the webview can stream local video files with
//! Range support. One listener for the whole app; each registered file gets
//! a token URL; each connection is handled on its own thread so a stalled
//! or abandoned request never blocks the next one.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

struct Server {
    port: u16,
    files: Mutex<HashMap<String, PathBuf>>,
    next_id: Mutex<u64>,
}

static SERVER: OnceLock<Result<Server, String>> = OnceLock::new();

fn server() -> Result<&'static Server, String> {
    SERVER
        .get_or_init(|| {
            let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
            let port = listener.local_addr().map_err(|e| format!("addr: {e}"))?.port();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    std::thread::spawn(move || {
                        let _ = handle(stream);
                    });
                }
            });
            Ok(Server {
                port,
                files: Mutex::new(HashMap::new()),
                next_id: Mutex::new(0),
            })
        })
        .as_ref()
        .map_err(|e| e.clone())
}

/// Register `path` and return a URL the webview can load.
pub fn register(path: &str) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("stat {path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{path} is not a file"));
    }
    let s = server()?;
    let token = {
        let mut id = s.next_id.lock().unwrap();
        *id += 1;
        format!("v{}", *id)
    };
    s.files.lock().unwrap().insert(token.clone(), PathBuf::from(path));
    Ok(format!("http://127.0.0.1:{}/{}.mp4", s.port, token))
}

fn lookup(token: &str) -> Option<PathBuf> {
    server().ok()?.files.lock().unwrap().get(token).cloned()
}

/// Parse "bytes=a-b", "bytes=a-", "bytes=-n" into an inclusive range.
fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    if size == 0 {
        return None;
    }
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        (size.saturating_sub(n), size - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() { size - 1 } else { b.parse::<u64>().ok()?.min(size - 1) };
        (start, end)
    };
    if start > end || start >= size {
        return None;
    }
    Some((start, end))
}

fn handle(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    // Read headers (up to 16KB) until the blank line.
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk)?;
        if n == 0 || buf.len() > 16 * 1024 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let req = String::from_utf8_lossy(&buf);
    let mut lines = req.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let range_header = lines
        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
        .map(|l| l[6..].trim().to_string());

    let token = target.trim_start_matches('/').split(['.', '?']).next().unwrap_or_default();
    let Some(path) = lookup(token) else {
        return respond_empty(&mut stream, "404 Not Found");
    };
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return respond_empty(&mut stream, "404 Not Found"),
    };
    let size = file.metadata()?.len();

    let (status, start, end) = match &range_header {
        Some(r) => match parse_range(r, size) {
            Some((s, e)) => ("206 Partial Content", s, e),
            None => {
                let resp = format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                return stream.write_all(resp.as_bytes());
            }
        },
        None => ("200 OK", 0, size.saturating_sub(1)),
    };
    let len = if size == 0 { 0 } else { end - start + 1 };

    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\nContent-Length: {len}\r\nConnection: close\r\n"
    );
    if status.starts_with("206") {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{size}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if method.eq_ignore_ascii_case("HEAD") || len == 0 {
        return Ok(());
    }

    file.seek(SeekFrom::Start(start))?;
    let mut remaining = len;
    let mut out = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let cap = remaining.min(out.len() as u64) as usize;
        let n = file.read(&mut out[..cap])?;
        if n == 0 {
            break;
        }
        // Errors here mean the client went away (normal when seeking).
        stream.write_all(&out[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

fn respond_empty(stream: &mut TcpStream, status: &str) -> std::io::Result<()> {
    let resp = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(resp.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=90-500", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=0-", 0), None);
        assert_eq!(parse_range("bytes=5-2", 100), None);
    }
}
