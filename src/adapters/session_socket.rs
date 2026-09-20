//! Pushing one line into a running Claude Code session, over its messaging
//! socket.
//!
//! Claude Code exports `CLAUDE_CODE_MESSAGING_SOCKET` and
//! `CLAUDE_CODE_MESSAGING_TOKEN` to a session's hooks and Bash commands. A
//! client connects, writes newline-delimited JSON — an optional `auth` line,
//! then a `user` message — and closes. The token is what marks the message as
//! the session's own, since a detached sender isn't the session's child.
//!
//! Two properties shape everything here. The socket never replies, so a write
//! that succeeds means the bytes reached a live socket and nothing more; and
//! the server drops a connection that hasn't sent a complete line within 30
//! seconds, so the payload is built before anything is opened.
//!
//! The wire format was read off the CLI's debug log (verified against Claude
//! Code 2.1.278), not a published contract — which is the whole reason it is
//! quarantined behind one small adapter. Unix only: the Windows transport is a
//! named pipe and is not supported.

use crate::secret::Secret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A hung session must not hold up the poller that found the news.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// Where a session listens, and the token that makes a message its own.
///
/// Stored as well as used: a subscription outlives the process that made it,
/// so this is what `watches.json` keeps. One type rather than a live one and
/// a persisted one — there is nothing in an address that doesn't survive a
/// write. `Debug` stays redacted through `Secret`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAddress {
    path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<Secret>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<SocketIdentity>,
}

/// Which socket was at the path when the subscription was made.
///
/// Paths are `<dir>/<pid>.sock` and the token is not a gate — a message sent
/// with the wrong token is still delivered. So a pid reused by a later Claude
/// Code session makes a stored address point at a stranger's inbox, and
/// ownership cannot tell them apart: same user, same path, different session.
/// This is the only thing that can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocketIdentity {
    dev: u64,
    ino: u64,
    /// Inode change time, and the reason dev+ino alone won't do: inode
    /// numbers get handed back out, and a rebound path is a new file rather
    /// than a mutation of the old one. Nanosecond-resolution on APFS and
    /// ext4, so two sockets at one path differ here even if `ino` repeats.
    ///
    /// It is change time, not birth time — `st_birthtime` isn't portable to
    /// Linux — so a chmod on a live socket would read as a different session
    /// and cost that session its push. Nothing does that to a bound socket.
    ctime: i64,
    ctime_nsec: i64,
}

impl SocketIdentity {
    /// Capture at subscribe time, while the session is known to be the one
    /// that asked.
    pub fn capture(path: &Path) -> io::Result<Self> {
        Ok(Self::of(&std::fs::symlink_metadata(path)?))
    }

    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
}

/// The payload reached a live socket. Deliberately not `()`: there is no ack
/// on this protocol, so this is as far as any claim can honestly go — written,
/// not read, and certainly not acted on.
#[derive(Debug, PartialEq, Eq)]
pub struct Written;

