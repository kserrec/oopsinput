//! Loopback HTTP/1.1 client for Ollama (SPEC §5-L4, §12). Transport only:
//! POST a JSON body, get the response body bytes back. Prompt assembly and
//! schema validation belong to the inference layer, not here.
//!
//! Self-written on purpose — SPEC §12 rejects an HTTP crate for this:
//! loopback needs no TLS, no redirects, no connection reuse; one POST per
//! process. What it does need is the discipline every external helper
//! already answers to: a single hard deadline covering *every* blocking
//! call (connect, each write, each read — a server trickling one byte per
//! poll must not evade it), and a cap on how much a broken or hostile
//! server can make us buffer. On any error the caller treats model
//! evidence as unavailable (SPEC §9-6); nothing here can block a command.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// Ollama's default listen address. Fixed, not configurable, in v1:
/// SPEC §14 allows no network beyond loopback, and an address knob would be
/// a standing invitation to point the client somewhere else. The debug-only
/// port override exists for the integration tests' mock servers and can
/// never leave loopback — only the port is variable.
pub(crate) fn ollama_addr() -> SocketAddr {
    #[cfg(debug_assertions)]
    if let Ok(p) = std::env::var("OOPSINPUT_TEST_OLLAMA_PORT")
        && let Ok(port) = p.parse::<u16>()
    {
        return SocketAddr::from(([127, 0, 0, 1], port));
    }
    SocketAddr::from(([127, 0, 0, 1], 11434))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ModelError {
    /// Refused before any I/O: the target address is not loopback.
    NotLoopback,
    /// Refused before sending: the process on the other end of the
    /// connection is not owned by a trusted uid (see `verify_peer`).
    UntrustedPeer,
    /// Could not connect — the daemon is down or not listening.
    Connect,
    /// The overall deadline expired somewhere in the exchange.
    Timeout,
    /// The response exceeded a size cap.
    TooLarge,
    /// The response was not parseable HTTP, or ended mid-body.
    Malformed,
    /// Well-formed HTTP with a non-2xx status.
    Status(u16),
    /// Read/write failed for a reason other than the deadline.
    Io,
}

/// The response head (status line + headers) may not exceed this. Ollama's
/// heads are a few hundred bytes.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// POST `body` to `http://{addr}{path}`, return the response body on any
/// 2xx. `deadline` bounds the whole exchange; `max_body` bounds the decoded
/// response body.
pub(crate) fn post_json(
    addr: SocketAddr,
    path: &str,
    body: &[u8],
    deadline: Instant,
    max_body: usize,
) -> Result<Vec<u8>, ModelError> {
    if !addr.ip().is_loopback() {
        return Err(ModelError::NotLoopback);
    }
    let mut stream = TcpStream::connect_timeout(&addr, remaining(deadline)?).map_err(|e| {
        if is_timeout(&e) {
            ModelError::Timeout
        } else {
            ModelError::Connect
        }
    })?;
    verify_peer(&stream, addr.port())?;

    let head = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    write_all(&mut stream, head.as_bytes(), deadline)?;
    write_all(&mut stream, body, deadline)?;

    read_response(&mut stream, deadline, max_body)
}

/// Human account uids start here on standard Linux (SYS_UID_MAX). Below it:
/// root and system service accounts — the `ollama` systemd user, Docker's
/// root-owned proxy.
const FIRST_HUMAN_UID: u32 = 1000;

/// SECURITY (audit 2026-08-06): anyone's process may bind 127.0.0.1:11434
/// while Ollama is down — binding a free unprivileged port needs no rights —
/// and this client used to send the raw command buffer to whatever answered.
/// On a shared machine that hands gate-eligible command lines to any other
/// local user squatting the port. So, after connecting and BEFORE sending a
/// byte: find the accepted socket's entry in /proc/net/tcp (the peer's side
/// of *this* connection — local = the service port, remote = our ephemeral
/// port, so there is no window to swap in a different listener) and require
/// its owner to be our own user or a system account (uid < 1000). Any doubt
/// — unreadable table, missing entry, foreign human uid — refuses the
/// consultation; the caller falls back to deterministic-only, the same as
/// every other model failure. A hostile *system* account is outside the
/// threat model (that is a compromised machine, which SPEC §9 states we do
/// not resist).
fn verify_peer(stream: &TcpStream, service_port: u16) -> Result<(), ModelError> {
    let our_port = stream
        .local_addr()
        .map_err(|_| ModelError::UntrustedPeer)?
        .port();
    let table = std::fs::read_to_string("/proc/net/tcp").map_err(|_| ModelError::UntrustedPeer)?;
    let peer_uid =
        find_peer_uid(&table, service_port, our_port).ok_or(ModelError::UntrustedPeer)?;
    // Our own uid, without libc: /proc/self is owned by this process's uid.
    let self_uid = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self")
            .map_err(|_| ModelError::UntrustedPeer)?
            .uid()
    };
    if peer_uid_trusted(peer_uid, self_uid) {
        Ok(())
    } else {
        Err(ModelError::UntrustedPeer)
    }
}

