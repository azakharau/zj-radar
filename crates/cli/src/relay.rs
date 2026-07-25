//! Remote-side persistence and forwarding for `zj-radar remote`.
//!
//! Every pushed status is kept as one atomic file per Zellij session/pane and,
//! when an SSH stream-local forward is present, copied over one bounded Unix
//! stream. The hidden `relay` command replays that state once before replacing
//! itself with the minimal embedded Zellij attach.

use crate::kind::Kind;
use crate::payload::{parse, to_wire, StatusPayload, MAX_PAYLOAD_BYTES};
use crate::status::Status;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

const SOCKET_ROOT: &str = "/tmp";
const CONNECTION_HEX_BYTES: usize = 32;
const ACTIVE_FILE: &str = "active";
const EMBED_CONFIG_FILE: &str = "embed.kdl";
const EMBED_CONFIG: &str = r#"env {
    TERM "xterm-256color"
    COLORTERM "truecolor"
}

keybinds {
    shared {
        bind "Ctrl l" {
            Clear;
            Write 12;
        }
    }
}
"#;
const RECONCILE_LOCK_FILE: &str = ".reconcile.lock";
const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const MAX_DISCOVERY_BYTES: usize = 16 * 1024 * 1024;
const DISCOVERY_DRAIN_BYTES_PER_TICK: usize = 256 * 1024;
const LIVE_FRAME: u8 = b'L';
const REPLAY_FRAME: u8 = b'R';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    Live,
    Replay,
}

impl Delivery {
    fn tag(self) -> u8 {
        match self {
            Delivery::Live => LIVE_FRAME,
            Delivery::Replay => REPLAY_FRAME,
        }
    }
}

pub(crate) fn decode_frame(bytes: &[u8]) -> Option<(Delivery, &str)> {
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return None;
    }
    let (&tag, payload) = bytes.split_first()?;
    let delivery = match tag {
        LIVE_FRAME => Delivery::Live,
        REPLAY_FRAME => Delivery::Replay,
        _ => return None,
    };
    Some((delivery, std::str::from_utf8(payload).ok()?))
}

/// Stable, dependency-free FNV-1a key. The remote user is part of the key so
/// two accounts attaching to an identically named session do not share a
/// stream-local forwarding endpoint in the host-wide `/tmp` namespace.
fn relay_key(user: &str, session: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in user
        .bytes()
        .chain(std::iter::once(0xff))
        .chain(session.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub(crate) fn connection_is_valid(raw: &str) -> bool {
    raw.len() == CONNECTION_HEX_BYTES
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

pub(crate) fn parse_connection(raw: &str) -> Result<String, String> {
    connection_is_valid(raw)
        .then(|| raw.to_string())
        .ok_or_else(|| "CONNECTION must be exactly 32 lowercase hexadecimal characters".into())
}

fn socket_path_in(root: &Path, user: &str, session: &str, connection: &str) -> PathBuf {
    debug_assert!(connection_is_valid(connection));
    root.join(format!(
        "zjr-{:016x}-{connection}.sock",
        relay_key(user, session)
    ))
}

fn state_dir_in(root: &Path, user: &str, session: &str) -> PathBuf {
    root.join(format!("zjr-{:016x}.state", relay_key(user, session)))
}

pub(crate) fn socket_path(user: &str, session: &str, connection: &str) -> PathBuf {
    socket_path_in(Path::new(SOCKET_ROOT), user, session, connection)
}

fn state_dir(user: &str, session: &str) -> PathBuf {
    state_dir_in(Path::new(SOCKET_ROOT), user, session)
}

fn relay_user() -> Option<String> {
    std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("LOGNAME").ok())
        .filter(|user| !user.is_empty() && user.len() <= 256 && !user.contains('\0'))
}

fn state_file(dir: &Path, pane_id: u32) -> PathBuf {
    dir.join(format!("{pane_id}.json"))
}

fn active_file(dir: &Path) -> PathBuf {
    dir.join(ACTIVE_FILE)
}

fn write_embed_config(dir: &Path) -> io::Result<PathBuf> {
    let path = dir.join(EMBED_CONFIG_FILE);
    crate::fsutil::atomic_write(&path, EMBED_CONFIG.as_bytes())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

/// Create the state directory without a world-readable interval. Existing
/// directories are tightened as well; a symlink or non-directory is refused.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if !meta.file_type().is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "relay state path is not a directory",
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            if let Err(create) = builder.create(dir) {
                if create.kind() != io::ErrorKind::AlreadyExists {
                    return Err(create);
                }
                let meta = fs::symlink_metadata(dir)?;
                if !meta.file_type().is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "relay state path is not a directory",
                    ));
                }
            }
        }
        Err(e) => return Err(e),
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

