//! Local half of `zj-radar remote HOST SESSION`.
//!
//! One interactive OpenSSH connection carries both the terminal and a remote
//! Unix-socket forward. Each remote pane pushes its exact status payload into
//! the listener; this module aggregates by remote pane id and publishes only a
//! single rewritten observation for the real local parent pane.

use crate::payload::{parse, to_wire, StatusPayload, MAX_PAYLOAD_BYTES};
use crate::status::Status;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const MAX_HOST_BYTES: usize = 255;
const MAX_SESSION_BYTES: usize = crate::payload::MAX_TASK_CHARS;
const CONNECTION_BYTES: usize = 16;

// A severed SSH transport can kill the embedded Zellij client between its
// terminal-mode setup and teardown. End synchronized output first so every
// reset that follows is actually painted, then undo the modes Zellij/crossterm
// enables for its TUI. This is deliberately narrower than a full terminal
// reset: the outer Zellij session keeps its colors and other user state.
const TERMINAL_CLEANUP: &[u8] = concat!(
    "\x1b[?2026l", // end synchronized output
    "\x1b[<1u",    // pop one kitty keyboard-protocol level
    "\x1b[?1006l", // SGR mouse off
    "\x1b[?1015l", // urxvt mouse off
    "\x1b[?1003l", // any-event mouse off
    "\x1b[?1002l", // button-event mouse off
    "\x1b[?1000l", // basic mouse off
    "\x1b[?1004l", // focus reporting off
    "\x1b[?2004l", // bracketed paste off
    "\x1b[?25h",   // cursor visible
    "\x1b[?1049l", // leave alternate screen
)
.as_bytes();

struct TerminalState {
    fd: libc::c_int,
    saved: Option<libc::termios>,
    restored: bool,
}

impl TerminalState {
    fn capture() -> Self {
        Self::capture_fd(libc::STDIN_FILENO)
    }

    fn capture_fd(fd: libc::c_int) -> Self {
        let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
        let saved = if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } == 0 {
            Some(unsafe { saved.assume_init() })
        } else {
            None
        };
        Self {
            fd,
            saved,
            restored: false,
        }
    }

    fn restore(&mut self) {
        let stdout = io::stdout();
        let is_terminal = stdout.is_terminal();
        self.restore_to(stdout.lock(), is_terminal);
    }

    fn restore_to(&mut self, mut output: impl Write, output_is_terminal: bool) {
        if self.restored {
            return;
        }
        self.restored = true;
        if output_is_terminal {
            let _ = output.write_all(TERMINAL_CLEANUP);
            let _ = output.flush();
        }
        if let Some(saved) = &self.saved {
            // Immediate restore is intentional: recovery must not wait on a
            // tty whose output flow was itself left in a broken state.
            let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, saved) };
        }
    }
}

impl Drop for TerminalState {
    fn drop(&mut self) {
        self.restore();
    }
}

fn host_is_valid(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_HOST_BYTES
        && raw.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && raw.bytes().last().is_some_and(|b| b.is_ascii_alphanumeric())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@'))
        && raw.bytes().filter(|&b| b == b'@').count() <= 1
}

pub(crate) fn session_is_valid(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_SESSION_BYTES
        && raw.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

pub(crate) fn parse_host(raw: &str) -> Result<String, String> {
    host_is_valid(raw)
        .then(|| raw.to_string())
        .ok_or_else(|| "HOST must use only ASCII letters, digits, '.', '_', '-', and one optional '@'".into())
}

pub(crate) fn parse_session(raw: &str) -> Result<String, String> {
    session_is_valid(raw)
        .then(|| raw.to_string())
        .ok_or_else(|| "SESSION must start with an ASCII letter or digit and use only letters, digits, '_', or '-'".into())
}

fn ssh_user_from_config(raw: &[u8]) -> Option<String> {
    String::from_utf8_lossy(raw).lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let key = fields.next()?;
        let value = fields.next()?;
        (key.eq_ignore_ascii_case("user") && fields.next().is_none())
            .then(|| value.to_string())
    })
}

fn resolve_ssh_user(host: &str) -> Result<String, String> {
    let output = std::process::Command::new("ssh")
        .args(["-G", host])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run `ssh -G`: {e}"))?;
    if !output.status.success() {
        return Err(format!("`ssh -G {host}` exited with {}", output.status));
    }
    ssh_user_from_config(&output.stdout)
        .filter(|user| !user.is_empty() && user.len() <= 256 && !user.contains('\0'))
        .ok_or_else(|| format!("`ssh -G {host}` did not resolve a user"))
}

