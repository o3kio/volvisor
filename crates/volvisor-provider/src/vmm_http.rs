//! Minimal HTTP/1.1 `PUT` client over a Unix domain socket (P6-B,
//! ADR-0006 first slice part 1): the transport behind
//! [`crate::vmm::VmmController::resize_disk`].
//!
//! # Why hand-rolled
//!
//! The verified fact (checked against cloud-hypervisor v37.0's
//! published command list): `ch-remote` has **no `resize-disk`
//! subcommand**. Disk resize is REST-API-only — `PUT
//! /api/v1/vm.resize-disk` over the VMM's `--api-socket` unix
//! socket, with the `VmResizeDisk` JSON body. The adapter object
//! ([`crate::vmm::ChRemoteVmm`]) carries the capability, but the
//! mechanism is this module's HTTP/1.1 exchange over the same socket
//! the `ch-remote` commands target — no new dependency, exactly the
//! request bytes the upstream schema pins.
//!
//! # The exchange
//!
//! One request, one response, `Connection: close`:
//!
//! ```text
//! PUT /api/v1/vm.resize-disk HTTP/1.1\r\n
//! Host: localhost\r\n
//! Content-Type: application/json\r\n
//! Content-Length: <n>\r\n
//! Connection: close\r\n
//! \r\n
//! <body>
//! ```
//!
//! The status line is parsed (`HTTP/1.x <code> <reason>`); this
//! module is the transport primitive and returns the parsed
//! response — the 2xx-is-success rule belongs to the caller
//! ([`crate::vmm::VmmController::resize_disk`] treats 2xx — 204 No
//! Content expected — as success and maps anything else to a typed
//! error carrying the status).
//!
//! # Bounds
//!
//! Every phase is bounded; nothing blocks the caller indefinitely:
//!
//! - **connect**: a unix-domain `connect` can block only on a
//!   listener whose accept backlog is full (a wedged VMM — exactly
//!   the failure the notification retry must survive). It runs on a
//!   dedicated thread reported over a channel with a bounded
//!   receive, the [`crate::runner::RealRunner`] detached-reaper
//!   precedent: a bounded, documented thread leak instead of an
//!   unblocked caller.
//! - **write/read**: `set_read_timeout`/`set_write_timeout` bound
//!   the exchange; a response that never completes is a typed
//!   error, never a success.
//! - **size**: the response is read at most
//!   [`MAX_RESPONSE_BYTES`]; a server that streams forever cannot
//!   grow the reader unboundedly.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use volvisor_types::{ApiError, ApiErrorCode};

/// The response read cap: a misbehaving server cannot stream the
/// reader past this many bytes.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// The connect bound: how long the bounded-connect helper waits for
/// the connect thread before returning the typed timeout error.
pub const CONNECT_BOUND: Duration = Duration::from_secs(2);

/// One parsed HTTP response: the status line's code and reason, plus
/// everything after it (headers and any body), capped, as diagnostic
/// excerpt material — never parsed further here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmmHttpResponse {
    /// The status code (e.g. `204`).
    pub status: u16,
    /// The reason phrase (e.g. `No Content`); empty when absent.
    pub reason: String,
    /// Everything after the status line, capped at
    /// [`MAX_RESPONSE_BYTES`] and lossily decoded — diagnostic
    /// excerpt material only.
    pub rest: String,
}