fn publish_active_connection(dir: &Path, connection: &str) -> io::Result<()> {
    if !connection_is_valid(connection) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid relay connection id",
        ));
    }
    let path = active_file(dir);
    crate::fsutil::atomic_write(&path, connection.as_bytes())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn read_active_connection(dir: &Path) -> Option<String> {
    let path = active_file(dir);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.mode() & 0o077 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(CONNECTION_HEX_BYTES);
    fs::File::open(path)
        .ok()?
        .take((CONNECTION_HEX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() != CONNECTION_HEX_BYTES {
        return None;
    }
    let connection = String::from_utf8(bytes).ok()?;
    connection_is_valid(&connection).then_some(connection)
}

fn active_socket_in(
    state: &Path,
    root: &Path,
    user: &str,
    session: &str,
) -> Option<PathBuf> {
    let connection = read_active_connection(state)?;
    Some(socket_path_in(root, user, session, &connection))
}

fn remove_previous_connection_socket(
    root: &Path,
    user: &str,
    session: &str,
    previous: Option<&str>,
    current: &str,
    current_metadata: &fs::Metadata,
) {
    let Some(previous) = previous.filter(|previous| *previous != current) else {
        return;
    };
    let path = socket_path_in(root, user, session, previous);
    remove_socket_if_owned(&path, current_metadata.uid());
}

fn remove_socket_if_owned(path: &Path, owner_uid: u32) {
    let removable = fs::symlink_metadata(path).ok().is_some_and(|metadata| {
        metadata.file_type().is_socket()
            && metadata.mode() & 0o077 == 0
            && metadata.uid() == owner_uid
    });
    if removable {
        let _ = fs::remove_file(path);
    }
}

struct ForwardedSocket {
    path: PathBuf,
    owner_uid: u32,
}

impl ForwardedSocket {
    fn new(path: PathBuf, owner_uid: u32) -> Self {
        Self { path, owner_uid }
    }
}

impl Drop for ForwardedSocket {
    fn drop(&mut self) {
        remove_socket_if_owned(&self.path, self.owner_uid);
    }
}

fn update_state(dir: &Path, payload: &StatusPayload, raw: &str) -> io::Result<()> {
    if raw.len() > MAX_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay payload exceeds 64 KiB",
        ));
    }
    let path = state_file(dir, payload.pane_id);
    ensure_private_dir(dir)?;
    crate::fsutil::atomic_write(&path, raw.as_bytes())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

/// Atomically publish a complete startup identity only when no hook state won
/// the race first. A hard link is the std-only no-replace primitive: the temp
/// file is fully written before the final name appears, and `AlreadyExists`
/// leaves the hook's newer payload untouched.
fn persist_identity_if_absent(dir: &Path, payload: &StatusPayload, raw: &str) -> io::Result<bool> {
    if raw.len() > MAX_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay payload exceeds 64 KiB",
        ));
    }
    ensure_private_dir(dir)?;
    let path = state_file(dir, payload.pane_id);
    let mut temp_and_file = None;
    for attempt in 0..16_u8 {
        let temp = dir.join(format!(
            ".identity-{}-{}-{attempt}.tmp",
            payload.pane_id,
            std::process::id()
        ));
        match fs::OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => {
                temp_and_file = Some((temp, file));
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let Some((temp, mut file)) = temp_and_file else {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a relay identity temp file",
        ));
    };
    let prepared = file
        .write_all(raw.as_bytes())
        .and_then(|_| fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)));
    drop(file);
    if let Err(e) = prepared {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    let linked = match fs::hard_link(&temp, &path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    };
    let _ = fs::remove_file(temp);
    linked
}

fn push_payload(socket: &Path, raw: &str, delivery: Delivery) -> io::Result<()> {
    if raw.len() + 1 > MAX_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay payload exceeds 64 KiB",
        ));
    }
    let mut frame = Vec::with_capacity(raw.len() + 1);
    frame.push(delivery.tag());
    frame.extend_from_slice(raw.as_bytes());
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(1)))?;
    stream.write_all(&frame)?;
    stream.shutdown(std::net::Shutdown::Write)
}

fn relay_is_armed(state: &Path) -> bool {
    fs::symlink_metadata(state)
        .ok()
        .is_some_and(|meta| meta.file_type().is_dir() && meta.mode() & 0o077 == 0)
}

fn record_for_session(
    root: &Path,
    user: &str,
    session: &str,
    payload: &StatusPayload,
    raw: &str,
) {
    let state = state_dir_in(root, user, session);
    if !relay_is_armed(&state) || update_state(&state, payload, raw).is_err() {
        return;
    }
    if let Some(socket) = active_socket_in(&state, root, user, session) {
        let _ = push_payload(&socket, raw, Delivery::Live);
    }
}

/// Persist and forward one payload from `notify`. This is deliberately
/// best-effort: an unavailable bridge must never break an agent hook, while
/// the ordinary local `zellij pipe` broadcast still proceeds independently.
pub(crate) fn record_from_env(raw: &str) {
    if raw.len() > MAX_PAYLOAD_BYTES {
        return;
    }
    let Some(payload) = parse(raw) else {
        return;
    };
    let Some(session) = std::env::var("ZELLIJ_SESSION_NAME")
        .ok()
        .filter(|s| crate::remote::session_is_valid(s))
    else {
        return;
    };
    let Some(user) = relay_user() else {
        return;
    };
    record_for_session(Path::new(SOCKET_ROOT), &user, &session, &payload, raw);
}

fn read_stored_payload(path: &Path, pane_id: u32) -> Option<(String, StatusPayload)> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take((MAX_PAYLOAD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return None;
    }
    let raw = String::from_utf8(bytes).ok()?;
    let payload = parse(&raw)?;
    (payload.pane_id == pane_id).then_some((raw, payload))
}

fn read_payload_file(path: &Path, pane_id: u32) -> Option<String> {
    read_stored_payload(path, pane_id).map(|(raw, _)| raw)
}

fn state_paths(dir: &Path) -> io::Result<Vec<(u32, PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut paths: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let pane_id = name.to_str()?.strip_suffix(".json")?.parse().ok()?;
            Some((pane_id, entry.path()))
        })
        .collect();
    paths.sort_by_key(|(pane_id, _)| *pane_id);
    Ok(paths)
}

fn saved_payloads(dir: &Path) -> io::Result<Vec<String>> {
    let paths = state_paths(dir)?;
    Ok(paths
        .into_iter()
        .filter_map(|(pane_id, path)| read_payload_file(&path, pane_id))
        .collect())
}

