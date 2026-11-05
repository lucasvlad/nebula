//! Small helpers shared by the `mitm-demo` binary: a capturing relay, a
//! hex dump, a finder for readable text, and ANSI colours.
//!
//! None of this is part of the product. It only watches (or, in the demo,
//! meddles with) bytes that already travel over the network.

use std::sync::OnceLock;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Everything the relay saw, per direction (client to server, server to client).
#[derive(Debug, Default, Clone)]
pub struct Captured {
    pub c2s: Vec<u8>,
    pub s2c: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct RelayOpts {
    /// Print each forwarded chunk as it passes.
    pub live: bool,
    /// Flip one bit of the client-to-server byte at this stream offset.
    pub tamper_at: Option<u64>,
}

/// Forward bytes between two connected sockets until both sides finish,
/// recording everything. Read errors count as "connection closed".
pub async fn relay(client: TcpStream, server: TcpStream, opts: &RelayOpts) -> Captured {
    let (cr, cw) = client.into_split();
    let (sr, sw) = server.into_split();
    let (c2s, s2c) = tokio::join!(
        pump(cr, sw, "Device B → Device A", opts.live, opts.tamper_at),
        pump(sr, cw, "Device A → Device B", opts.live, None),
    );
    Captured { c2s, s2c }
}

async fn pump(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    label: &str,
    live: bool,
    tamper_at: Option<u64>,
) -> Vec<u8> {
    let mut seen = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = from.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        let chunk = &mut buf[..n];
        let start = seen.len() as u64;
        if let Some(at) = tamper_at
            && at >= start
            && at < start + n as u64
        {
            let i = (at - start) as usize;
            chunk[i] ^= 0x01;
            println!(
                "  {}",
                red(&format!("Tampering with byte {at}: flipping one bit."))
            );
        }
        if live {
            println!("  {}", cyan(&format!("{label}, {n} bytes")));
            for line in hexdump(chunk, 48).lines() {
                println!("    {}", dim(line));
            }
        }
        seen.extend_from_slice(chunk);
        if to.write_all(chunk).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
    seen
}

/// Classic hex dump, 16 bytes per line. Shows at most `max` bytes and says
/// how many more were left out.
pub fn hexdump(bytes: &[u8], max: usize) -> String {
    let shown = &bytes[..bytes.len().min(max)];
    let mut out = String::new();
    for (i, line) in shown.chunks(16).enumerate() {
        let hex: Vec<String> = line.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = line
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        out.push_str(&format!(
            "{:04x}  {:<47}  |{ascii}|\n",
            i * 16,
            hex.join(" ")
        ));
    }
    if shown.len() < bytes.len() {
        out.push_str(&format!(
            "…     ({} more bytes)\n",
            bytes.len() - shown.len()
        ));
    }
    out
}

fn is_text_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b',' | b'\'' | b'!' | b'?' | b'-')
}

/// Runs of at least `min_len` human-readable ASCII characters (letters,
/// digits, spaces and basic punctuation), trimmed of surrounding spaces.
pub fn readable_runs(bytes: &[u8], min_len: usize) -> Vec<String> {
    let mut runs = Vec::new();
    for part in bytes.split(|&b| !is_text_byte(b)) {
        let text = String::from_utf8_lossy(part);
        let text = text.trim();
        if text.len() >= min_len && text.chars().any(|c| c.is_ascii_alphabetic()) {
            runs.push(text.to_string());
        }
    }
    runs
}

/// True if `needle` occurs in `hay` (case-insensitive, plain bytes).
pub fn contains_text(hay: &[u8], needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase().into_bytes();
    !needle.is_empty()
        && hay
            .to_ascii_lowercase()
            .windows(needle.len())
            .any(|w| w == needle.as_slice())
}

/// Print "Readable text found: ..." for captured bytes.
pub fn report_readable(bytes: &[u8]) {
    let runs = readable_runs(bytes, 5);
    if runs.is_empty() {
        println!("  Readable text found: {}", green("none"));
    } else {
        println!("  Readable text found:");
        for r in runs {
            println!("    \"{}\"", yellow(&r));
        }
    }
}

// ---- colours -------------------------------------------------------------

fn color_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()))
}

fn paint(code: &str, s: &str) -> String {
    if color_on() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn red(s: &str) -> String {
    paint("1;31", s)
}
pub fn green(s: &str) -> String {
    paint("1;32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
}
pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn dim(s: &str) -> String {
    paint("2", s)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hexdump_format() {
        let d = hexdump(b"Hello, Nebula!!!XYZ", 100);
        let mut lines = d.lines();
        assert_eq!(
            lines.next(),
            Some("0000  48 65 6c 6c 6f 2c 20 4e 65 62 75 6c 61 21 21 21  |Hello, Nebula!!!|")
        );
        let second = lines.next().unwrap();
        assert!(second.starts_with("0010  58 59 5a "));
        assert!(second.ends_with("|XYZ|"));
    }

    #[test]
    fn hexdump_truncates() {
        let d = hexdump(&[0u8; 100], 32);
        assert_eq!(d.lines().count(), 3);
        assert!(d.contains("68 more bytes"));
    }

    #[test]
    fn finds_sentence_in_plaintext() {
        let s = "Meet at the library at 7. My locker code is 4821.";
        let mut data = vec![0u8, 1, 2];
        data.extend_from_slice(s.as_bytes());
        data.push(0xff);
        assert_eq!(readable_runs(&data, 5), vec![s.to_string()]);
        assert!(contains_text(&data, "LOCKER"));
    }

    #[test]
    fn nothing_long_in_random_bytes() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let data: Vec<u8> = (0..4000)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 33) as u8
            })
            .collect();
        assert!(readable_runs(&data, 8).is_empty());
    }
}