/// Issue one `PUT <target>` with `body` (JSON) over `socket`, bounded
/// by `timeout` for the write and read phases (the connect phase is
/// bounded by [`CONNECT_BOUND`]).
///
/// The exchange terminates per HTTP/1.1, not per the peer's whim:
/// after the status line and headers, a 204/304 (no body by
/// definition), a `Content-Length: 0`, or a fully-read
/// `Content-Length` body completes it — the peer may hold the
/// connection open afterward (a VMM that does not rush its close is
/// not a transport failure). A response with neither a body-defining
/// status nor a `Content-Length` is close-delimited (the HTTP/1.1
/// rule) and still ends at the peer's close.
///
/// # Errors
/// [`ApiError`] typed `INTERNAL` for every transport failure (the
/// socket cannot be connected within the bound, the write or read
/// fails or times out, the response exceeds the size cap) and for an
/// unparseable status line. A **parsed** non-2xx status is a
/// successful transport and returns [`VmmHttpResponse`] — the status
/// interpretation is the caller's rule.
pub fn put_json(
    socket: &Path,
    target: &str,
    body: &str,
    timeout: Duration,
) -> Result<VmmHttpResponse, ApiError> {
    let internal = |detail: String| {
        ApiError::new(
            ApiErrorCode::Internal,
            format!("HTTP PUT {target} over {}: {detail}", socket.display()),
        )
    };
    let mut stream = connect_bounded(socket).map_err(internal)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| internal(format!("failed to set the read timeout: {e}")))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| internal(format!("failed to set the write timeout: {e}")))?;
    let request = format!(
        "PUT {target} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| internal(format!("failed to write the request: {e}")))?;
    stream
        .flush()
        .map_err(|e| internal(format!("failed to flush the request: {e}")))?;
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        if exchange_complete(&raw) {
            // The exchange is complete per HTTP/1.1 (the module
            // docs): success does not wait for the peer's close.
            break;
        }
        let read = stream
            .read(&mut chunk)
            .map_err(|e| internal(format!("failed to read the response: {e}")))?;
        if read == 0 {
            // `Connection: close`: the server ended the exchange.
            break;
        }
        if raw.len() + read > MAX_RESPONSE_BYTES {
            return Err(internal(format!(
                "the response exceeds the {MAX_RESPONSE_BYTES}-byte read cap"
            )));
        }
        raw.extend_from_slice(&chunk[..read]);
    }
    parse_status_line(&raw).map_err(internal)
}

/// Whether `raw` completes the HTTP/1.1 exchange: the status line,
/// the headers, and — per the status and `Content-Length` — the full
/// body. A 204/304 carries no body by definition; a
/// `Content-Length: 0` exchange ends at the headers; a
/// `Content-Length: N` body is complete at `N` bytes past the
/// header block. A response that is neither body-less by status nor
/// `Content-Length`-delimited is close-delimited (the HTTP/1.1
/// rule) and completes only at the peer's close — which the read
/// loop's `read == 0` handles. A malformed status line is not
/// decided here: the final [`parse_status_line`] fails typed on it.
/// All arithmetic is over bytes — the lossily-decoded text is used
/// only to read the (ASCII, in practice) header fields.
fn exchange_complete(raw: &[u8]) -> bool {
    let Some(header_end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let headers = String::from_utf8_lossy(&raw[..header_end]);
    let Some(status) = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
    else {
        return false;
    };
    if status == 204 || status == 304 {
        return true;
    }
    let length = headers.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())?
    });
    match length {
        Some(0) => true,
        Some(length) => raw.len() >= header_end + 4 + length,
        None => false,
    }
}

/// Connect to `socket`, bounded by [`CONNECT_BOUND`].
///
/// A unix-domain `connect` blocks only when the listener's accept
/// backlog is full (a wedged VMM). `UnixStream::connect` offers no
/// timeout variant, so the connect runs on a dedicated thread and is
/// awaited with a bounded channel receive; on timeout the typed error
/// returns immediately and the thread is leaked until its connect
/// resolves — a bounded, documented leak, the [`crate::runner::RealRunner`]
/// detached-reaper precedent.
fn connect_bounded(socket: &Path) -> Result<UnixStream, String> {
    let socket = socket.to_owned();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        // A closed receiver (the caller already timed out) is fine.
        let _ = sender.send(UnixStream::connect(&socket));
    });
    match receiver.recv_timeout(CONNECT_BOUND) {
        Ok(result) => result.map_err(|e| format!("failed to connect: {e}")),
        Err(_) => Err(format!(
            "the connect did not complete within {}s (a wedged listener?)",
            CONNECT_BOUND.as_secs_f64()
        )),
    }
}

/// Parse the response's status line (`HTTP/1.x <code> [reason]`),
/// returning the code, the reason and the (lossily decoded,
/// already-capped) remainder.
///
/// Fail-closed: a response that does not start with `HTTP/`, carries
/// no numeric status code, or has no line terminator at all is an
/// error, never a guess.
fn parse_status_line(raw: &[u8]) -> Result<VmmHttpResponse, String> {
    let text = String::from_utf8_lossy(raw);
    let (line, rest) = match text.find('\n') {
        Some(index) => (&text[..index], &text[index + 1..]),
        None => {
            return Err(format!(
                "the response carries no status line: {:?}",
                line_excerpt(raw)
            ));
        }
    };
    let line = line.trim_end_matches('\r');
    let mut parts = line.splitn(3, ' ');
    let protocol = parts.next().unwrap_or_default();
    if !protocol.starts_with("HTTP/") {
        return Err(format!(
            "the status line does not start with HTTP/: {line:?}"
        ));
    }
    let code = parts.next().unwrap_or_default();
    let status = code
        .parse::<u16>()
        .map_err(|_| format!("the status code {code:?} is not numeric"))?;
    let reason = parts.next().unwrap_or_default().to_owned();
    Ok(VmmHttpResponse {
        status,
        reason,
        rest: rest.to_owned(),
    })
}

