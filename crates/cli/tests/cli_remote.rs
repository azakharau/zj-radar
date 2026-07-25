use assert_cmd::Command;

#[test]
fn remote_help_exposes_exact_host_and_session_positionals() {
    let output = Command::cargo_bin("zj-radar")
        .unwrap()
        .args(["remote", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage: zj-radar remote <HOST> <SESSION>"), "{stdout}");
    assert!(stdout.contains("SSH destination or config alias"), "{stdout}");
    assert!(stdout.contains("Remote Zellij session name"), "{stdout}");
}

#[test]
fn remote_rejects_untrusted_host_and_session_before_execution() {
    for args in [
        ["remote", "host;touch", "safe"],
        ["remote", "agent-server", "bad/session"],
        ["remote", "-oProxyCommand=bad", "safe"],
    ] {
        let output = Command::cargo_bin("zj-radar")
            .unwrap()
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "args={args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("invalid value") || stderr.contains("unexpected argument"),
            "args={args:?}, stderr={stderr}"
        );
    }
}

#[cfg(unix)]
#[test]
fn sequential_remote_runs_use_one_ssh_each_and_distinct_endpoints() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let shims = tempfile::tempdir().unwrap();
    let log = shims.path().join("ssh.log");
    let ssh = shims.path().join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\n\
         if [ \"$1\" = \"-G\" ]; then\n\
           printf 'query:%s\\n' \"$*\" >> \"$ZJ_RADAR_SSH_LOG\"\n\
           printf 'hostname 10.0.0.8\\nuser alice\\nport 22\\n'\n\
           exit 0\n\
         fi\n\
         printf 'connect:%s\\n' \"$*\" >> \"$ZJ_RADAR_SSH_LOG\"\n\
         exit 0\n",
    )
    .unwrap();
    let zellij = shims.path().join("zellij");
    fs::write(&zellij, "#!/bin/sh\nexit 0\n").unwrap();
    for path in [&ssh, &zellij] {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let path = format!("{}:{}", shims.path().display(), existing.to_string_lossy());

    for _ in 0..2 {
        let output = Command::cargo_bin("zj-radar")
            .unwrap()
            .args(["remote", "agent-server", "agf"])
            .env("PATH", &path)
            .env("ZJ_RADAR_SSH_LOG", &log)
            .env("ZELLIJ", "1")
            .env("ZELLIJ_PANE_ID", "terminal_77")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let lines: Vec<_> = fs::read_to_string(log).unwrap().lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 4, "one config expansion plus one connection per run: {lines:?}");
    let mut connections = Vec::new();
    for pair in lines.chunks_exact(2) {
        assert_eq!(pair[0], "query:-G agent-server");
        assert!(pair[1].starts_with("connect:-tt -o ExitOnForwardFailure=yes"), "{:?}", pair[1]);
        assert!(pair[1].contains(" -o StreamLocalBindUnlink=no "), "{:?}", pair[1]);
        assert!(pair[1].contains(" -R /tmp/zjr-"), "{:?}", pair[1]);
        let connection = pair[1].split_whitespace().last().unwrap();
        assert_eq!(connection.len(), 32, "{:?}", pair[1]);
        assert!(
            connection
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        );
        assert!(
            pair[1].contains(&format!("-{connection}.sock:")),
            "the same token must select the forwarded socket: {:?}",
            pair[1]
        );
        assert!(
            pair[1].contains(
                "-- agent-server exec env TERM=xterm-256color COLORTERM=truecolor PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:$PATH\" zj-radar relay agf "
            ),
            "{:?}",
            pair[1]
        );
        connections.push(connection.to_string());
    }
    assert_ne!(connections[0], connections[1], "sequential runs must not reuse a stale endpoint");
}

#[cfg(unix)]
#[test]
fn failed_ssh_restores_the_calling_terminal() {
    use std::fs::{self, File};
    use std::io::Read;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command as ProcessCommand, Stdio};

    let shims = tempfile::tempdir().unwrap();
    let ssh = shims.path().join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\n\
         if [ \"$1\" = \"-G\" ]; then\n\
           printf 'hostname 10.0.0.8\\nuser alice\\nport 22\\n'\n\
           exit 0\n\
         fi\n\
         stty raw -echo\n\
         exit 255\n",
    )
    .unwrap();
    let zellij = shims.path().join("zellij");
    fs::write(&zellij, "#!/bin/sh\nexit 0\n").unwrap();
    for path in [&ssh, &zellij] {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

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

    let existing = std::env::var_os("PATH").unwrap_or_default();
    let path = format!("{}:{}", shims.path().display(), existing.to_string_lossy());
    let duplicate = |fd| {
        let copy = unsafe { libc::dup(fd) };
        assert!(copy >= 0);
        Stdio::from(unsafe { File::from_raw_fd(copy) })
    };
    let status = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("zj-radar"))
        .args(["remote", "agent-server", "agf"])
        .env("PATH", path)
        .env("ZELLIJ", "1")
        .env("ZELLIJ_PANE_ID", "terminal_77")
        .stdin(duplicate(slave))
        .stdout(duplicate(slave))
        .stderr(duplicate(slave))
        .status()
        .unwrap();
    assert!(!status.success());

    let mut restored = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(unsafe { libc::tcgetattr(slave, restored.as_mut_ptr()) }, 0);
    let restored = unsafe { restored.assume_init() };
    const PENDIN: libc::tcflag_t = 0x2000_0000;
    assert_eq!(restored.c_iflag, original.c_iflag);
    assert_eq!(restored.c_oflag, original.c_oflag);
    assert_eq!(restored.c_cflag, original.c_cflag);
    assert_eq!(restored.c_lflag & !PENDIN, original.c_lflag & !PENDIN);

    unsafe { libc::close(slave) };
    let mut output = Vec::new();
    unsafe { File::from_raw_fd(master) }
        .read_to_end(&mut output)
        .unwrap();
    assert!(
        output
            .windows(b"\x1b[?2026l".len())
            .any(|window| window == b"\x1b[?2026l"),
        "terminal cleanup was not emitted: {:?}",
        String::from_utf8_lossy(&output)
    );
}