fn peer_uid_trusted(peer_uid: u32, self_uid: u32) -> bool {
    peer_uid == self_uid || peer_uid < FIRST_HUMAN_UID
}

/// Find the uid owning the peer's side of an established loopback
/// connection in a /proc/net/tcp table: the row whose local address is
/// 127.0.0.1:`service_port` and whose remote address is 127.0.0.1:`our_port`.
/// Row layout: `sl local_address rem_address st ... uid ...` with addresses
/// as `0100007F:PORT` (hex, uppercase) and uid at whitespace field 7.
fn find_peer_uid(table: &str, service_port: u16, our_port: u16) -> Option<u32> {
    let local = format!("0100007F:{service_port:04X}");
    let remote = format!("0100007F:{our_port:04X}");
    table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let _sl = fields.next()?;
        if fields.next()? != local || fields.next()? != remote {
            return None;
        }
        fields.nth(4)?.parse().ok() // skip st, tx_rx, tr_tm, retrnsmt → uid
    })
}

/// Time left before `deadline`, or Timeout. Never returns zero (the strict
/// comparison guarantees it), which matters because `set_read_timeout`
/// rejects a zero Duration.
fn remaining(deadline: Instant) -> Result<Duration, ModelError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(ModelError::Timeout);
    }
    Ok(deadline - now)
}

fn is_timeout(e: &std::io::Error) -> bool {
    // Linux reports a timed-out read/write on a blocking socket as EAGAIN,
    // which std maps to WouldBlock.
    matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
}

/// `write_all` with the deadline re-applied per syscall, so a slow-draining
/// peer cannot stretch the total past the deadline.
fn write_all(stream: &mut TcpStream, mut buf: &[u8], deadline: Instant) -> Result<(), ModelError> {
    while !buf.is_empty() {
        stream
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|_| ModelError::Io)?;
        match stream.write(buf) {
            Ok(0) => return Err(ModelError::Io),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if is_timeout(&e) => return Err(ModelError::Timeout),
            Err(_) => return Err(ModelError::Io),
        }
    }
    Ok(())
}

/// One bounded read appended to `out`. Ok(0) is EOF. The read timeout is
/// recomputed from the deadline on every call — this, not the per-call
/// timeout alone, is what defeats a trickling server.
fn read_some(
    stream: &mut TcpStream,
    out: &mut Vec<u8>,
    deadline: Instant,
) -> Result<usize, ModelError> {
    stream
        .set_read_timeout(Some(remaining(deadline)?))
        .map_err(|_| ModelError::Io)?;
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                return Ok(n);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if is_timeout(&e) => return Err(ModelError::Timeout),
            Err(_) => return Err(ModelError::Io),
        }
    }
}

fn read_response(
    stream: &mut TcpStream,
    deadline: Instant,
    max_body: usize,
) -> Result<Vec<u8>, ModelError> {
    let mut raw = Vec::new();
    let head_end = loop {
        if let Some(pos) = find(&raw, b"\r\n\r\n") {
            break pos;
        }
        if raw.len() > MAX_HEAD_BYTES {
            return Err(ModelError::TooLarge);
        }
        if read_some(stream, &mut raw, deadline)? == 0 {
            return Err(ModelError::Malformed); // EOF before the head ended
        }
    };
    let (status, chunked, content_length) = parse_head(&raw[..head_end])?;
    if !(200..300).contains(&status) {
        return Err(ModelError::Status(status));
    }

    let mut body: Vec<u8> = raw[head_end + 4..].to_vec();
    if chunked {
        // Cap the *encoded* stream too: normal chunk framing is a fraction
        // of a percent of the data, so 2x + slack never bites a sane
        // response, but a server padding with chunk extensions or one-byte
        // chunks hits it and fails closed to "unavailable evidence".
        let raw_cap = max_body.saturating_mul(2).saturating_add(4096);
        loop {
            if let Some(decoded) = decode_chunked(&body, max_body)? {
                return Ok(decoded);
            }
            if body.len() > raw_cap {
                return Err(ModelError::TooLarge);
            }
            if read_some(stream, &mut body, deadline)? == 0 {
                return Err(ModelError::Malformed); // EOF mid-chunk
            }
        }
    } else if let Some(len) = content_length {
        if len > max_body {
            return Err(ModelError::TooLarge);
        }
        while body.len() < len {
            if read_some(stream, &mut body, deadline)? == 0 {
                return Err(ModelError::Malformed); // EOF before advertised length
            }
        }
        body.truncate(len); // Connection: close — anything past len is noise
        Ok(body)
    } else {
        // No framing at all: Connection: close delimits the body.
        loop {
            if body.len() > max_body {
                return Err(ModelError::TooLarge);
            }
            if read_some(stream, &mut body, deadline)? == 0 {
                return Ok(body);
            }
        }
    }
}