/// A short excerpt of a raw response, for error details.
fn line_excerpt(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .chars()
        .take(120)
        .collect::<String>()
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::time::Instant;

    const TIMEOUT: Duration = Duration::from_secs(2);

    /// One accepted connection's full request bytes.
    struct Served {
        request: String,
    }

    /// Run a one-shot fake UDS HTTP server: accept one connection,
    /// read until the body's `Content-Length` bytes arrived, hand the
    /// exact request bytes to the test, send the scripted response
    /// bytes and close.
    fn serve_once(
        dir: &Path,
        name: &str,
        response: &str,
    ) -> (PathBuf, std::thread::JoinHandle<Served>) {
        let socket = dir.join(name);
        let listener = UnixListener::bind(&socket).expect("bind the fake api socket");
        let response = response.to_owned();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept one connection");
            let mut raw = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = stream.read(&mut chunk).expect("read the request");
                raw.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&raw).into_owned();
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let headers = &text[..header_end];
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            (name.eq_ignore_ascii_case("content-length"))
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or_default();
                    if raw.len() >= header_end + 4 + length {
                        break;
                    }
                }
                if read == 0 {
                    break;
                }
            }
            stream
                .write_all(response.as_bytes())
                .expect("write the scripted response");
            drop(stream);
            Served {
                request: String::from_utf8_lossy(&raw).into_owned(),
            }
        });
        (socket, handle)
    }

    /// Run a one-shot fake UDS HTTP server that answers `response`
    /// and then **never closes** the connection (the peer holds it
    /// open): the exchange must complete on the protocol's own
    /// terms, never on the close.
    fn serve_holding(
        dir: &Path,
        name: &str,
        response: &str,
    ) -> (PathBuf, std::thread::JoinHandle<Served>) {
        let socket = dir.join(name);
        let listener = UnixListener::bind(&socket).expect("bind the fake api socket");
        let response = response.to_owned();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept one connection");
            let mut raw = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = stream.read(&mut chunk).expect("read the request");
                raw.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&raw).into_owned();
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let headers = &text[..header_end];
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            (name.eq_ignore_ascii_case("content-length"))
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or_default();
                    if raw.len() >= header_end + 4 + length {
                        break;
                    }
                }
                if read == 0 {
                    break;
                }
            }
            stream
                .write_all(response.as_bytes())
                .expect("write the scripted response");
            // Deliberately never close: the write already delivered
            // the bytes; the peer's close must not be the
            // exchange's terminator.
            std::mem::forget(stream);
            Served {
                request: String::from_utf8_lossy(&raw).into_owned(),
            }
        });
        (socket, handle)
    }

    #[test]
    fn put_json_writes_the_exact_request_bytes_and_parses_204() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (socket, server) = serve_once(dir.path(), "ok.sock", "HTTP/1.1 204 No Content\r\n\r\n");
        let response = put_json(
            &socket,
            "/api/v1/vm.resize-disk",
            r#"{"id":"vol-1","new_size":2048}"#,
            TIMEOUT,
        )
        .expect("the exchange succeeds");
        assert_eq!(response.status, 204);
        assert_eq!(response.reason, "No Content");
        let served_request = server.join().expect("the server thread");
        assert_eq!(
            served_request.request,
            "PUT /api/v1/vm.resize-disk HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Type: application/json\r\n\
             Content-Length: 30\r\n\
             Connection: close\r\n\
             \r\n\
             {\"id\":\"vol-1\",\"new_size\":2048}"
        );
    }

    #[test]
    fn put_json_returns_the_parsed_status_of_a_non_2xx_response() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (socket, server) = serve_once(
            dir.path(),
            "refused.sock",
            "HTTP/1.1 500 Internal Server Error\r\n\r\n{\"error\":\"no such disk\"}",
        );
        // The transport succeeded: the status is parsed and returned
        // for the caller's 2xx rule — it is not this module's error.
        let response = put_json(&socket, "/api/v1/vm.resize-disk", "{}", TIMEOUT)
            .expect("a parsed 500 is a successful transport");
        assert_eq!(response.status, 500);
        assert_eq!(response.reason, "Internal Server Error");
        assert!(response.rest.contains("no such disk"));
        server.join().expect("the server thread");
    }

    #[test]
    fn put_json_fails_typed_on_a_malformed_status_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (socket, server) = serve_once(dir.path(), "garbage.sock", "not-http-at-all\r\n\r\n");
        let error = put_json(&socket, "/api/v1/vm.resize-disk", "{}", TIMEOUT)
            .expect_err("a malformed status line is a typed transport failure");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(error.detail.contains("status line"), "{error}");
        server.join().expect("the server thread");
    }

    #[test]
    fn put_json_fails_typed_when_nothing_listens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("nothing.sock");
        let error = put_json(&socket, "/api/v1/vm.resize-disk", "{}", TIMEOUT)
            .expect_err("a dead socket is a typed transport failure");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(error.detail.contains("failed to connect"), "{error}");
    }

    #[test]
    fn put_json_fails_typed_when_the_server_hangs_instead_of_closing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("hang.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        // Accept, read nothing, never respond, never close: the read
        // timeout must bound the exchange.
        let _hang = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            std::mem::forget(stream);
        });
        let started = Instant::now();
        let error = put_json(
            &socket,
            "/api/v1/vm.resize-disk",
            "{}",
            Duration::from_millis(200),
        )
        .expect_err("a hung exchange is a typed failure");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(error.detail.contains("failed to read"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the read timeout bounds the exchange"
        );
    }

    #[test]
    fn put_json_completes_on_a_content_length_body_without_waiting_for_the_close() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (socket, server) = serve_holding(
            dir.path(),
            "hold-body.sock",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        );
        let started = Instant::now();
        let response = put_json(&socket, "/api/v1/vm.resize-disk", "{}", TIMEOUT)
            .expect("a complete Content-Length exchange succeeds without the peer's close");
        assert_eq!(response.status, 200);
        assert!(response.rest.contains("ok"));
        assert!(
            started.elapsed() < TIMEOUT,
            "the exchange completed on the protocol's terms, not by waiting out the timeout"
        );
        server.join().expect("the server thread");
    }

    #[test]
    fn put_json_completes_on_204_without_waiting_for_the_close() {
        // The verified resize-disk success shape: a VMM that answers
        // 204 and then holds the connection open is not a transport
        // failure — 204 completes the exchange by definition.
        let dir = tempfile::tempdir().expect("tempdir");
        let (socket, server) = serve_holding(
            dir.path(),
            "hold-204.sock",
            "HTTP/1.1 204 No Content\r\n\r\n",
        );
        let response = put_json(&socket, "/api/v1/vm.resize-disk", "{}", TIMEOUT)
            .expect("204 completes without the close");
        assert_eq!(response.status, 204);
        server.join().expect("the server thread");
    }

    #[test]
    fn put_json_fails_typed_when_the_response_exceeds_the_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A response larger than MAX_RESPONSE_BYTES: headers plus a
        // body stream past the cap.
        let oversized = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            MAX_RESPONSE_BYTES + 1,
            "x".repeat(MAX_RESPONSE_BYTES + 1)
        );
        let (socket, server) = serve_once(dir.path(), "flood.sock", &oversized);
        let error = put_json(&socket, "/api/v1/vm.resize-disk", "{}", TIMEOUT)
            .expect_err("an unbounded stream is a typed failure");
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert!(error.detail.contains("read cap"), "{error}");
        server.join().expect("the server thread");
    }

    #[test]
    fn parse_status_line_accepts_http_1_0_and_missing_reason() {
        let parsed = parse_status_line(b"HTTP/1.0 200\r\nrest").expect("parses");
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.reason, "");
        assert_eq!(parsed.rest, "rest");
        for raw in [
            &b""[..],
            b"HTTP/1.1\r\n",
            b"HTTP/1.1 abc\r\n",
            b"HTTPX/1.1 200 OK\r\n",
        ] {
            assert!(
                parse_status_line(raw).is_err(),
                "a malformed line must fail typed: {:?}",
                String::from_utf8_lossy(raw)
            );
        }
    }
}
