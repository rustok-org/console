//! Socket worker thread — the client's only owner of the UNIX stream.
//!
//! `AGENTS.md` forbids an async *runtime*, not threads. The approver's `approve`
//! is slow (Argon2 verify + sign + broadcast — seconds), so a single-threaded
//! blocking read in the MVU loop would freeze the UI (countdown, Ctrl-C) for the
//! whole broadcast window. Instead a dedicated worker owns a blocking
//! `std::os::unix::net::UnixStream` and does request→response; the MVU loop talks
//! to it over two `mpsc` channels and never blocks on the socket.
//!
//! One request is in flight per connection (protocol §1): the worker sends a line
//! and reads exactly one reply before taking the next [`Request`]. Ordering the
//! *intents* (one in flight, latest-wins polling) is the MVU layer's job; the
//! transport just serializes the exchange.
//!
//! Every failure surfaces as [`Reply::Fatal`] — a refused connect, a mid-session
//! EOF, or a malformed reply line all end the worker cleanly instead of panicking.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use zeroize::Zeroizing;

use crate::protocol::{
    self, AuthOutcome, GetOutcome, HelloOutcome, PROTO_VERSION, Summary, encode_request,
    parse_auth, parse_get, parse_hello, parse_list,
};

/// Informational client id sent in `hello` (the server does not validate it).
const CLIENT_ID: &str = concat!("rustok-console/", env!("CARGO_PKG_VERSION"));
/// A response line longer than this is refused — the transport mirror of the
/// server's own 64 KiB cap, so a runaway peer cannot exhaust client memory.
const MAX_LINE_BYTES: u64 = 64 * 1024;

/// A request the MVU layer asks the worker to send. `auth` carries its already
/// serialized line inside a [`Zeroizing`] buffer — the PIN never travels as a
/// plain `String` (built in the MVU layer, zeroized on drop here after the write).
pub enum Request {
    /// Pre-serialized `auth` line (PIN inside), zeroized after sending.
    Auth(Zeroizing<String>),
    /// Ask for the queue summaries.
    List,
    /// Ask for one item's card by id.
    Get(String),
}

/// A message from the worker to the MVU layer.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    /// Handshake accepted (sent once, right after connect).
    Hello {
        /// Informational server id.
        server: String,
    },
    /// Result of an `auth`.
    Auth(AuthOutcome),
    /// Result of a `list`.
    List(Vec<Summary>),
    /// Result of a `get`.
    Get(GetOutcome),
    /// The connection is finished and unusable — the worker has exited.
    Fatal(TransportError),
}

/// A transport- or protocol-level failure. Each ends the worker; the MVU layer
/// renders the matching message and exits with the right code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The socket could not be opened — the wallet is not running / not reachable.
    NotConnected,
    /// The connection dropped mid-session (EOF, broken pipe).
    ConnectionLost,
    /// A reply line was not valid protocol (malformed / unexpected code).
    Protocol(String),
    /// Fatal `hello` version mismatch — the client must upgrade. Carries the
    /// versions the server supports.
    UnsupportedProto(Vec<u32>),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => {
                write!(f, "wallet not running? could not open the approver socket")
            }
            Self::ConnectionLost => write!(f, "connection lost"),
            Self::Protocol(m) => write!(f, "protocol error: {m}"),
            Self::UnsupportedProto(v) => {
                write!(
                    f,
                    "server speaks an unsupported protocol; it supports {v:?} — upgrade the console"
                )
            }
        }
    }
}

impl std::error::Error for TransportError {}

/// The MVU layer's handle to the worker: send [`Request`]s, receive [`Reply`]s.
/// Dropping it ends the worker (see [`Transport::drop`]).
pub struct Transport {
    req_tx: Sender<Request>,
    reply_rx: Receiver<Reply>,
    worker: Option<JoinHandle<()>>,
    /// A clone of the socket kept solely so `Drop` can `shutdown` it and unblock a
    /// worker parked in a blocking read. `None` if the connect failed.
    shutdown: Option<UnixStream>,
}

