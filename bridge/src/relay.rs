//! Minimal TCP client for the surf-relay.
//!
//! The surf-relay speaks newline-delimited JSON on TCP 8787 (same protocol
//! the hosted sim device uses — `relay/surf_relay.py`). The bridge is a
//! drop-in replacement for that sim path: one envelope per line, replies
//! arrive as one line.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{anyhow, Result};

/// Send one envelope (JSON bytes) to the relay and read exactly one reply
/// line. Kept synchronous and small like the python relay it mirrors.
pub fn exchange(relay_addr: &str, envelope: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    let mut stream = TcpStream::connect(relay_addr)
        .map_err(|e| anyhow!("connect {relay_addr}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|_| stream.set_write_timeout(Some(timeout)))
        .map_err(|e| anyhow!("set timeout: {e}"))?;

    stream
        .write_all(envelope)
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| stream.flush())
        .map_err(|e| anyhow!("write to relay: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .map_err(|e| anyhow!("read from relay: {e}"))?;
    if line.is_empty() {
        return Err(anyhow!("relay closed the connection"));
    }
    // Strip the framing newline so the payload handed back to the device is
    // exactly the relay's JSON line (matches the sim's TCP read path).
    while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    /// A canned relay that echoes a pong for any line — mirrors the python
    /// `test_broadcast.py` fake-relay convention.
    fn spawn_canned_relay(reply: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    // read until newline
                    let mut got = Vec::new();
                    loop {
                        let n = match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        got.extend_from_slice(&buf[..n]);
                        if got.contains(&b'\n') {
                            break;
                        }
                    }
                    let _ = stream.write_all(reply);
                    let _ = stream.flush();
                });
            }
        });
        addr
    }

    #[test]
    fn sends_envelope_and_reads_reply() {
        let addr = spawn_canned_relay(b"{\"type\":\"pong\"}\n");
        let out = exchange(
            &addr,
            br#"{"type":"ping"}"#,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&out).contains("pong"));
    }

    #[test]
    fn errors_when_relay_unreachable() {
        // Bind then drop — port is closed.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        let res = exchange(&addr, b"{}", Duration::from_millis(300));
        assert!(res.is_err());
    }

    #[test]
    fn errors_when_relay_never_replies() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut b = [0u8; 1024];
            let _ = s.read(&mut b); // swallow input, never reply
            let _ = std::thread::sleep(Duration::from_secs(10));
        });
        let res = exchange(&addr, b"{}", Duration::from_millis(400));
        assert!(res.is_err());
    }
}
