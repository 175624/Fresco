//! Unix-socket control server. Runs on its own thread and forwards each
//! request to the main render loop via a channel, returning the reply.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use anyhow::{anyhow, Result};

use crate::ipc::{ensure_safe_socket_dir, socket_dir, socket_path, Request, Response};

/// A request paired with a channel to send its response back on.
pub type Command = (Request, Sender<Response>);

/// Hard cap on one control-socket request line, enforced via [`Read::take`]
/// rather than a bare `read_line` — which has no length limit of its own and
/// will happily keep buffering whatever a connected peer sends until it
/// finally sees a newline (or the peer's own send buffer runs out). Any local
/// user can connect to this socket and send a line, so an unbounded read here
/// is an unbounded-memory footgun handed to anyone on the box, not just a
/// theoretical concern. 64 KiB is generous for any real [`Request`] — the
/// largest, `LockNotify`, is a handful of short strings — so this never
/// clips a legitimate caller.
const MAX_REQUEST_LINE: usize = 64 * 1024;

/// Bind the control socket and spawn the accept loop.
///
/// Doubles as the single-instance lock: if an existing socket answers a
/// connection, another daemon is alive and we return an error.
pub fn start_server() -> Result<Receiver<Command>> {
    let dir = socket_dir();
    if let Err(e) = ensure_safe_socket_dir(&dir) {
        log::error!(
            "refusing to start the control socket in an unsafe directory ({}): {e:#}",
            dir.display()
        );
        return Err(e);
    }
    let path = socket_path();

    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err(anyhow!("another frescod instance is already running"));
        }
        // Stale socket from a crashed daemon — remove and rebind.
        std::fs::remove_file(&path).ok();
    }

    let listener = UnixListener::bind(&path)?;
    let (tx, rx) = channel::<Command>();

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            handle_conn(stream, &tx);
        }
    });

    Ok(rx)
}

/// Pure check: does a line this long (as `read_line` already read it,
/// including its trailing `\n` if it found one) exceed [`MAX_REQUEST_LINE`]?
/// Split out purely so the exact boundary is a plain, instant unit test
/// rather than something only checkable indirectly through a real socket
/// round-trip.
fn is_oversized_request_line(line_len: usize) -> bool {
    line_len > MAX_REQUEST_LINE
}

fn handle_conn(mut stream: UnixStream, tx: &Sender<Command>) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    // `+ 1`: a line of exactly `MAX_REQUEST_LINE` bytes (including its `\n`)
    // must still succeed; `Take` stopping the underlying reader right at the
    // cap would otherwise make that boundary line indistinguishable from a
    // genuinely oversized one that simply never contained a newline within
    // the allowed span. `is_oversized_request_line` is what actually draws
    // the line, using the length `read_line` reports either way.
    let read_result = reader
        .by_ref()
        .take(MAX_REQUEST_LINE as u64 + 1)
        .read_line(&mut line);
    if read_result.is_err() {
        return;
    }
    if is_oversized_request_line(line.len()) {
        send_response(
            &mut stream,
            &Response::Err {
                message: format!("request line too long (max {MAX_REQUEST_LINE} bytes)"),
            },
        );
        return;
    }
    let Ok(req) = serde_json::from_str::<Request>(line.trim()) else {
        return;
    };

    let (rtx, rrx) = channel::<Response>();
    if tx.send((req, rtx)).is_err() {
        return; // main loop gone
    }
    if let Ok(resp) = rrx.recv() {
        send_response(&mut stream, &resp);
    }
}

/// Write one JSON response line, ignoring write failures — the peer may
/// already be gone, and every other error path in this module already treats
/// that as nothing more to do.
fn send_response(stream: &mut UnixStream, resp: &Response) {
    if let Ok(mut s) = serde_json::to_string(resp) {
        s.push('\n');
        let _ = stream.write_all(s.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // -- is_oversized_request_line: exact boundary, no socket needed --------

    #[test]
    fn oversized_boundary_is_exact() {
        assert!(!is_oversized_request_line(MAX_REQUEST_LINE));
        assert!(is_oversized_request_line(MAX_REQUEST_LINE + 1));
    }

    // -- handle_conn: real UnixStream pairs, isolated temp-dir sockets -------

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fresco-control-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// One connected client/server `UnixStream` pair over a throwaway socket
    /// path, so each test drives `handle_conn` exactly as the real accept
    /// loop would, without touching the real (global) `socket_path()` — that
    /// path is shared with any real `frescod` on the machine and with every
    /// other test in this binary, so binding it here would be a race, not a
    /// test.
    fn connected_pair(dir: &std::path::Path) -> (UnixStream, UnixStream) {
        let path = dir.join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn oversized_request_line_is_rejected_with_an_err_response() {
        let dir = tempdir("oversize");
        let (client, server) = connected_pair(&dir);
        let (tx, _rx) = channel::<Command>(); // never reached: rejected before dispatch

        // Written from a background thread rather than inline: the payload
        // is comfortably bigger than some platforms' default socket buffer,
        // so writing it synchronously before `handle_conn` ever starts
        // reading could block forever. Reading concurrently on the main
        // thread is what a real accept-loop connection looks like anyway.
        let mut writer_end = client.try_clone().unwrap();
        let writer = thread::spawn(move || {
            // Well over the cap, and deliberately no newline anywhere in it —
            // `Take` must cut this off at the cap regardless.
            let payload = vec![b'a'; MAX_REQUEST_LINE + 100];
            let _ = writer_end.write_all(&payload);
        });

        handle_conn(server, &tx);
        writer.join().unwrap();

        let mut reply = String::new();
        BufReader::new(&client).read_line(&mut reply).unwrap();
        let resp: Response = serde_json::from_str(reply.trim()).unwrap();
        match resp {
            Response::Err { message } => assert!(message.contains("too long"), "{message}"),
            other => panic!("expected Response::Err, got {other:?}"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn well_formed_request_is_still_forwarded_and_answered() {
        let dir = tempdir("well-formed");
        let (mut client, server) = connected_pair(&dir);
        let (tx, rx) = channel::<Command>();

        // Stand-in for the main render loop this normally talks to.
        let responder = thread::spawn(move || {
            let (req, rtx) = rx.recv().expect("handle_conn must forward the request");
            assert_eq!(req, Request::Status);
            rtx.send(Response::Ok).unwrap();
        });

        client.write_all(b"{\"cmd\":\"status\"}\n").unwrap();
        handle_conn(server, &tx);

        let mut reply = String::new();
        BufReader::new(&client).read_line(&mut reply).unwrap();
        assert_eq!(reply.trim(), r#"{"result":"ok"}"#);

        responder.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_json_within_the_cap_is_silently_dropped_like_before() {
        // Unchanged pre-existing behaviour: a line that parses as neither
        // oversized nor valid JSON gets no reply at all, not an `Err` — this
        // guards against the size cap accidentally starting to answer a case
        // it was never meant to touch.
        let dir = tempdir("invalid-json");
        let (mut client, server) = connected_pair(&dir);
        let (tx, _rx) = channel::<Command>();

        client.write_all(b"not json at all\n").unwrap();
        handle_conn(server, &tx);

        client
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        let mut reply = String::new();
        let _ = BufReader::new(&client).read_line(&mut reply);
        assert!(
            reply.is_empty(),
            "expected no reply for invalid JSON, got {reply:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
