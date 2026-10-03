//! Daemon lifecycle: --enable / --disable / --status.
//!
//! pidfile: ~/.local/share/frank-opencode/frank.pid
//! portfile: ~/.local/share/frank-opencode/frank.port
//! log: ~/.local/share/frank-opencode/frank.log

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use sysinfo::{Pid, System};

fn dir() -> PathBuf {
    crate::config::AppConfig::data_dir()
}

pub fn pid_file() -> PathBuf {
    dir().join("frank.pid")
}

pub fn port_file() -> PathBuf {
    dir().join("frank.port")
}

pub fn log_file() -> PathBuf {
    dir().join("frank.log")
}

fn read_pid() -> Option<u32> {
    fs::read_to_string(pid_file()).ok()?.trim().parse().ok()
}

fn pid_alive(pid: u32) -> bool {
    let mut sys = System::new();
    sys.refresh_all();
    sys.process(Pid::from_u32(pid)).is_some()
}

/// Single-connection health probe: TCP connect + `GET /health`, expect 200.
/// (Previously this was two separate connects under a confusing name.)
fn gateway_healthy(port: u16) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    let addr: std::net::SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(1500)));
    let _ = s.write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n");
    let mut buf = [0u8; 1024];
    let Ok(n) = s.read(&mut buf) else {
        return false;
    };
    // Parse the status line (`HTTP/1.x 200 ...`) instead of matching "200"
    // anywhere in the head: a body or header containing "200" is not health.
    let status_line = String::from_utf8_lossy(&buf[..n])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let mut parts = status_line.split_whitespace();
    matches!(
        (parts.next(), parts.next()),
        (Some(v), Some("200")) if v.starts_with("HTTP/")
    )
}

/// Write a small state file with owner-only permissions (0600 on unix).
/// The README asks for 0600; `fs::write` alone would follow the umask (0644),
/// and `mode(0o600)` only applies at creation, so enforce afterwards too in
/// case the file pre-existed with wider permissions.
pub(crate) fn write_private(path: PathBuf, content: &str) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&path).and_then(|mut f| {
        use std::io::Write;
        f.write_all(content.as_bytes())
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn stored_port() -> Option<u16> {
    fs::read_to_string(port_file()).ok()?.trim().parse().ok()
}

/// Open (or create) a log file in append mode with owner-only permissions.
/// Sibling of [`write_private`] for the daemon log, which must append rather
/// than truncate; same create-or-tighten 0600 semantics.
pub(crate) fn open_private_append(path: PathBuf) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(&path)?;
    #[cfg(unix)]
    {
        // `mode` only applies at creation; enforce for pre-existing files too.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(f)
}

/// Enable: spawn detached child running `--serve`, wait for /health.
pub fn enable(port: u16, config_arg: Option<PathBuf>) -> anyhow::Result<()> {
    if let Some(pid) = read_pid() {
        if pid_alive(pid) {
            let running = stored_port().unwrap_or(crate::config::DEFAULT_PORT);
            if running != port {
                anyhow::bail!(
                    "frank-opencode already running on :{running} (pid {pid}); \
                     run `frank-opencode --disable` first to move it to :{port}"
                );
            }
            println!("frank-opencode already running (pid {pid}, port {running})");
            print_next_steps(running);
            return Ok(());
        }
        let _ = fs::remove_file(pid_file());
    }
    fs::create_dir_all(dir())?;
    let exe = std::env::current_exe()?;
    let log = open_private_append(log_file())?;
    let log_err = log.try_clone()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--serve")
        .arg("--port")
        .arg(port.to_string())
        .arg("--daemon-child");
    if let Some(c) = config_arg {
        cmd.arg("--config").arg(c);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    // Detach from the terminal session (no process-group dependency for MVP).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let child = cmd.spawn()?;
    write_private(pid_file(), &child.id().to_string())?;
    write_private(port_file(), &port.to_string())?;

    // Wait for health (catalog now loads in background, so this is fast).
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if gateway_healthy(port) {
            println!(
                "frank-opencode enabled on http://127.0.0.1:{port} (pid {})",
                child.id()
            );
            print_next_steps(port);
            return Ok(());
        }
        if let Some(pid) = read_pid() {
            if !pid_alive(pid) {
                break;
            }
        }
    }
    anyhow::bail!(
        "daemon did not become healthy; see {}",
        log_file().display()
    )
}

/// Disable: SIGTERM the pidfile process, clean up.
pub fn disable() -> anyhow::Result<()> {
    let Some(pid) = read_pid() else {
        println!("frank-opencode is not running (no pidfile)");
        return Ok(());
    };
    if !pid_alive(pid) {
        let _ = fs::remove_file(pid_file());
        let _ = fs::remove_file(port_file());
        println!("frank-opencode was not running (stale pidfile removed)");
        return Ok(());
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    // Give it a moment, then confirm.
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if !pid_alive(pid) {
            break;
        }
    }
    if pid_alive(pid) {
        anyhow::bail!("could not stop pid {pid}; kill it manually");
    }
    let _ = fs::remove_file(pid_file());
    let _ = fs::remove_file(port_file());
    println!("frank-opencode disabled (pid {pid} stopped)");
    Ok(())
}

pub fn status() -> anyhow::Result<()> {
    match read_pid() {
        Some(pid) if pid_alive(pid) => {
            let port = stored_port().unwrap_or(crate::config::DEFAULT_PORT);
            let h = if gateway_healthy(port) {
                "healthy"
            } else {
                "unreachable"
            };
            println!("frank-opencode running (pid {pid}, port {port}, {h})");
        }
        Some(pid) => println!("frank-opencode pidfile exists but pid {pid} is dead"),
        None => println!("frank-opencode is not running"),
    }
    Ok(())
}

fn print_next_steps(port: u16) {
    println!();
    println!("Claude Code:");
    println!("  export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}");
    println!("  export ANTHROPIC_AUTH_TOKEN=dummy");
    println!("  export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1");
    println!("  claude");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &PathBuf) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn write_private_creates_0600() {
        let dir = std::env::temp_dir().join("frank-perm-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("new-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        write_private(path.clone(), "secret").unwrap();
        assert_eq!(mode_of(&path), 0o600, "{}", path.display());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_private_tightens_preexisting_file() {
        let dir = std::env::temp_dir().join("frank-perm-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("pre-{}.txt", std::process::id()));
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(mode_of(&path), 0o644);
        write_private(path.clone(), "new").unwrap();
        assert_eq!(mode_of(&path), 0o600, "{}", path.display());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let _ = std::fs::remove_file(&path);
    }
}