#[derive(Debug, thiserror::Error)]
pub enum DeliverError {
    /// The session this address named is not there — it exited, or something
    /// else now holds the path. Drop the address.
    #[error("no session at {path}: {reason}")]
    Gone { path: PathBuf, reason: String },
    /// The path is not something we will write a token to. Permanent in the
    /// same way `Gone` is — a socket does not become ours later.
    #[error("refusing to use {path}: {reason}")]
    Unsafe { path: PathBuf, reason: String },
    /// Busy, slow, or our own fault. The address may still be good.
    #[error("delivering to {path}: {source}")]
    Transient {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl DeliverError {
    /// Whether the caller should forget this address. The distinction is the
    /// point of this adapter: a pull feed is the guaranteed path, so dropping
    /// a live address costs a push, and keeping a dead one costs a poll.
    pub fn drop_address(&self) -> bool {
        !matches!(self, Self::Transient { .. })
    }
}

impl SessionAddress {
    pub fn new(path: impl Into<PathBuf>, token: Option<Secret>) -> Self {
        Self {
            path: path.into(),
            token,
            identity: None,
        }
    }

    /// Pin the address to the socket that was there when it was captured.
    /// Without this, delivery is to whoever holds the path now.
    pub fn expecting(mut self, identity: SocketIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn deliver(&self, text: &str) -> Result<Written, DeliverError> {
        // Built first: the server times out a connection that hasn't produced
        // a complete line, so nothing is opened until there is one to send.
        let payload = self.payload(text);

        self.vet_path()?;
        let mut stream = self.connect()?;
        stream
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .map_err(|source| self.classify(source))?;
        stream
            .write_all(payload.as_bytes())
            .map_err(|source| self.classify(source))?;
        stream.flush().map_err(|source| self.classify(source))?;
        Ok(Written)
    }

    /// Connect, with a deadline.
    ///
    /// Hand-rolled because `connect_timeout` is TCP-only, and a plain AF_UNIX
    /// connect is not bounded: on Linux it blocks until the listener's backlog
    /// drains, which is precisely the wedged session that must not take the
    /// poller with it. (BSD returns ECONNREFUSED instead, so macOS never sees
    /// this.) Non-blocking connect, then poll for writability.
    fn connect(&self) -> Result<UnixStream, DeliverError> {
        let address = self.sockaddr()?;
        // Owned immediately, so every path below closes the descriptor.
        let socket = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if socket < 0 {
            return Err(self.classify(io::Error::last_os_error()));
        }
        let stream = unsafe { UnixStream::from(OwnedFd::from_raw_fd(socket)) };
        stream
            .set_nonblocking(true)
            .map_err(|source| self.classify(source))?;

        let connected = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                &address as *const libc::sockaddr_un as *const libc::sockaddr,
                size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if connected < 0 {
            let error = io::Error::last_os_error();
            // Anything but "in progress" is the real answer already.
            if error.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(self.classify(error));
            }
            self.await_connection(&stream)?;
        }

        // Back to blocking, so the write timeout is what bounds the write.
        stream
            .set_nonblocking(false)
            .map_err(|source| self.classify(source))?;
        Ok(stream)
    }

    /// Wait for the in-flight connect to settle, and report how it settled —
    /// `poll` only says the socket is writable, `SO_ERROR` says whether that
    /// means connected or refused.
    fn await_connection(&self, stream: &UnixStream) -> Result<(), DeliverError> {
        let mut poll_fd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready =
            unsafe { libc::poll(&mut poll_fd, 1, CONNECT_TIMEOUT.as_millis() as libc::c_int) };
        match ready {
            0 => Err(self.classify(io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))),
            n if n < 0 => Err(self.classify(io::Error::last_os_error())),
            _ => match stream.take_error() {
                Ok(None) => Ok(()),
                Ok(Some(source)) | Err(source) => Err(self.classify(source)),
            },
        }
    }

    fn sockaddr(&self) -> Result<libc::sockaddr_un, DeliverError> {
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = self.path.as_os_str().as_bytes();
        // sun_path is 104 bytes on macOS, 108 on Linux, and needs its
        // terminator. A path that doesn't fit is never connectable.
        if bytes.len() >= address.sun_path.len() {
            return Err(self.refuse("path too long for a unix socket"));
        }
        for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
            *slot = *byte as libc::c_char;
        }
        Ok(address)
    }

    /// Serialized, never formatted: message text carries quotes and newlines,
    /// and a raw newline would split one message into two frames.
    fn payload(&self, text: &str) -> String {
        let mut payload = String::new();
        if let Some(token) = &self.token {
            payload.push_str(&json!({ "type": "auth", "token": token.expose() }).to_string());
            payload.push('\n');
        }
        payload.push_str(
            &json!({
                "type": "user",
                "message": { "role": "user", "content": text },
            })
            .to_string(),
        );
        payload.push('\n');
        payload
    }