fn local_socket_path(pid: u32, connection: &str) -> PathBuf {
    Path::new("/tmp")
        .join(format!("zjr-local-{pid}-{connection}"))
        .join("relay.sock")
}

fn connection_id_from_bytes(bytes: [u8; CONNECTION_BYTES]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(CONNECTION_BYTES * 2);
    for byte in bytes {
        id.push(HEX[usize::from(byte >> 4)] as char);
        id.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    id
}

fn new_connection_id() -> io::Result<String> {
    let mut bytes = [0_u8; CONNECTION_BYTES];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(connection_id_from_bytes(bytes))
}

struct SocketPath {
    socket: PathBuf,
    dir: PathBuf,
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_dir(&self.dir);
    }
}

fn bind_private_listener(path: &Path) -> io::Result<(UnixListener, SocketPath)> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "socket path has no parent")
    })?;
    if fs::symlink_metadata(dir).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("socket directory already exists: {}", dir.display()),
        ));
    }
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(dir)?;
    let guard = SocketPath {
        socket: path.to_path_buf(),
        dir: dir.to_path_buf(),
    };
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok((listener, guard))
}

fn remote_command(session: &str, connection: &str) -> String {
    format!(
        "exec env TERM=xterm-256color COLORTERM=truecolor PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:$PATH\" zj-radar relay {session} {connection}"
    )
}

fn ssh_args(
    host: &str,
    session: &str,
    connection: &str,
    remote_socket: &Path,
    local_socket: &Path,
) -> Vec<String> {
    vec![
        "-tt".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "StreamLocalBindUnlink=no".into(),
        "-o".into(),
        "StreamLocalBindMask=0177".into(),
        "-R".into(),
        format!("{}:{}", remote_socket.display(), local_socket.display()),
        "--".into(),
        host.into(),
        remote_command(session, connection),
    ]
}

struct Aggregate {
    local_pane_id: u32,
    session: String,
    panes: BTreeMap<u32, StatusPayload>,
    live_seen: HashSet<u32>,
}

impl Aggregate {
    fn new(local_pane_id: u32, session: &str) -> Self {
        Self {
            local_pane_id,
            session: session.to_string(),
            panes: BTreeMap::new(),
            live_seen: HashSet::new(),
        }
    }

    fn apply(
        &mut self,
        delivery: crate::relay::Delivery,
        payload: StatusPayload,
    ) -> Option<StatusPayload> {
        if delivery == crate::relay::Delivery::Replay && self.live_seen.contains(&payload.pane_id) {
            return None;
        }
        if delivery == crate::relay::Delivery::Live {
            self.live_seen.insert(payload.pane_id);
        }
        if payload.gone {
            self.panes.remove(&payload.pane_id);
        } else {
            self.panes.insert(payload.pane_id, payload);
        }
        Some(self.current())
    }

    fn current(&self) -> StatusPayload {
        let Some((_, selected)) = self.panes.iter().max_by(
            |(left_id, left), (right_id, right)| {
                left.status
                    .cmp(&right.status)
                    .then_with(|| right_id.cmp(left_id))
            },
        ) else {
            return StatusPayload {
                pane_id: self.local_pane_id,
                status: Status::Idle,
                task: self.session.clone(),
                source: "generic".into(),
                gone: true,
                ..StatusPayload::default()
            };
        };
        let mut aggregate = selected.clone();
        aggregate.pane_id = self.local_pane_id;
        aggregate.task = self.session.clone();
        aggregate.gone = false;
        aggregate
    }
}

fn read_one(stream: &mut UnixStream) -> Option<(crate::relay::Delivery, String)> {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let mut bytes = Vec::new();
    stream
        .take((MAX_PAYLOAD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return None;
    }
    let (delivery, raw) = crate::relay::decode_frame(&bytes)?;
    Some((delivery, raw.to_string()))
}

enum Accepted {
    Stop,
    Ignore,
    Frame(crate::relay::Delivery, String),
}

fn accept_one(listener: &UnixListener, stopping: &AtomicBool) -> io::Result<Accepted> {
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(pair) => break pair,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    };
    if stopping.load(Ordering::Acquire) {
        return Ok(Accepted::Stop);
    }
    let frame = read_one(&mut stream);
    if stopping.load(Ordering::Acquire) {
        return Ok(Accepted::Stop);
    }
    Ok(match frame {
        Some((delivery, raw)) => Accepted::Frame(delivery, raw),
        None => Accepted::Ignore,
    })
}