fn replay_state(dir: &Path, socket: &Path) -> io::Result<usize> {
    let payloads = saved_payloads(dir)?;
    for raw in &payloads {
        push_payload(socket, raw, Delivery::Replay)?;
    }
    Ok(payloads.len())
}

#[derive(serde::Deserialize)]
struct ListedPane {
    id: u32,
    is_plugin: bool,
    exited: bool,
    #[serde(default)]
    pane_command: String,
}

fn command_basename(raw: &str) -> &str {
    raw.split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("")
}

/// Successful startup discovery only. `None` is distinct from an empty map:
/// failure to query/parse must preserve saved state, while a successful empty
/// result proves there are no live panes and may prune it all.
fn live_panes_from_json(raw: &[u8]) -> Option<BTreeMap<u32, Option<Kind>>> {
    let panes: Vec<ListedPane> = serde_json::from_slice(raw).ok()?;
    let mut live = BTreeMap::new();
    for pane in panes {
        if pane.is_plugin || pane.exited {
            continue;
        }
        let kind = Kind::from_source(command_basename(&pane.pane_command));
        live.insert(pane.id, kind.is_agent().then_some(kind));
    }
    Some(live)
}

fn list_panes_args(session: &str) -> [&str; 6] {
    ["--session", session, "action", "list-panes", "--all", "--json"]
}

fn discovery_result(
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> Option<BTreeMap<u32, Option<Kind>>> {
    if success {
        return live_panes_from_json(stdout);
    }
    let no_session = [stdout, stderr].into_iter().any(|bytes| {
        String::from_utf8_lossy(bytes).contains("There is no active session")
    });
    no_session.then(BTreeMap::new)
}

/// Exactly one structured host query at relay startup. It never infers work
/// from titles or output: `pane_command` contributes identity only.
fn discover_live_panes(session: &str) -> Option<BTreeMap<u32, Option<Kind>>> {
    let child = std::process::Command::new("zellij")
        .args(list_panes_args(session))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let output = wait_for_output(child, DISCOVERY_TIMEOUT).ok()??;
    discovery_result(output.status.success(), &output.stdout, &output.stderr)
}

fn wait_for_output(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> io::Result<Option<std::process::Output>> {
    let Some(mut stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "stdout is not piped"));
    };
    let Some(mut stderr) = child.stderr.take() else {
        terminate_child(&mut child);
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "stderr is not piped"));
    };
    if let Err(e) = set_nonblocking(&stdout).and_then(|_| set_nonblocking(&stderr)) {
        terminate_child(&mut child);
        return Err(e);
    }
    let deadline = std::time::Instant::now() + timeout;
    let mut status = None;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    loop {
        if let Err(e) = drain_pipe(&mut stdout, &mut stdout_bytes, &mut stdout_eof)
            .and_then(|_| drain_pipe(&mut stderr, &mut stderr_bytes, &mut stderr_eof))
        {
            terminate_child(&mut child);
            return Err(e);
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(e) => {
                    terminate_child(&mut child);
                    return Err(e);
                }
            }
        }
        if let Some(status) = status.filter(|_| stdout_eof && stderr_eof) {
            return Ok(Some(std::process::Output {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            }));
        }
        if std::time::Instant::now() >= deadline {
            terminate_child(&mut child);
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn terminate_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn set_nonblocking<T: AsRawFd>(stream: &T) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    // SAFETY: `fd` belongs to a live ChildStdout/ChildStderr for this call;
    // fcntl neither takes ownership nor outlives the descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same live descriptor; the existing flags are preserved and only
    // O_NONBLOCK is added.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_pipe<R: Read>(reader: &mut R, bytes: &mut Vec<u8>, eof: &mut bool) -> io::Result<()> {
    if *eof {
        return Ok(());
    }
    let mut chunk = [0_u8; 8192];
    let mut drained = 0;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => {
                *eof = true;
                return Ok(());
            }
            Ok(count) if bytes.len() + count <= MAX_DISCOVERY_BYTES => {
                bytes.extend_from_slice(&chunk[..count]);
                drained += count;
                if drained >= DISCOVERY_DRAIN_BYTES_PER_TICK {
                    return Ok(());
                }
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zellij pane discovery output exceeds 16 MiB",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn lock_reconciliation(dir: &Path) -> io::Result<fs::File> {
    ensure_private_dir(dir)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(RECONCILE_LOCK_FILE))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.lock()?;
    Ok(file)
}

fn reconcile_temp_pane_id(name: &str) -> Option<u32> {
    let mut parts = name
        .strip_prefix(".reconcile-")?
        .strip_suffix(".tmp")?
        .split('-');
    let pane_id = parts.next()?.parse().ok()?;
    let _: u32 = parts.next()?.parse().ok()?;
    let _: u8 = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some(pane_id)
}

fn recover_reconcile_state(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(pane_id) = name.to_str().and_then(reconcile_temp_pane_id) else {
            continue;
        };
        let quarantine = entry.path();
        if read_stored_payload(&quarantine, pane_id).is_some() {
            match fs::hard_link(&quarantine, state_file(dir, pane_id)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        fs::remove_file(quarantine)?;
    }
    Ok(())
}

fn quarantine_state_file(dir: &Path, pane_id: u32, path: &Path) -> io::Result<Option<PathBuf>> {
    for attempt in 0..16_u8 {
        let quarantine = dir.join(format!(
            ".reconcile-{pane_id}-{}-{attempt}.tmp",
            std::process::id()
        ));
        let file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&quarantine)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let prepared = fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o600));
        drop(file);
        if let Err(e) = prepared {
            let _ = fs::remove_file(&quarantine);
            return Err(e);
        }
        match fs::rename(path, &quarantine) {
            Ok(()) => return Ok(Some(quarantine)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let _ = fs::remove_file(&quarantine);
                return Ok(None);
            }
            Err(e) => {
                let _ = fs::remove_file(&quarantine);
                return Err(e);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a relay reconcile temp file",
    ))
}