/// Parse the status line and the two headers that matter for framing.
fn parse_head(head: &[u8]) -> Result<(u16, bool, Option<usize>), ModelError> {
    let text = std::str::from_utf8(head).map_err(|_| ModelError::Malformed)?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or(""); // split yields ≥1 item; "" fails below
    let mut parts = status_line.split_ascii_whitespace();
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(ModelError::Malformed);
    }
    let status: u16 = parts
        .next()
        .unwrap_or("")
        .parse()
        .map_err(|_| ModelError::Malformed)?;

    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "transfer-encoding" => {
                chunked = value.trim().to_ascii_lowercase().contains("chunked");
            }
            "content-length" => {
                content_length = Some(value.trim().parse().map_err(|_| ModelError::Malformed)?);
            }
            _ => {}
        }
    }
    Ok((status, chunked, content_length))
}

/// Try to decode a complete chunked body from `raw`. Ok(None) = need more
/// bytes. Re-decodes from scratch after each read — quadratic in the worst
/// case, but the input is capped and reads arrive in large pieces, so the
/// simplicity wins. Trailers after the terminal chunk are ignored.
fn decode_chunked(raw: &[u8], max_body: usize) -> Result<Option<Vec<u8>>, ModelError> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let Some(line_end) = find(&raw[pos..], b"\r\n") else {
            return Ok(None);
        };
        let line =
            std::str::from_utf8(&raw[pos..pos + line_end]).map_err(|_| ModelError::Malformed)?;
        let size_hex = line.split(';').next().unwrap_or("").trim(); // chunk extensions dropped
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| ModelError::Malformed)?;
        if size == 0 {
            return Ok(Some(out));
        }
        if size > max_body || out.len() + size > max_body {
            return Err(ModelError::TooLarge);
        }
        let data = pos + line_end + 2;
        if raw.len() < data + size + 2 {
            return Ok(None);
        }
        if &raw[data + size..data + size + 2] != b"\r\n" {
            return Err(ModelError::Malformed);
        }
        out.extend_from_slice(&raw[data..data + size]);
        pos = data + size + 2;
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Mock-server helpers shared by this module's tests and the inference
    //! layer's: every test runs against a real TCP peer on 127.0.0.1, so
    //! the test *is* the probe — the failure mode is produced on a live
    //! socket and watched, not simulated.

    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// One-shot server: accept one connection, run `f` on it.
    pub(crate) fn serve_with<F: FnOnce(TcpStream) + Send + 'static>(f: F) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                f(s);
            }
        });
        addr
    }

    /// One-shot server that reads the full request, then writes `response`
    /// (in `parts` writes with a small gap, to exercise incremental reads).
    pub(crate) fn serve(response: &[u8], parts: usize) -> SocketAddr {
        let response = response.to_vec();
        serve_with(move |mut s| {
            read_full_request(&mut s);
            let step = response.len().div_ceil(parts);
            for piece in response.chunks(step.max(1)) {
                s.write_all(piece).unwrap();
                s.flush().unwrap();
                if parts > 1 {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        })
    }

    /// Read our client's request: head, then exactly Content-Length bytes.
    pub(crate) fn read_full_request(s: &mut TcpStream) {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        let head_end = loop {
            if let Some(p) = find(&got, b"\r\n\r\n") {
                break p;
            }
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "client closed before finishing its request");
            got.extend_from_slice(&buf[..n]);
        };
        let head = String::from_utf8(got[..head_end].to_vec()).unwrap();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        while got.len() < head_end + 4 + len {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
    }

    pub(crate) fn soon() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn content_length_body_returned() {
        let addr = serve(
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
            1,
        );
        let body = post_json(addr, "/api/show", b"{}", soon(), 1024).unwrap();
        assert_eq!(body, b"{\"ok\":true}");
    }

    #[test]
    fn chunked_body_across_split_writes_is_reassembled() {
        // Uppercase "Chunked" on purpose: header values are case-insensitive.
        let addr = serve(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n\
              6;ext=1\r\n{\"ok\":\r\n5\r\ntrue}\r\n0\r\n\r\n",
            4,
        );
        let body = post_json(addr, "/api/chat", b"{}", soon(), 1024).unwrap();
        assert_eq!(body, b"{\"ok\":true}");
    }

    #[test]
    fn eof_framed_body_returned() {
        let addr = serve(b"HTTP/1.1 200 OK\r\n\r\nplain", 1);
        let body = post_json(addr, "/", b"{}", soon(), 1024).unwrap();
        assert_eq!(body, b"plain");
    }

    #[test]
    fn daemon_down_is_connect() {
        // Bind then drop: the port was just now closed, so nothing listens.
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert_eq!(
            post_json(addr, "/", b"{}", soon(), 1024).unwrap_err(),
            ModelError::Connect
        );
    }

    #[test]
    fn silent_server_hits_deadline_not_hang() {
        let addr = serve_with(|mut s| {
            read_full_request(&mut s);
            thread::sleep(Duration::from_millis(500)); // never answers
        });
        let start = Instant::now();
        let err = post_json(
            addr,
            "/",
            b"{}",
            Instant::now() + Duration::from_millis(80),
            1024,
        )
        .unwrap_err();
        assert_eq!(err, ModelError::Timeout);
        // Well before the server's 500 ms nap: the deadline cut us loose.
        assert!(start.elapsed() < Duration::from_millis(300));
    }

    #[test]
    fn trickling_server_cannot_evade_the_overall_deadline() {
        // Each 20 ms drip resets a naive per-read timeout, so only the
        // recomputed remaining-time enforcement catches this. Probed
        // 2026-08-06: with read_some pinned to a fixed 1 s per-read timeout
        // instead of remaining(), the client rode out the server's whole
        // 30-drip run (~0.6 s against a 100 ms deadline) and failed with
        // Malformed at EOF instead of Timeout.
        let addr = serve_with(|mut s| {
            read_full_request(&mut s);
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n")
                .unwrap();
            for _ in 0..30 {
                if s.write_all(b"x").is_err() {
                    return; // client gave up — exactly what we want
                }
                let _ = s.flush();
                thread::sleep(Duration::from_millis(20));
            }
        });
        let start = Instant::now();
        let err = post_json(
            addr,
            "/",
            b"{}",
            Instant::now() + Duration::from_millis(100),
            2048,
        )
        .unwrap_err();
        assert_eq!(err, ModelError::Timeout);
        assert!(start.elapsed() < Duration::from_millis(300));
    }

    #[test]
    fn oversized_content_length_rejected_before_reading_body() {
        let addr = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 999999\r\n\r\nx", 1);
        assert_eq!(
            post_json(addr, "/", b"{}", soon(), 1024).unwrap_err(),
            ModelError::TooLarge
        );
    }

    #[test]
    fn oversized_eof_framed_body_rejected() {
        let mut resp = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        resp.extend(std::iter::repeat_n(b'x', 5000));
        let addr = serve(&resp, 1);
        assert_eq!(
            post_json(addr, "/", b"{}", soon(), 1024).unwrap_err(),
            ModelError::TooLarge
        );
    }

    #[test]
    fn oversized_chunk_rejected() {
        let addr = serve(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffff\r\n",
            1,
        );
        assert_eq!(
            post_json(addr, "/", b"{}", soon(), 1024).unwrap_err(),
            ModelError::TooLarge
        );
    }

    #[test]
    fn non_2xx_is_status() {
        let addr = serve(b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\nno", 1);
        assert_eq!(
            post_json(addr, "/api/show", b"{}", soon(), 1024).unwrap_err(),
            ModelError::Status(404)
        );
    }

    #[test]
    fn garbage_status_line_is_malformed() {
        let addr = serve(b"WAT 200\r\n\r\n", 1);
        assert_eq!(
            post_json(addr, "/", b"{}", soon(), 1024).unwrap_err(),
            ModelError::Malformed
        );
    }

    #[test]
    fn eof_before_head_is_malformed() {
        let addr = serve(b"HTTP/1.1 200", 1); // closes mid-status-line
        assert_eq!(
            post_json(addr, "/", b"{}", soon(), 1024).unwrap_err(),
            ModelError::Malformed
        );
    }

    #[test]
    fn eof_mid_chunk_is_malformed() {
        let addr = serve(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
            1,
        );
        assert_eq!(
            post_json(addr, "/", b"{}", soon(), 1024).unwrap_err(),
            ModelError::Malformed
        );
    }

    #[test]
    fn eof_short_of_content_length_is_malformed() {
        let addr = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc", 1);
        assert_eq!(
            post_json(addr, "/", b"{}", soon(), 1024).unwrap_err(),
            ModelError::Malformed
        );
    }

    #[test]
    fn non_loopback_refused_without_io() {
        let addr: SocketAddr = "192.0.2.1:80".parse().unwrap(); // TEST-NET, never routable
        let start = Instant::now();
        assert_eq!(
            post_json(addr, "/", b"{}", soon(), 1024).unwrap_err(),
            ModelError::NotLoopback
        );
        assert!(start.elapsed() < Duration::from_millis(50)); // no connect attempt
    }

    // Peer verification (audit 2026-08-06). Every socket test in this file
    // already exercises the accept path live — the mock servers run as our
    // own uid, so a broken check would fail the whole suite. The refusal
    // side can't be produced without a second uid, so its logic is pinned
    // as pure functions on realistic /proc/net/tcp rows.

    #[test]
    fn peer_uid_parsed_from_realistic_proc_net_tcp() {
        // Real row shape — uid is field 7, AFTER retrnsmt ("00000000"),
        // which also parses as a number: an off-by-one here reads uid 0 and
        // trusts everyone. This fixture (uid 1027) pins the exact column.
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
             0: 0100007F:2CAB 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1027        0 12345 1 0000000000000000 100 0 0 10 0\n\
             1: 0100007F:2CAB 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1027        0 12346 1 0000000000000000 20 4 30 10 -1\n";
        assert_eq!(find_peer_uid(table, 0x2CAB, 0xC350), Some(1027));
        // The listener row (rem 0.0.0.0:0) must not match our connection.
        assert_eq!(find_peer_uid(table, 0x2CAB, 0x1234), None);
        assert_eq!(find_peer_uid("", 0x2CAB, 0xC350), None);
    }

    #[test]
    fn peer_trust_policy() {
        // own uid: trusted
        assert!(peer_uid_trusted(1000, 1000));
        // root (Docker proxy) and system accounts (ollama service): trusted
        assert!(peer_uid_trusted(0, 1000));
        assert!(peer_uid_trusted(999, 1000));
        // another human user squatting the port: refused
        assert!(!peer_uid_trusted(1001, 1000));
        assert!(!peer_uid_trusted(1000, 1001));
    }

    #[test]
    fn same_uid_peer_is_accepted_live() {
        // The full verify_peer path against a real socket owned by us —
        // the positive half of the audit fix, on a live /proc/net/tcp.
        let addr = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok", 1);
        let body = post_json(addr, "/", b"{}", soon(), 1024).unwrap();
        assert_eq!(body, b"ok");
    }

    // decode_chunked edges not reachable through a socket round-trip:

    #[test]
    fn chunk_decoder_incomplete_returns_none() {
        assert_eq!(decode_chunked(b"5\r\nab", 1024).unwrap(), None);
        assert_eq!(decode_chunked(b"5", 1024).unwrap(), None);
        assert_eq!(decode_chunked(b"", 1024).unwrap(), None);
    }

    #[test]
    fn chunk_decoder_bad_hex_is_malformed() {
        assert_eq!(
            decode_chunked(b"xyz\r\nab\r\n", 1024).unwrap_err(),
            ModelError::Malformed
        );
    }

    #[test]
    fn chunk_decoder_missing_crlf_after_data_is_malformed() {
        assert_eq!(
            decode_chunked(b"2\r\nabXX0\r\n\r\n", 1024).unwrap_err(),
            ModelError::Malformed
        );
    }

    #[test]
    fn chunk_decoder_uppercase_hex_ok() {
        let decoded = decode_chunked(b"A\r\n0123456789\r\n0\r\n\r\n", 1024)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, b"0123456789");
    }
}