impl Transport {
    /// Connect to the approver socket at `path` and start the worker. The worker
    /// performs the `hello` handshake immediately; its first [`Reply`] is either
    /// [`Reply::Hello`] or a [`Reply::Fatal`]. The connect itself is synchronous —
    /// a local UNIX connect does not block on a handshake — so a clone of the
    /// stream can be kept for [`Transport::drop`] to shut down; everything after
    /// (handshake, requests) runs on the worker thread.
    #[must_use]
    pub fn connect(path: impl AsRef<Path>) -> Self {
        let (req_tx, req_rx) = channel::<Request>();
        let (reply_tx, reply_rx) = channel::<Reply>();

        let (worker, shutdown) = match UnixStream::connect(path.as_ref()) {
            Ok(stream) => {
                // Closing the request channel does NOT interrupt a blocking
                // read_line in the worker; a shutdown on this clone does.
                let shutdown = stream.try_clone().ok();
                let worker = std::thread::spawn(move || worker_loop(stream, &req_rx, &reply_tx));
                (Some(worker), shutdown)
            }
            Err(_) => {
                let _ = reply_tx.send(Reply::Fatal(TransportError::NotConnected));
                (None, None)
            }
        };
        Self {
            req_tx,
            reply_rx,
            worker,
            shutdown,
        }
    }

    /// Queue a request for the worker to send. Returns `false` if the worker has
    /// exited (connection finished) — the caller then reads the final [`Reply`].
    pub fn send(&self, req: Request) -> bool {
        self.req_tx.send(req).is_ok()
    }

    /// Non-blocking poll for a reply. `None` means "nothing yet".
    #[must_use]
    pub fn try_recv(&self) -> Option<Reply> {
        self.reply_rx.try_recv().ok()
    }