    /// The path arrives from a state file, so it is treated as input: a real
    /// socket, ours, not reached through a symlink, and — when the address was
    /// pinned — still the same socket it was pinned to.
    ///
    /// It is a check, not a guarantee — nothing binds the stat to the connect
    /// that follows. Closing that race means asking the kernel who is on the
    /// far end after connecting, and `libc` exposes that as `getpeereid` on
    /// macOS and `SO_PEERCRED` on Linux, which is more platform code than this
    /// buys today.
    fn vet_path(&self) -> Result<(), DeliverError> {
        let metadata = std::fs::symlink_metadata(&self.path).map_err(|e| self.classify(e))?;
        if !metadata.file_type().is_socket() {
            // A symlink reports as a symlink here, which is how it is refused.
            return Err(self.refuse("not a socket"));
        }
        if metadata.uid() != unsafe { libc::getuid() } {
            return Err(self.refuse("owned by another user"));
        }
        // Same stat, so the identity is the thing just vetted.
        if let Some(expected) = self.identity
            && SocketIdentity::of(&metadata) != expected
        {
            return Err(DeliverError::Gone {
                path: self.path.clone(),
                reason: "a different socket is bound there now".to_string(),
            });
        }
        Ok(())
    }

    /// ENOENT and ECONNREFUSED are the two ways a departed session shows up.
    /// Everything else — timeouts, EACCES, a peer that died mid-write — stays
    /// transient: one wasted attempt is cheaper than dropping a live session,
    /// and a truly dead one refuses the next connect anyway.
    fn classify(&self, source: io::Error) -> DeliverError {
        match source.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => DeliverError::Gone {
                path: self.path.clone(),
                reason: source.to_string(),
            },
            _ => DeliverError::Transient {
                path: self.path.clone(),
                source,
            },
        }
    }

    fn refuse(&self, reason: &str) -> DeliverError {
        DeliverError::Unsafe {
            path: self.path.clone(),
            reason: reason.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::{self, Receiver};

    /// A directory short enough to keep socket paths under the ~104-byte
    /// `sun_path` cap on macOS, which the usual per-test temp dir blows.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = PathBuf::from(format!("/tmp/ink-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn sock(&self) -> PathBuf {
            self.0.join("s")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Bind, then accept one connection and hand back the lines it sent.
    /// Binding happens before returning so a caller can connect immediately.
    fn serve(path: &Path) -> Receiver<Vec<String>> {
        let listener = UnixListener::bind(path).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let lines = BufReader::new(stream)
                .lines()
                .map(|l| l.unwrap())
                .collect::<Vec<_>>();
            let _ = tx.send(lines);
        });
        rx
    }

    fn received(rx: Receiver<Vec<String>>) -> Vec<String> {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the listener received a message")
    }

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).expect("each line is one JSON document")
    }

    #[test]
    fn a_message_arrives_as_a_single_json_line() {
        let dir = Dir::new("plain");
        let rx = serve(&dir.sock());

        SessionAddress::new(dir.sock(), None)
            .deliver("PR #12 was approved")
            .unwrap();

        let lines = received(rx);
        assert_eq!(lines.len(), 1, "no auth line without a token: {lines:?}");
        assert_eq!(
            parse(&lines[0]),
            json!({"type": "user", "message": {"role": "user", "content": "PR #12 was approved"}})
        );
    }

    #[test]
    fn a_token_rides_ahead_of_the_message_on_its_own_line() {
        let dir = Dir::new("token");
        let rx = serve(&dir.sock());

        SessionAddress::new(dir.sock(), Some(Secret::new("tok_abc123")))
            .deliver("hello")
            .unwrap();

        let lines = received(rx);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            parse(&lines[0]),
            json!({"type": "auth", "token": "tok_abc123"})
        );
        assert_eq!(parse(&lines[1])["type"], "user");
    }

    #[test]
    fn newlines_and_quotes_in_the_text_do_not_break_the_framing() {
        // The failure this guards is silent and total: a raw newline would
        // split one message into two frames, and the second is not valid JSON.
        let text = "he said \"ship it\"\n\nthen: {\"state\": \"merged\"}\n";
        let dir = Dir::new("framing");
        let rx = serve(&dir.sock());

        SessionAddress::new(dir.sock(), None).deliver(text).unwrap();

        let lines = received(rx);
        assert_eq!(lines.len(), 1, "one message, one frame: {lines:?}");
        assert_eq!(parse(&lines[0])["message"]["content"], text);
    }

    #[test]
    fn a_missing_socket_says_the_session_is_gone() {
        let dir = Dir::new("missing");
        let error = SessionAddress::new(dir.sock(), None)
            .deliver("news")
            .unwrap_err();
        assert!(matches!(error, DeliverError::Gone { .. }), "{error}");
        assert!(error.drop_address());
    }

    #[test]
    fn a_socket_nobody_is_listening_on_says_the_session_is_gone() {
        // The realistic shape: the session exited without unlinking, so the
        // file outlives it and only the connect tells the truth.
        let dir = Dir::new("stale");
        drop(UnixListener::bind(dir.sock()).unwrap());
        assert!(dir.sock().exists(), "a dropped listener leaves its socket");

        let error = SessionAddress::new(dir.sock(), None)
            .deliver("news")
            .unwrap_err();
        assert!(matches!(error, DeliverError::Gone { .. }), "{error}");
    }

    #[test]
    fn a_regular_file_is_refused_rather_than_opened() {
        let dir = Dir::new("regular");
        fs::write(dir.sock(), b"not a socket").unwrap();

        let error = SessionAddress::new(dir.sock(), Some(Secret::new("tok")))
            .deliver("news")
            .unwrap_err();
        assert!(matches!(error, DeliverError::Unsafe { .. }), "{error}");
        assert!(error.drop_address());
    }

    #[test]
    fn a_symlink_to_a_live_socket_is_refused() {
        // Where the token would leak: a symlink can point anywhere, and the
        // socket it resolves to passes every check made on the target.
        let dir = Dir::new("symlink");
        let real = dir.0.join("real");
        let rx = serve(&real);
        std::os::unix::fs::symlink(&real, dir.sock()).unwrap();

        let error = SessionAddress::new(dir.sock(), Some(Secret::new("tok")))
            .deliver("news")
            .unwrap_err();
        assert!(matches!(error, DeliverError::Unsafe { .. }), "{error}");
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "nothing was written"
        );
    }

    #[test]
    fn a_socket_rebound_by_a_later_session_is_not_delivered_to() {
        // The pid-reuse case: /tmp/cc-socks/<pid>.sock outlives its session,
        // and the token is no gate — a wrong one is still delivered. Without
        // the identity, this message lands in a stranger's session.
        let dir = Dir::new("rebound");
        let first = serve(&dir.sock());
        let identity = SocketIdentity::capture(&dir.sock()).unwrap();
        let address = SessionAddress::new(dir.sock(), Some(Secret::new("tok"))).expecting(identity);

        address.deliver("for the first session").unwrap();
        assert_eq!(
            received(first).len(),
            2,
            "the pinned socket still takes auth plus message"
        );

        // The first listener is gone once its thread finished; a new session
        // unlinks the stale path and binds its own.
        fs::remove_file(dir.sock()).unwrap();
        let second = serve(&dir.sock());
        assert_ne!(
            SocketIdentity::capture(&dir.sock()).unwrap(),
            identity,
            "a rebound path is a new file, which is what makes this detectable"
        );

        let error = address.deliver("not for you").unwrap_err();
        assert!(matches!(error, DeliverError::Gone { .. }), "{error}");
        assert!(error.drop_address());
        assert!(
            second.recv_timeout(Duration::from_millis(200)).is_err(),
            "the new session heard nothing"
        );
    }

    #[test]
    fn an_address_never_prints_its_token() {
        let address = SessionAddress::new("/tmp/ink/s", Some(Secret::new("tok_abc123")));
        let debug = format!("{address:?}");
        assert!(!debug.contains("tok_abc123"), "{debug}");
        assert!(debug.contains("/tmp/ink/s"), "{debug}");
    }

    #[test]
    fn a_timeout_keeps_the_address_but_a_refusal_drops_it() {
        let address = SessionAddress::new("/tmp/ink/s", None);
        for (kind, dropped) in [
            (io::ErrorKind::TimedOut, false),
            (io::ErrorKind::PermissionDenied, false),
            (io::ErrorKind::BrokenPipe, false),
            (io::ErrorKind::NotFound, true),
            (io::ErrorKind::ConnectionRefused, true),
        ] {
            let error = address.classify(io::Error::from(kind));
            assert_eq!(error.drop_address(), dropped, "{kind:?}: {error}");
        }
    }
}