fn restore_quarantined_state(quarantine: &Path, path: &Path) -> io::Result<()> {
    match fs::hard_link(quarantine, path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    fs::remove_file(quarantine)
}

fn finish_reconcile_state(
    quarantine: &Path,
    path: &Path,
    pane_id: u32,
    live_kind: Option<Kind>,
) -> io::Result<()> {
    let keep = live_kind.is_some_and(|kind| {
        read_stored_payload(quarantine, pane_id)
            .is_some_and(|(_, payload)| Kind::from_source(&payload.source) == kind)
    });
    if keep {
        restore_quarantined_state(quarantine, path)
    } else {
        fs::remove_file(quarantine)
    }
}

fn prepare_state<F>(dir: &Path, discover: F) -> io::Result<()>
where
    F: FnOnce() -> Option<BTreeMap<u32, Option<Kind>>>,
{
    let _lock = lock_reconciliation(dir)?;
    recover_reconcile_state(dir)?;
    let mut quarantined = Vec::new();
    for (pane_id, path) in state_paths(dir)? {
        if let Some(quarantine) = quarantine_state_file(dir, pane_id, &path)? {
            quarantined.push((pane_id, path, quarantine));
        }
    }
    let live = discover();
    for (pane_id, path, quarantine) in quarantined {
        if let Some(live) = &live {
            finish_reconcile_state(
                &quarantine,
                &path,
                pane_id,
                live.get(&pane_id).and_then(|kind| *kind),
            )?;
        } else {
            restore_quarantined_state(&quarantine, &path)?;
        }
    }
    let Some(live) = live else {
        return Ok(());
    };
    for (pane_id, kind) in live {
        let Some(kind) = kind else {
            continue;
        };
        let identity = StatusPayload {
            pane_id,
            status: Status::Idle,
            source: kind.as_source().to_string(),
            ..StatusPayload::default()
        };
        let raw = to_wire(&identity);
        let _ = persist_identity_if_absent(dir, &identity, &raw)?;
    }
    Ok(())
}

fn attach_args(session: &str, config: &Path) -> [OsString; 14] {
    [
        "--config".into(),
        config.as_os_str().to_owned(),
        "attach".into(),
        "-c".into(),
        session.into(),
        "options".into(),
        "--default-layout".into(),
        "embed".into(),
        "--show-release-notes".into(),
        "false".into(),
        "--show-startup-tips".into(),
        "false".into(),
        "--pane-frames".into(),
        "false".into(),
    ]
}

/// Hidden SSH-side command: replay the exact latest state for this session,
/// then replace this process with the minimal embedded Zellij attach.
pub(crate) fn run(session: &str, connection: &str) {
    if !connection_is_valid(connection) {
        crate::exit::fail_report("zj-radar relay", "invalid connection id");
        return;
    }
    let Some(user) = relay_user() else {
        crate::exit::fail_report("zj-radar relay", "could not resolve the remote user");
        return;
    };
    let socket = socket_path(&user, session, connection);
    let socket_metadata = match fs::symlink_metadata(&socket) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.mode() & 0o077 == 0 => {
            metadata
        }
        _ => {
            crate::exit::fail_report(
                "zj-radar relay",
                format!("forwarded socket is missing or not owner-only: {}", socket.display()),
            );
            return;
        }
    };
    let _socket = ForwardedSocket::new(socket.clone(), socket_metadata.uid());
    let state = state_dir(&user, session);
    if let Err(e) = ensure_private_dir(&state) {
        crate::exit::fail_report("zj-radar relay", format!("arming session state failed: {e}"));
        return;
    }
    let config = match write_embed_config(&state) {
        Ok(path) => path,
        Err(e) => {
            crate::exit::fail_report(
                "zj-radar relay",
                format!("preparing embedded config failed: {e}"),
            );
            return;
        }
    };
    if let Err(e) = prepare_state(&state, || discover_live_panes(session)) {
        crate::exit::fail_report("zj-radar relay", format!("preparing session state failed: {e}"));
        return;
    }
    if let Err(e) = replay_state(&state, &socket) {
        crate::exit::fail_report("zj-radar relay", format!("replaying session state failed: {e}"));
        return;
    }
    let previous = read_active_connection(&state);
    if let Err(e) = publish_active_connection(&state, connection) {
        crate::exit::fail_report("zj-radar relay", format!("publishing relay endpoint failed: {e}"));
        return;
    }
    // Close the replay/pointer race: updates that landed during the first
    // replay were persisted before the atomic pointer switch, and this second
    // pass delivers them. It is supplemental: the first pass already proved
    // the bridge, so a failed duplicate must not tear down the live terminal or
    // leave `active` pointing at a deliberately removed socket.
    let _ = replay_state(&state, &socket);
    remove_previous_connection_socket(
        Path::new(SOCKET_ROOT),
        &user,
        session,
        previous.as_deref(),
        connection,
        &socket_metadata,
    );
    let error = std::process::Command::new("zellij")
        .args(attach_args(session, &config))
        .exec();
    crate::exit::fail_report("zj-radar relay", format!("launching the remote session failed: {error}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::to_wire;
    use crate::status::Status;

    fn payload(pane_id: u32, status: Status, gone: bool) -> String {
        to_wire(&StatusPayload {
            pane_id,
            status,
            source: "omp".into(),
            gone,
            ..StatusPayload::default()
        })
    }

    #[test]
    fn socket_path_is_short_and_unique_per_connection() {
        let first = "00112233445566778899aabbccddeeff";
        let second = "ffeeddccbbaa99887766554433221100";
        let a = socket_path_in(Path::new("/tmp"), "alice", "work", first);
        assert_eq!(
            a,
            socket_path_in(Path::new("/tmp"), "alice", "work", first)
        );
        assert_ne!(
            a,
            socket_path_in(Path::new("/tmp"), "alice", "work", second)
        );
        assert_ne!(
            a,
            socket_path_in(Path::new("/tmp"), "bob", "work", first)
        );
        assert_ne!(
            a,
            socket_path_in(Path::new("/tmp"), "alice", "other", first)
        );
        assert!(a.starts_with("/tmp"));
        assert!(a.as_os_str().len() < 80, "Unix socket path must stay short: {a:?}");
    }

    #[test]
    fn connection_parser_accepts_only_exact_lowercase_hex() {
        assert!(parse_connection("00112233445566778899aabbccddeeff").is_ok());
        for invalid in [
            "",
            "0011",
            "00112233445566778899AABBCCDDEEFF",
            "00112233445566778899aabbccddeefg",
            "00112233445566778899aabbccddeeff00",
        ] {
            assert!(parse_connection(invalid).is_err(), "connection={invalid}");
        }
    }

    #[test]
    fn per_pane_state_replays_latest_and_keeps_gone_as_a_tombstone() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let pane_7_running = payload(7, Status::Running, false);
        let pane_7_done = payload(7, Status::Done, false);
        let pane_9_pending = payload(9, Status::Pending, false);
        update_state(&state, &parse(&pane_7_running).unwrap(), &pane_7_running).unwrap();
        update_state(&state, &parse(&pane_7_done).unwrap(), &pane_7_done).unwrap();
        update_state(&state, &parse(&pane_9_pending).unwrap(), &pane_9_pending).unwrap();

        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o077,
            0,
            "state directory must be owner-only"
        );
        let saved = saved_payloads(&state).unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(parse(&saved[0]).unwrap().status, Status::Done);
        assert_eq!(parse(&saved[1]).unwrap().pane_id, 9);

        let pane_7_gone = payload(7, Status::Idle, true);
        update_state(&state, &parse(&pane_7_gone).unwrap(), &pane_7_gone).unwrap();
        let saved = saved_payloads(&state).unwrap();
        assert_eq!(saved.len(), 2);
        assert!(parse(&saved[0]).unwrap().gone);
        assert_eq!(parse(&saved[1]).unwrap().pane_id, 9);
        let tombstone = fs::read_to_string(state_file(&state, 7)).unwrap();
        assert!(parse(&tombstone).unwrap().gone);
    }

    #[test]
    fn replay_sends_one_bounded_stream_per_saved_pane() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        for (pane_id, status) in [(4, Status::Running), (11, Status::Error)] {
            let raw = payload(pane_id, status, false);
            update_state(&state, &parse(&raw).unwrap(), &raw).unwrap();
        }
        let socket = temp.path().join("relay.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let receiver = std::thread::spawn(move || {
            let mut pane_ids = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut frame = Vec::new();
                stream.read_to_end(&mut frame).unwrap();
                assert!(frame.len() <= MAX_PAYLOAD_BYTES);
                let (delivery, raw) = decode_frame(&frame).unwrap();
                assert_eq!(delivery, Delivery::Replay);
                pane_ids.push(parse(raw).unwrap().pane_id);
            }
            pane_ids
        });

        assert_eq!(replay_state(&state, &socket).unwrap(), 2);
        assert_eq!(receiver.join().unwrap(), vec![4, 11]);
    }

    #[test]
    fn second_replay_delivers_a_gone_that_landed_during_startup() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let running = payload(4, Status::Running, false);
        update_state(&state, &parse(&running).unwrap(), &running).unwrap();
        let socket = temp.path().join("relay.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let receiver = std::thread::spawn(move || {
            let mut payloads = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut frame = Vec::new();
                stream.read_to_end(&mut frame).unwrap();
                let (delivery, raw) = decode_frame(&frame).unwrap();
                assert_eq!(delivery, Delivery::Replay);
                payloads.push(parse(raw).unwrap());
            }
            payloads
        });

        assert_eq!(replay_state(&state, &socket).unwrap(), 1);
        let gone = payload(4, Status::Idle, true);
        update_state(&state, &parse(&gone).unwrap(), &gone).unwrap();
        assert_eq!(replay_state(&state, &socket).unwrap(), 1);
        let payloads = receiver.join().unwrap();
        assert_eq!(payloads[0].status, Status::Running);
        assert!(payloads[1].gone);
    }

    #[test]
    fn structured_discovery_keeps_live_terminals_and_maps_agent_basenames_only() {
        let raw = br#"[
          {"id":0,"is_plugin":false,"exited":false,"pane_command":"/home/a/.local/bin/omp -p"},
          {"id":1,"is_plugin":false,"exited":false,"pane_command":"cargo"},
          {"id":2,"is_plugin":true,"exited":false,"pane_command":"omp"},
          {"id":3,"is_plugin":false,"exited":true,"pane_command":"codex"},
          {"id":4,"is_plugin":false,"exited":false,"pane_command":"/usr/local/bin/claude --resume"}
        ]"#;
        let live = live_panes_from_json(raw).unwrap();
        assert_eq!(live.len(), 3);
        assert_eq!(live.get(&0), Some(&Some(Kind::Omp)));
        assert_eq!(live.get(&1), Some(&None));
        assert_eq!(live.get(&4), Some(&Some(Kind::Claude)));
        assert!(!live.contains_key(&2), "plugins are not terminal panes");
        assert!(!live.contains_key(&3), "exited panes are not live");
        assert_eq!(command_basename("/usr/bin/codex --profile review"), "codex");
    }

    #[test]
    fn discovery_child_output_is_captured_before_the_deadline() {
        let child = std::process::Command::new("sh")
            .args(["-c", "printf ready"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let output = wait_for_output(child, std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ready");
    }

    #[test]
    fn discovery_drains_output_larger_than_an_os_pipe() {
        let child = std::process::Command::new("sh")
            .args([
                "-c",
                "i=0; while [ \"$i\" -lt 20000 ]; do printf 0123456789abcdef; i=$((i + 1)); done",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let output = wait_for_output(child, std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 320_000);
    }

    #[test]
    fn wedged_discovery_child_is_killed_at_the_deadline() {
        let child = std::process::Command::new("sh")
            .args(["-c", "exec sleep 5"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        assert!(wait_for_output(child, std::time::Duration::from_millis(20))
            .unwrap()
            .is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn continuously_readable_discovery_output_cannot_starve_the_deadline() {
        let child = std::process::Command::new("sh")
            .args(["-c", "exec yes x"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        assert!(wait_for_output(child, std::time::Duration::from_millis(20))
            .unwrap()
            .is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn startup_reconcile_prunes_dead_ids_and_seeds_only_missing_live_agents() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let existing = payload(1, Status::Running, false);
        let stale = payload(8, Status::Pending, false);
        let non_agent = payload(5, Status::Running, false);
        let wrong_identity = payload(4, Status::Pending, false);
        update_state(&state, &parse(&existing).unwrap(), &existing).unwrap();
        update_state(&state, &parse(&stale).unwrap(), &stale).unwrap();
        update_state(&state, &parse(&non_agent).unwrap(), &non_agent).unwrap();
        update_state(&state, &parse(&wrong_identity).unwrap(), &wrong_identity).unwrap();

        let live = BTreeMap::from([
            (1, Some(Kind::Omp)),
            (4, Some(Kind::Codex)),
            (5, None),
        ]);
        prepare_state(&state, || Some(live)).unwrap();

        assert!(!state_file(&state, 8).exists(), "dead pane state is pruned");
        assert!(!state_file(&state, 5).exists(), "ordinary commands lose stale agent state");
        let saved: BTreeMap<_, _> = saved_payloads(&state)
            .unwrap()
            .into_iter()
            .map(|raw| {
                let payload = parse(&raw).unwrap();
                (payload.pane_id, payload)
            })
            .collect();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[&1].status, Status::Running, "latest state is retained");
        assert_eq!(saved[&4].status, Status::Idle);
        assert_eq!(saved[&4].source, "codex");
    }

    #[test]
    fn startup_identity_never_overwrites_hook_state_that_won_the_race() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let running = payload(3, Status::Running, false);
        update_state(&state, &parse(&running).unwrap(), &running).unwrap();
        let idle = to_wire(&StatusPayload {
            pane_id: 3,
            status: Status::Idle,
            source: "omp".into(),
            ..StatusPayload::default()
        });
        assert!(!persist_identity_if_absent(&state, &parse(&idle).unwrap(), &idle).unwrap());
        let saved = fs::read_to_string(state_file(&state, 3)).unwrap();
        assert_eq!(parse(&saved).unwrap().status, Status::Running);
    }

    #[test]
    fn gone_tombstone_blocks_startup_identity_resurrection() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let gone = payload(3, Status::Idle, true);
        update_state(&state, &parse(&gone).unwrap(), &gone).unwrap();

        let idle = to_wire(&StatusPayload {
            pane_id: 3,
            status: Status::Idle,
            source: "omp".into(),
            ..StatusPayload::default()
        });
        let live = BTreeMap::from([(3, Some(Kind::Omp))]);
        prepare_state(&state, || Some(live)).unwrap();
        let saved = saved_payloads(&state).unwrap();
        assert_eq!(saved.len(), 1);
        assert!(parse(&saved[0]).unwrap().gone);
        assert!(!persist_identity_if_absent(&state, &parse(&idle).unwrap(), &idle).unwrap());
        assert!(
            parse(&fs::read_to_string(state_file(&state, 3)).unwrap())
                .unwrap()
                .gone
        );
    }

    #[test]
    fn reconcile_never_removes_newer_hook_state() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let old = payload(3, Status::Pending, false);
        update_state(&state, &parse(&old).unwrap(), &old).unwrap();
        let path = state_file(&state, 3);
        let quarantine = quarantine_state_file(&state, 3, &path).unwrap().unwrap();

        let newer = to_wire(&StatusPayload {
            pane_id: 3,
            status: Status::Running,
            source: "codex".into(),
            ..StatusPayload::default()
        });
        update_state(&state, &parse(&newer).unwrap(), &newer).unwrap();
        finish_reconcile_state(&quarantine, &path, 3, Some(Kind::Codex)).unwrap();

        let saved = parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.status, Status::Running);
        assert_eq!(saved.source, "codex");
        assert!(!quarantine.exists());
    }

    #[test]
    fn reconcile_restore_does_not_overwrite_a_concurrent_tombstone() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let running = payload(3, Status::Running, false);
        update_state(&state, &parse(&running).unwrap(), &running).unwrap();
        let path = state_file(&state, 3);
        let quarantine = quarantine_state_file(&state, 3, &path).unwrap().unwrap();

        let gone = payload(3, Status::Idle, true);
        update_state(&state, &parse(&gone).unwrap(), &gone).unwrap();
        finish_reconcile_state(&quarantine, &path, 3, Some(Kind::Omp)).unwrap();

        assert!(parse(&fs::read_to_string(&path).unwrap()).unwrap().gone);
        assert!(!quarantine.exists());
    }

    #[test]
    fn discovery_runs_inside_the_reconciliation_lock() {
        let temp = tempfile::tempdir().unwrap();
        prepare_state(temp.path(), || {
            let second = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(temp.path().join(RECONCILE_LOCK_FILE))
                .unwrap();
            assert!(matches!(second.try_lock(), Err(fs::TryLockError::WouldBlock)));
            Some(BTreeMap::new())
        })
        .unwrap();
    }

    #[test]
    fn failed_discovery_recovers_state_quarantined_by_a_crashed_relay() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let running = payload(3, Status::Running, false);
        update_state(&state, &parse(&running).unwrap(), &running).unwrap();
        let path = state_file(&state, 3);
        let quarantine = quarantine_state_file(&state, 3, &path).unwrap().unwrap();
        assert!(!path.exists());

        prepare_state(&state, || None).unwrap();

        let saved = parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.status, Status::Running);
        assert!(!quarantine.exists());
    }

    #[test]
    fn crash_recovery_never_overwrites_a_newer_hook_tombstone() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let running = payload(3, Status::Running, false);
        update_state(&state, &parse(&running).unwrap(), &running).unwrap();
        let path = state_file(&state, 3);
        let quarantine = quarantine_state_file(&state, 3, &path).unwrap().unwrap();
        let gone = payload(3, Status::Idle, true);
        update_state(&state, &parse(&gone).unwrap(), &gone).unwrap();

        prepare_state(&state, || Some(BTreeMap::from([(3, Some(Kind::Omp))]))).unwrap();

        assert!(parse(&fs::read_to_string(&path).unwrap()).unwrap().gone);
        assert!(!quarantine.exists());
    }

    #[test]
    fn hook_state_written_during_discovery_wins_over_the_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let old = payload(3, Status::Pending, false);
        update_state(&state, &parse(&old).unwrap(), &old).unwrap();
        let path = state_file(&state, 3);
        let newer = to_wire(&StatusPayload {
            pane_id: 3,
            status: Status::Running,
            source: "codex".into(),
            ..StatusPayload::default()
        });

        prepare_state(&state, || {
            assert!(!path.exists(), "the old state is quarantined before discovery");
            update_state(&state, &parse(&newer).unwrap(), &newer).unwrap();
            Some(BTreeMap::from([(3, None)]))
        })
        .unwrap();

        let saved = parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.status, Status::Running);
        assert_eq!(saved.source, "codex");
    }

    #[test]
    fn failed_restore_keeps_the_only_quarantined_copy() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let running = payload(3, Status::Running, false);
        update_state(&state, &parse(&running).unwrap(), &running).unwrap();
        let path = state_file(&state, 3);
        let quarantine = quarantine_state_file(&state, 3, &path).unwrap().unwrap();
        let unavailable = state.join("missing").join("3.json");

        assert!(finish_reconcile_state(&quarantine, &unavailable, 3, Some(Kind::Omp)).is_err());
        assert!(quarantine.exists(), "recovery must retain the only state copy");
    }

    #[test]
    fn discovery_and_attach_argv_are_exact_and_shell_free() {
        assert_eq!(
            list_panes_args("agf"),
            ["--session", "agf", "action", "list-panes", "--all", "--json"]
        );
        assert_eq!(
            attach_args("agf", Path::new("/tmp/agf/embed.kdl")),
            [
                "--config",
                "/tmp/agf/embed.kdl",
                "attach",
                "-c",
                "agf",
                "options",
                "--default-layout",
                "embed",
                "--show-release-notes",
                "false",
                "--show-startup-tips",
                "false",
                "--pane-frames",
                "false",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn embedded_config_is_owner_only_and_binds_clear() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_embed_config(temp.path()).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), EMBED_CONFIG);
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(EMBED_CONFIG.parse::<kdl::KdlDocument>().is_ok());
        assert!(EMBED_CONFIG.contains("TERM \"xterm-256color\""));
        assert!(EMBED_CONFIG.contains("COLORTERM \"truecolor\""));
        assert!(EMBED_CONFIG.contains("bind \"Ctrl l\""));
        assert!(EMBED_CONFIG.contains("Clear;\n            Write 12;"));
    }

    #[test]
    fn confirmed_absent_session_prunes_but_other_discovery_failures_preserve() {
        assert!(discovery_result(false, b"", b"There is no active session!\n")
            .unwrap()
            .is_empty());
        assert!(discovery_result(false, b"", b"permission denied\n").is_none());
        assert!(discovery_result(true, b"not json", b"").is_none());
    }

    #[test]
    fn relay_frames_are_tagged_and_bounded_as_one_stream_payload() {
        let raw = payload(3, Status::Running, false);
        let mut live = vec![LIVE_FRAME];
        live.extend_from_slice(raw.as_bytes());
        assert_eq!(decode_frame(&live), Some((Delivery::Live, raw.as_str())));
        let mut replay = live;
        replay[0] = REPLAY_FRAME;
        assert_eq!(decode_frame(&replay), Some((Delivery::Replay, raw.as_str())));
        assert!(decode_frame(&vec![b'x'; MAX_PAYLOAD_BYTES + 1]).is_none());
    }

    #[test]
    fn active_connection_is_atomic_owner_only_and_strictly_validated() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        ensure_private_dir(&state).unwrap();
        let first = "00112233445566778899aabbccddeeff";
        let second = "ffeeddccbbaa99887766554433221100";

        publish_active_connection(&state, first).unwrap();
        assert_eq!(read_active_connection(&state).as_deref(), Some(first));
        let metadata = fs::symlink_metadata(active_file(&state)).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.mode() & 0o077, 0);

        publish_active_connection(&state, second).unwrap();
        assert_eq!(read_active_connection(&state).as_deref(), Some(second));

        fs::set_permissions(active_file(&state), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_active_connection(&state).is_none(), "loose mode is rejected");

        fs::remove_file(active_file(&state)).unwrap();
        let target = state.join("target");
        fs::write(&target, first).unwrap();
        std::os::unix::fs::symlink(&target, active_file(&state)).unwrap();
        assert!(read_active_connection(&state).is_none(), "symlinks are rejected");

        fs::remove_file(active_file(&state)).unwrap();
        fs::write(active_file(&state), format!("{first}0")).unwrap();
        fs::set_permissions(active_file(&state), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_active_connection(&state).is_none(), "oversized values are rejected");
    }

    #[test]
    fn publishing_a_reconnect_removes_only_the_previous_owner_socket() {
        let temp = tempfile::Builder::new()
            .prefix("zjr-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path();
        let state = state_dir_in(root, "alice", "work");
        ensure_private_dir(&state).unwrap();
        let previous = "00112233445566778899aabbccddeeff";
        let current = "ffeeddccbbaa99887766554433221100";
        let previous_socket = socket_path_in(root, "alice", "work", previous);
        let current_socket = socket_path_in(root, "alice", "work", current);
        let _previous_listener = std::os::unix::net::UnixListener::bind(&previous_socket).unwrap();
        let _current_listener = std::os::unix::net::UnixListener::bind(&current_socket).unwrap();
        fs::set_permissions(&previous_socket, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&current_socket, fs::Permissions::from_mode(0o600)).unwrap();
        publish_active_connection(&state, previous).unwrap();
        let captured = read_active_connection(&state);
        publish_active_connection(&state, current).unwrap();

        let current_metadata = fs::symlink_metadata(&current_socket).unwrap();
        remove_previous_connection_socket(
            root,
            "alice",
            "work",
            captured.as_deref(),
            current,
            &current_metadata,
        );
        assert!(!previous_socket.exists());
        assert!(current_socket.exists());

        let regular_token = "11112222333344445555666677778888";
        let regular = socket_path_in(root, "alice", "work", regular_token);
        fs::write(&regular, b"not a socket").unwrap();
        fs::set_permissions(&regular, fs::Permissions::from_mode(0o600)).unwrap();
        remove_previous_connection_socket(
            root,
            "alice",
            "work",
            Some(regular_token),
            current,
            &current_metadata,
        );
        assert!(regular.exists(), "regular files are never removed");
    }

    #[test]
    fn failed_relay_scope_removes_only_its_socket_and_keeps_previous_active() {
        let temp = tempfile::Builder::new()
            .prefix("zjr-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path();
        let state = state_dir_in(root, "alice", "work");
        ensure_private_dir(&state).unwrap();
        let previous = "00112233445566778899aabbccddeeff";
        publish_active_connection(&state, previous).unwrap();
        let current = "ffeeddccbbaa99887766554433221100";
        let current_socket = socket_path_in(root, "alice", "work", current);
        let _listener = std::os::unix::net::UnixListener::bind(&current_socket).unwrap();
        fs::set_permissions(&current_socket, fs::Permissions::from_mode(0o600)).unwrap();
        let owner_uid = fs::symlink_metadata(&current_socket).unwrap().uid();

        {
            let _guard = ForwardedSocket::new(current_socket.clone(), owner_uid);
        }
        assert!(!current_socket.exists(), "failed endpoint is removed");
        assert_eq!(
            read_active_connection(&state).as_deref(),
            Some(previous),
            "the previous live bridge remains selected"
        );
    }

    #[test]
    fn stale_connection_does_not_block_persistence_or_the_next_live_bridge() {
        let temp = tempfile::Builder::new()
            .prefix("zjr-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path();
        let state = state_dir_in(root, "alice", "work");
        ensure_private_dir(&state).unwrap();
        let stale = "00112233445566778899aabbccddeeff";
        publish_active_connection(&state, stale).unwrap();

        let running = payload(7, Status::Running, false);
        record_for_session(root, "alice", "work", &parse(&running).unwrap(), &running);
        assert_eq!(
            parse(&fs::read_to_string(state_file(&state, 7)).unwrap())
                .unwrap()
                .status,
            Status::Running,
            "a failed stale-socket push must not lose the latest state"
        );

        let current = "ffeeddccbbaa99887766554433221100";
        let socket = socket_path_in(root, "alice", "work", current);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        publish_active_connection(&state, current).unwrap();
        let receiver = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut frame = Vec::new();
            stream.read_to_end(&mut frame).unwrap();
            frame
        });

        let done = payload(7, Status::Done, false);
        record_for_session(root, "alice", "work", &parse(&done).unwrap(), &done);
        let frame = receiver.join().unwrap();
        let (delivery, raw) = decode_frame(&frame).unwrap();
        assert_eq!(delivery, Delivery::Live);
        assert_eq!(parse(raw).unwrap().status, Status::Done);
    }

    #[test]
    fn ordinary_hooks_stay_inert_until_a_session_is_armed() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        assert!(!relay_is_armed(&state));
        ensure_private_dir(&state).unwrap();
        assert!(relay_is_armed(&state));
    }
}