    /// Blocking receive of the next reply (used in tests and the connect handshake).
    #[must_use]
    pub fn recv(&self) -> Option<Reply> {
        self.reply_rx.recv().ok()
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // Close the request channel (ends the worker's idle `recv`) AND shut the
        // socket down. Closing the channel alone does NOT interrupt a worker parked
        // in a blocking read — a stalled server would then hang `join` forever; the
        // shutdown unblocks that read. Then join so the thread and its socket are
        // gone before we return.
        drop(std::mem::replace(&mut self.req_tx, channel().0));
        if let Some(stream) = &self.shutdown {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The worker body: connect, handshake, then serve one request at a time until
/// the request channel closes or the connection fails.
fn worker_loop(stream: UnixStream, req_rx: &Receiver<Request>, reply_tx: &Sender<Reply>) {
    let Ok(write_half) = stream.try_clone() else {
        let _ = reply_tx.send(Reply::Fatal(TransportError::ConnectionLost));
        return;
    };
    let mut writer = write_half;
    let mut reader = BufReader::new(stream);

    // Handshake first. A fatal outcome ends the worker.
    match handshake(&mut writer, &mut reader) {
        Ok(server) => {
            if reply_tx.send(Reply::Hello { server }).is_err() {
                return; // MVU gone already
            }
        }
        Err(fatal) => {
            let _ = reply_tx.send(Reply::Fatal(fatal));
            return;
        }
    }

    // Serve requests until the MVU layer drops the sender or the link breaks.
    while let Ok(req) = req_rx.recv() {
        let reply = match serve_one(&mut writer, &mut reader, &req) {
            Ok(reply) => reply,
            Err(fatal) => {
                let _ = reply_tx.send(Reply::Fatal(fatal));
                return; // connection is unusable — stop
            }
        };
        if reply_tx.send(reply).is_err() {
            return;
        }
    }
}

/// Send `hello` and interpret the reply. Returns the server id on success.
fn handshake(
    writer: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
) -> Result<String, TransportError> {
    let line = encode_request(&protocol::Request::Hello {
        proto: PROTO_VERSION,
        client: CLIENT_ID,
    })
    .map_err(|e| TransportError::Protocol(e.to_string()))?;
    let resp = exchange(writer, reader, &line)?;
    match parse_hello(&resp).map_err(|e| TransportError::Protocol(e.to_string()))? {
        HelloOutcome::Ok { server } => Ok(server),
        HelloOutcome::Unsupported { supported } => Err(TransportError::UnsupportedProto(supported)),
    }
}

/// Send one request and parse its reply. A broken connection or a malformed /
/// unexpected reply line is an `Err` (fatal — the exchange is out of sync and the
/// worker stops); expected error codes are already `Ok` variants (e.g. `get`'s
/// `unknown_id` → [`GetOutcome::UnknownId`]).
fn serve_one(
    writer: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    req: &Request,
) -> Result<Reply, TransportError> {
    let resp = match req {
        Request::Auth(line) => exchange(writer, reader, line)?,
        Request::List => {
            let line = encode_request(&protocol::Request::List)
                .map_err(|e| TransportError::Protocol(e.to_string()))?;
            exchange(writer, reader, &line)?
        }
        Request::Get(id) => {
            let line = encode_request(&protocol::Request::Get { id })
                .map_err(|e| TransportError::Protocol(e.to_string()))?;
            exchange(writer, reader, &line)?
        }
    };
    let parsed = match req {
        Request::Auth(_) => parse_auth(&resp).map(Reply::Auth),
        Request::List => parse_list(&resp).map(Reply::List),
        Request::Get(_) => parse_get(&resp).map(Reply::Get),
    };
    parsed.map_err(|e| TransportError::Protocol(e.to_string()))
}

/// Write one request line (adding the `\n`) and read exactly one reply line. An
/// EOF or I/O error is a lost connection; an over-long reply is a protocol error.
fn exchange(
    writer: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    line: &str,
) -> Result<String, TransportError> {
    writer
        .write_all(line.as_bytes())
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .map_err(|_| TransportError::ConnectionLost)?;

    let mut resp = String::new();
    // Reborrow so `take` (which consumes its reader) does not move the `&mut`
    // param; the cap mirrors the server's own oversize guard.
    let read = (&mut *reader)
        .take(MAX_LINE_BYTES + 1)
        .read_line(&mut resp)
        .map_err(|_| TransportError::ConnectionLost)?;
    if read == 0 {
        return Err(TransportError::ConnectionLost); // peer closed
    }
    if resp.len() as u64 > MAX_LINE_BYTES {
        return Err(TransportError::Protocol("oversize reply line".to_owned()));
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    /// A scripted fake approver: binds a temp socket, accepts one connection, and
    /// for each request line it reads, writes back the next scripted reply. When
    /// the script runs out it either closes (default) or keeps the connection open.
    struct FakeServer {
        path: PathBuf,
        handle: Option<JoinHandle<()>>,
    }

    impl FakeServer {
        /// Each script step reads one request line, then acts: `Some(line)` writes
        /// that reply; `None` reads the request but sends nothing (then the script
        /// ends and the connection drops) — this isolates the worker's read-side
        /// EOF path from a write-side broken pipe.
        fn start(tag: &str, replies: Vec<Option<&'static str>>) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rustok_console_t1a_{}_{tag}.sock",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).expect("bind fake socket");
            let handle = std::thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept");
                let mut writer = stream.try_clone().expect("clone");
                let mut reader = BufReader::new(stream);
                for reply in replies {
                    let mut line = String::new();
                    // Block until the worker sends a request line (deterministic —
                    // no sleep). EOF means the worker went away first.
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if let Some(reply) = reply {
                        let _ = writer.write_all(reply.as_bytes());
                        let _ = writer.write_all(b"\n");
                        let _ = writer.flush();
                    }
                }
                // Script exhausted: drop the connection so the next request sees EOF.
            });
            Self {
                path,
                handle: Some(handle),
            }
        }
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn connect_refused_when_no_socket() {
        let path = std::env::temp_dir().join(format!(
            "rustok_console_t1a_{}_nosock.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let t = Transport::connect(&path);
        assert_eq!(
            t.recv(),
            Some(Reply::Fatal(TransportError::NotConnected)),
            "a missing socket is a clean NotConnected, not a panic"
        );
    }

    #[test]
    fn handshake_then_list_then_get() {
        let server = FakeServer::start(
            "happy",
            vec![
                Some(r#"{"ok":true,"proto":1,"server":"core-server/0.1.0"}"#),
                Some(
                    r#"{"ok":true,"pending":[{"id":"a1","kind":"send","chain_id":1,"to":"0xabc","amount_wei":"1000","risk":"safe","high_risk":false,"not_after_unix":1}]}"#,
                ),
                Some(
                    r#"{"ok":true,"card":{"id":"a1","chain_id":1,"to":"0xabc","amount_wei":"1000","decoded_call":null,"high_risk":false,"high_risk_reasons":[],"raw_data":"0x","not_after_unix":1}}"#,
                ),
            ],
        );
        let t = Transport::connect(&server.path);
        assert_eq!(
            t.recv(),
            Some(Reply::Hello {
                server: "core-server/0.1.0".to_owned()
            })
        );

        assert!(t.send(Request::List));
        let Some(Reply::List(summaries)) = t.recv() else {
            panic!("expected a list reply");
        };
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "a1");

        assert!(t.send(Request::Get("a1".to_owned())));
        let Some(Reply::Get(GetOutcome::Card(card))) = t.recv() else {
            panic!("expected a card reply");
        };
        assert_eq!(card.raw_data, "0x");
    }

    #[test]
    fn auth_line_is_sent_and_result_parsed() {
        let server = FakeServer::start(
            "auth",
            vec![
                Some(r#"{"ok":true,"proto":1,"server":"core-server/0.1.0"}"#),
                Some(r#"{"ok":false,"error":"bad_pin","attempts_left":2}"#),
            ],
        );
        let t = Transport::connect(&server.path);
        assert!(matches!(t.recv(), Some(Reply::Hello { .. })));

        // The MVU layer builds this Zeroizing line (PIN inside); the transport
        // just sends it.
        let auth = Zeroizing::new(r#"{"op":"auth","pin":"000000"}"#.to_owned());
        assert!(t.send(Request::Auth(auth)));
        assert_eq!(
            t.recv(),
            Some(Reply::Auth(AuthOutcome::BadPin { attempts_left: 2 }))
        );
    }

    #[test]
    fn unsupported_proto_at_handshake_is_fatal() {
        let server = FakeServer::start(
            "badproto",
            vec![Some(
                r#"{"ok":false,"error":"unsupported_proto","supported":[2]}"#,
            )],
        );
        let t = Transport::connect(&server.path);
        assert_eq!(
            t.recv(),
            Some(Reply::Fatal(TransportError::UnsupportedProto(vec![2])))
        );
    }

    #[test]
    fn mid_session_eof_is_connection_lost() {
        // Server answers hello, then closes (script exhausted) → the next request
        // hits a broken write / EOF.
        let server = FakeServer::start(
            "eof",
            vec![Some(
                r#"{"ok":true,"proto":1,"server":"core-server/0.1.0"}"#,
            )],
        );
        let t = Transport::connect(&server.path);
        assert!(matches!(t.recv(), Some(Reply::Hello { .. })));
        assert!(t.send(Request::List));
        assert_eq!(t.recv(), Some(Reply::Fatal(TransportError::ConnectionLost)));
    }

    #[test]
    fn server_reads_request_then_closes_without_replying_is_connection_lost() {
        // The write succeeds (the server reads the line) but no reply comes and the
        // connection drops → the read-side EOF guard, not a broken pipe, is what
        // yields ConnectionLost. Isolates the `read == 0` path.
        let server = FakeServer::start(
            "read_then_close",
            vec![
                Some(r#"{"ok":true,"proto":1,"server":"core-server/0.1.0"}"#),
                None, // reads the `list` request, sends nothing, then drops
            ],
        );
        let t = Transport::connect(&server.path);
        assert!(matches!(t.recv(), Some(Reply::Hello { .. })));
        assert!(t.send(Request::List));
        assert_eq!(t.recv(), Some(Reply::Fatal(TransportError::ConnectionLost)));
    }

    #[test]
    fn a_garbled_reply_line_is_a_protocol_error_not_a_panic() {
        let server = FakeServer::start(
            "garbled",
            vec![
                Some(r#"{"ok":true,"proto":1,"server":"core-server/0.1.0"}"#),
                Some("this is not json"),
            ],
        );
        let t = Transport::connect(&server.path);
        assert!(matches!(t.recv(), Some(Reply::Hello { .. })));
        assert!(t.send(Request::List));
        assert!(matches!(
            t.recv(),
            Some(Reply::Fatal(TransportError::Protocol(_)))
        ));
    }

    #[test]
    fn garbled_handshake_is_a_protocol_error() {
        let server = FakeServer::start("badhello", vec![Some("not even json")]);
        let t = Transport::connect(&server.path);
        assert!(matches!(
            t.recv(),
            Some(Reply::Fatal(TransportError::Protocol(_)))
        ));
    }

    /// Accepts and reads the hello, signals `ready`, then blocks on a second read —
    /// holding the connection open without ever replying. Only a client-side
    /// shutdown ends it.
    struct StallServer {
        path: PathBuf,
        handle: Option<JoinHandle<()>>,
        ready: Receiver<()>,
    }

    impl StallServer {
        fn start(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rustok_console_t1a_{}_{tag}.sock",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).expect("bind stall socket");
            let (ready_tx, ready) = channel();
            let handle = std::thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line); // read the hello request
                let _ = ready_tx.send(()); // the worker is now parked on the reply read
                let mut sink = String::new();
                let _ = reader.read_line(&mut sink); // block until the client shuts down
            });
            Self {
                path,
                handle: Some(handle),
                ready,
            }
        }

        fn wait_ready(&self) {
            let _ = self.ready.recv();
        }
    }

    impl Drop for StallServer {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn drop_does_not_hang_when_the_server_stalls() {
        // The server reads the hello but never replies, so the worker is parked in
        // a blocking read. Dropping the Transport must shut the socket down and
        // return — not hang on join. Red without the Drop shutdown (times out).
        let server = StallServer::start("stall");
        let t = Transport::connect(&server.path);
        server.wait_ready();

        let (done_tx, done_rx) = channel();
        std::thread::spawn(move || {
            drop(t);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_ok(),
            "Transport::drop must shut the socket down and return, not hang"
        );
    }
}