fn serve(
    listener: UnixListener,
    stopping: Arc<AtomicBool>,
    local_pane_id: u32,
    session: String,
) -> io::Result<()> {
    let mut aggregate = Aggregate::new(local_pane_id, &session);
    loop {
        let (delivery, raw) = match accept_one(&listener, &stopping)? {
            Accepted::Stop => return Ok(()),
            Accepted::Ignore => continue,
            Accepted::Frame(delivery, raw) => (delivery, raw),
        };
        let Some(payload) = parse(&raw) else {
            continue;
        };
        if let Some(outgoing) = aggregate.apply(delivery, payload) {
            crate::notify::send_local_payload(to_wire(&outgoing));
        }
    }
}

fn gone_payload(local_pane_id: u32, session: &str) -> String {
    to_wire(&StatusPayload {
        pane_id: local_pane_id,
        status: Status::Idle,
        task: session.to_string(),
        source: "generic".into(),
        gone: true,
        ..StatusPayload::default()
    })
}

pub(crate) fn run(host: &str, session: &str) {
    if !host_is_valid(host) || !session_is_valid(session) {
        crate::exit::fail_report("zj-radar remote", "invalid HOST or SESSION");
        return;
    }
    let Some(local_pane_id) = crate::notify::pane_id_from_env() else {
        crate::exit::fail_report(
            "zj-radar remote",
            "must run inside the local Zellij pane that should own the relayed status",
        );
        return;
    };
    let user = match resolve_ssh_user(host) {
        Ok(user) => user,
        Err(e) => {
            crate::exit::fail_report("zj-radar remote", e);
            return;
        }
    };
    let connection = match new_connection_id() {
        Ok(connection) => connection,
        Err(e) => {
            crate::exit::fail_report(
                "zj-radar remote",
                format!("generating a connection id failed: {e}"),
            );
            return;
        }
    };
    let remote_socket = crate::relay::socket_path(&user, session, &connection);
    let local_socket = local_socket_path(std::process::id(), &connection);
    let (listener, _socket_path) = match bind_private_listener(&local_socket) {
        Ok(bound) => bound,
        Err(e) => {
            crate::exit::fail_report(
                "zj-radar remote",
                format!("binding {} failed: {e}", local_socket.display()),
            );
            return;
        }
    };
    let stopping = Arc::new(AtomicBool::new(false));
    let listener_stopping = Arc::clone(&stopping);
    let listener_session = session.to_string();
    let listener_thread = std::thread::spawn(move || {
        serve(
            listener,
            listener_stopping,
            local_pane_id,
            listener_session,
        )
    });

    let args = ssh_args(host, session, &connection, &remote_socket, &local_socket);
    let mut terminal = TerminalState::capture();
    let ssh_status = std::process::Command::new("ssh").args(&args).status();
    terminal.restore();

    stopping.store(true, Ordering::Release);
    let _ = UnixStream::connect(&local_socket);
    let listener_result = listener_thread.join();
    crate::notify::send_local_payload(gone_payload(local_pane_id, session));

    match listener_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => crate::exit::fail_report("zj-radar remote", format!("local relay failed: {e}")),
        Err(_) => crate::exit::fail_report("zj-radar remote", "local relay thread panicked"),
    }
    match ssh_status {
        Ok(status) if status.success() => {}
        Ok(status) => crate::exit::fail_report("zj-radar remote", format!("ssh exited with {status}")),
        Err(e) => crate::exit::fail_report("zj-radar remote", format!("could not launch ssh: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(pane_id: u32, status: Status, source: &str) -> StatusPayload {
        StatusPayload {
            pane_id,
            status,
            source: source.into(),
            task: "untrusted task".into(),
            ..StatusPayload::default()
        }
    }

    #[test]
    fn host_and_session_validation_exclude_option_and_shell_syntax() {
        for host in ["agent-server", "alice@example.com", "10.0.0.8", "lab_host"] {
            assert!(parse_host(host).is_ok(), "host={host}");
        }
        for host in ["", "-oProxyCommand=bad", "host name", "host;bad", "a@@b"] {
            assert!(parse_host(host).is_err(), "host={host}");
        }
        for session in ["agf", "agent-lab_2", "R2D2"] {
            assert!(parse_session(session).is_ok(), "session={session}");
        }
        for session in ["", "-bad", "a/b", "a b", "x;bad"] {
            assert!(parse_session(session).is_err(), "session={session}");
        }
        assert!(parse_session(&"a".repeat(MAX_SESSION_BYTES)).is_ok());
        assert!(parse_session(&"a".repeat(MAX_SESSION_BYTES + 1)).is_err());
    }

    #[test]
    fn ssh_config_user_parser_reads_one_exact_directive() {
        assert_eq!(
            ssh_user_from_config(b"host agent\nhostname 10.0.0.8\nuser alice\nport 22\n"),
            Some("alice".into())
        );
        assert_eq!(ssh_user_from_config(b"username wrong\n"), None);
        assert_eq!(ssh_user_from_config(b"user too many fields\n"), None);
    }

    #[test]
    fn ssh_argv_is_one_interactive_connection_with_stream_forward() {
        let connection = "00112233445566778899aabbccddeeff";
        let args = ssh_args(
            "agent-server",
            "agf",
            connection,
            Path::new("/tmp/zjr-1234.sock"),
            Path::new("/tmp/zjr-local-42.sock"),
        );
        assert_eq!(
            args,
            vec![
                "-tt",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "StreamLocalBindUnlink=no",
                "-o",
                "StreamLocalBindMask=0177",
                "-R",
                "/tmp/zjr-1234.sock:/tmp/zjr-local-42.sock",
                "--",
                "agent-server",
                "exec env TERM=xterm-256color COLORTERM=truecolor PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:$PATH\" zj-radar relay agf 00112233445566778899aabbccddeeff"
            ]
        );
        assert_eq!(args.iter().filter(|arg| arg.as_str() == "-R").count(), 1);
    }

    #[test]
    fn remote_command_has_only_static_expansion_and_a_validated_session() {
        let session = parse_session("agf-2").unwrap();
        let connection = crate::relay::parse_connection("00112233445566778899aabbccddeeff")
            .unwrap();
        assert_eq!(
            remote_command(&session, &connection),
            "exec env TERM=xterm-256color COLORTERM=truecolor PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:$PATH\" zj-radar relay agf-2 00112233445566778899aabbccddeeff"
        );
    }

    #[test]
    fn connection_id_is_exact_lowercase_hex() {
        assert_eq!(
            connection_id_from_bytes([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
                0xcc, 0xdd, 0xee, 0xff,
            ]),
            "00112233445566778899aabbccddeeff"
        );
    }

    #[test]
    fn terminal_cleanup_ends_synchronized_output_first() {
        assert!(TERMINAL_CLEANUP.starts_with(b"\x1b[?2026l"));
        for required in [
            b"\x1b[<1u".as_slice(),
            b"\x1b[?1006l".as_slice(),
            b"\x1b[?1004l".as_slice(),
            b"\x1b[?2004l".as_slice(),
            b"\x1b[?25h".as_slice(),
            b"\x1b[?1049l".as_slice(),
        ] {
            assert!(
                TERMINAL_CLEANUP
                    .windows(required.len())
                    .any(|window| window == required),
                "missing cleanup sequence {required:?}"
            );
        }
    }

    #[test]
    fn terminal_state_restores_termios_and_only_writes_cleanup_once() {
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );

        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(unsafe { libc::tcgetattr(slave, original.as_mut_ptr()) }, 0);
        let original = unsafe { original.assume_init() };
        // macOS can add driver-owned input flags on the first tcsetattr for a
        // fresh PTY. Normalize once, then capture the state our guard owns.
        assert_eq!(unsafe { libc::tcsetattr(slave, libc::TCSANOW, &original) }, 0);
        let mut normalized = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(unsafe { libc::tcgetattr(slave, normalized.as_mut_ptr()) }, 0);
        let original = unsafe { normalized.assume_init() };
        let mut terminal = TerminalState::capture_fd(slave);

        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        assert_eq!(unsafe { libc::tcsetattr(slave, libc::TCSANOW, &raw) }, 0);

        let mut output = Vec::new();
        terminal.restore_to(&mut output, true);
        terminal.restore_to(&mut output, true);

        let mut restored = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(unsafe { libc::tcgetattr(slave, restored.as_mut_ptr()) }, 0);
        let restored = unsafe { restored.assume_init() };
        // PENDIN is a transient driver state bit, not terminal configuration;
        // macOS may set it while applying raw/canonical transitions.
        const PENDIN: libc::tcflag_t = 0x2000_0000;
        assert_eq!(restored.c_iflag, original.c_iflag);
        assert_eq!(restored.c_oflag, original.c_oflag);
        assert_eq!(restored.c_cflag, original.c_cflag);
        assert_eq!(restored.c_lflag & !PENDIN, original.c_lflag & !PENDIN);
        assert_eq!(restored.c_cc, original.c_cc);
        assert_eq!(output, TERMINAL_CLEANUP);

        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }

    #[test]
    fn aggregate_uses_severity_and_never_leaks_a_remote_pane_id() {
        let mut aggregate = Aggregate::new(77, "agf");
        let running = aggregate
            .apply(crate::relay::Delivery::Live, status(4, Status::Running, "omp"))
            .unwrap();
        assert_eq!(running.pane_id, 77);
        assert_eq!(running.status, Status::Running);
        assert_eq!(running.task, "agf");
        assert_eq!(running.source, "omp");

        let pending = aggregate
            .apply(crate::relay::Delivery::Live, status(9, Status::Pending, "codex"))
            .unwrap();
        assert_eq!(pending.pane_id, 77);
        assert_eq!(pending.status, Status::Pending);
        assert_eq!(pending.task, "agf");

        let after_gone = aggregate
            .apply(
                crate::relay::Delivery::Live,
                StatusPayload {
                    pane_id: 9,
                    status: Status::Idle,
                    gone: true,
                    ..StatusPayload::default()
                },
            )
            .unwrap();
        assert_eq!(after_gone.pane_id, 77);
        assert_eq!(after_gone.status, Status::Running);
        assert!(!after_gone.gone);

        let empty = aggregate
            .apply(
                crate::relay::Delivery::Live,
                StatusPayload {
                    pane_id: 4,
                    status: Status::Idle,
                    gone: true,
                    ..StatusPayload::default()
                },
            )
            .unwrap();
        assert_eq!(empty.pane_id, 77);
        assert_eq!(empty.task, "agf");
        assert!(empty.gone);
    }

    #[test]
    fn aggregate_tie_breaks_by_remote_pane_id() {
        let mut aggregate = Aggregate::new(1, "work");
        let _ = aggregate.apply(
            crate::relay::Delivery::Live,
            status(12, Status::Done, "codex"),
        );
        let selected = aggregate
            .apply(crate::relay::Delivery::Live, status(3, Status::Done, "omp"))
            .unwrap();
        assert_eq!(selected.source, "omp");
        assert_eq!(selected.pane_id, 1);
    }

    #[test]
    fn newer_live_delivery_wins_over_a_later_stale_replay() {
        let mut aggregate = Aggregate::new(77, "agf");
        let done = aggregate
            .apply(crate::relay::Delivery::Live, status(4, Status::Done, "omp"))
            .unwrap();
        assert_eq!(done.status, Status::Done);
        assert!(
            aggregate
                .apply(
                    crate::relay::Delivery::Replay,
                    status(4, Status::Running, "omp")
                )
                .is_none(),
            "stale replay must not roll a newer live event backward"
        );
        assert_eq!(aggregate.current().status, Status::Done);

        aggregate
            .apply(
                crate::relay::Delivery::Live,
                StatusPayload {
                    pane_id: 4,
                    status: Status::Idle,
                    gone: true,
                    ..StatusPayload::default()
                },
            )
            .unwrap();
        assert!(
            aggregate
                .apply(
                    crate::relay::Delivery::Replay,
                    status(4, Status::Idle, "omp")
                )
                .is_none(),
            "stale identity replay must not resurrect a live gone event"
        );
        assert!(aggregate.current().gone);
    }

    #[test]
    fn local_listener_socket_is_owner_only() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("listener");
        let path = dir.join("relay.sock");
        let (_listener, guard) = bind_private_listener(&path).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o077, 0);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        drop(guard);
        assert!(!path.exists());
        assert!(!dir.exists());
    }

    #[test]
    fn stream_reader_rejects_more_than_64_kib() {
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            sender.write_all(&vec![b'x'; MAX_PAYLOAD_BYTES + 1]).unwrap();
            sender.shutdown(std::net::Shutdown::Write).unwrap();
        });
        assert!(read_one(&mut receiver).is_none());
        writer.join().unwrap();
    }

    #[test]
    fn shutdown_ignores_a_real_payload_queued_before_the_wake_connection() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("listener");
        let path = dir.join("relay.sock");
        let (listener, guard) = bind_private_listener(&path).unwrap();
        let mut queued = UnixStream::connect(&path).unwrap();
        let raw = to_wire(&status(9, Status::Running, "omp"));
        let mut frame = vec![b'L'];
        frame.extend_from_slice(raw.as_bytes());
        queued.write_all(&frame).unwrap();
        queued.shutdown(std::net::Shutdown::Write).unwrap();
        let stopping = AtomicBool::new(true);

        assert!(matches!(
            accept_one(&listener, &stopping).unwrap(),
            Accepted::Stop
        ));
        drop(guard);
    }
}
