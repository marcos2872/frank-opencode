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

fn health_ok(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/health");
    std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
        && reqwest_blocking_health(&url)
}

fn reqwest_blocking_health(url: &str) -> bool {
    // Minimal blocking GET without adding a blocking http client dep:
    // use curl-less raw request via TCP would be overkill; shell out to the
    // same binary path is unnecessary — a TCP connect + short HTTP probe:
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    let host_port = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("");
    let Ok(mut s) =
        TcpStream::connect_timeout(&host_port.parse().unwrap(), Duration::from_millis(500))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(800)));
    let _ = s.write_all(b"GET /health HTTP/1.0\r\nHost: x\r\n\r\n");
    let mut buf = [0u8; 512];
    let Ok(n) = s.read(&mut buf) else {
        return false;
    };
    String::from_utf8_lossy(&buf[..n]).contains("200")
}

pub fn stored_port() -> Option<u16> {
    fs::read_to_string(port_file()).ok()?.trim().parse().ok()
}

/// Enable: spawn detached child running `--serve`, wait for /health.
pub fn enable(port: u16, config_arg: Option<PathBuf>) -> anyhow::Result<()> {
    if let Some(pid) = read_pid() {
        if pid_alive(pid) {
            let p = stored_port().unwrap_or(port);
            println!("frank-opencode already running (pid {pid}, port {p})");
            print_next_steps(p);
            return Ok(());
        }
        let _ = fs::remove_file(pid_file());
    }
    fs::create_dir_all(dir())?;
    let exe = std::env::current_exe()?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file())?;
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
    fs::write(pid_file(), child.id().to_string())?;
    fs::write(port_file(), port.to_string())?;

    // Wait for health (up to ~8s: catalog fetch can be slow on first run).
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if health_ok(port) {
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
    println!("frank-opencode disabled (pid {pid} stopped)");
    Ok(())
}

pub fn status() -> anyhow::Result<()> {
    match read_pid() {
        Some(pid) if pid_alive(pid) => {
            let port = stored_port().unwrap_or(crate::config::DEFAULT_PORT);
            let h = if health_ok(port) {
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
