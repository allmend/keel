//! End-to-end harness. Builds a keel image from the working tree, brings up
//! docker compose stacks, and drives keel the way an operator does: the `keel`
//! CLI inside a node, and keelctl's mTLS control protocol from the host.
//!
//! Every test is `#[ignore]`, so a plain `cargo test` never needs Docker:
//!
//! ```text
//! cargo test -p keel-e2e --no-fail-fast -- --ignored
//! ```
//!
//! - `KEEL_E2E_IMAGE=<tag>` uses an existing image instead of building one.
//! - `KEEL_E2E_KEEP=1` leaves stacks running for inspection.
//! - Service logs of every stack are saved under `target/e2e-logs/`.
//!
//! Stacks run one at a time: they share a subnet, and a small Docker VM has
//! memory for one cluster, not several.

use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use keel_control::keelconfig::Keelconfig;
use keel_control::ControlRequest;

/// Subnet every stack uses; tests pick fixed addresses inside it.
pub const SUBNET: &str = "172.29.83.0/24";

/// Name prefix of every compose project the harness creates.
const PROJECT_PREFIX: &str = "keel-e2e-";

// Image

static IMAGE: OnceLock<Result<String, String>> = OnceLock::new();

/// The keel image under test: `KEEL_E2E_IMAGE`, or built once per test binary
/// from the working tree with tests/e2e/Dockerfile.
pub fn image() -> Result<String> {
    IMAGE
        .get_or_init(|| {
            if let Ok(tag) = std::env::var("KEEL_E2E_IMAGE") {
                return Ok(tag);
            }
            let tag = "keel-e2e:dev".to_owned();
            let root = repo_root();
            eprintln!("keel-e2e: building {tag} from {}", root.display());
            let out = Command::new("docker")
                .args(["build", "-f", "tests/e2e/Dockerfile", "-t", &tag, "."])
                .current_dir(&root)
                .env("DOCKER_BUILDKIT", "1")
                .output()
                .map_err(|e| format!("cannot run docker: {e}"))?;
            if !out.status.success() {
                return Err(format!("docker build failed:\n{}", String::from_utf8_lossy(&out.stderr)));
            }
            Ok(tag)
        })
        .clone()
        .map_err(|e| anyhow::anyhow!(e))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repo root exists")
}

// Runtime

/// Starts, stops and inspects keel nodes. Docker compose is the only runtime
/// today; a process runtime (keel as host processes, for FreeBSD) implements
/// the same trait so the tests do not change.
pub trait Runtime {
    /// Run a command in the node's environment.
    fn exec(&self, node: &str, args: &[&str]) -> Result<Output>;
    /// Everything the node has logged so far, escape sequences removed.
    fn logs(&self, node: &str) -> Result<String>;
    fn stop(&self, node: &str) -> Result<()>;
    fn start(&self, node: &str) -> Result<()>;
    /// Host-reachable address of a port the node listens on.
    fn addr(&self, node: &str, port: u16) -> Result<SocketAddr>;
    /// Processes running in the node.
    fn procs(&self, node: &str) -> Result<Vec<Proc>>;
}

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub cmd: String,
}

static SERIAL: Mutex<()> = Mutex::new(());

/// One docker compose project. Brought down (containers, networks, volumes)
/// when dropped, also when the test failed.
pub struct Stack {
    project: String,
    dir: PathBuf,
    services: Vec<String>,
    _serial: MutexGuard<'static, ()>,
}

impl Stack {
    /// Write `files` and the compose file into a fresh directory and bring the
    /// project up. `{image}` in the compose text is replaced with [`image()`].
    pub fn up(name: &str, compose: &str, files: &[(&str, String)]) -> Result<Stack> {
        let image = image()?;
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        remove_stale_projects();

        let project = format!("{PROJECT_PREFIX}{name}");
        // Inside the repo: Docker can bind-mount it wherever it can build it.
        let dir = repo_root().join("target/e2e").join(&project);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        for (file, content) in files {
            std::fs::write(dir.join(file), content).with_context(|| format!("write {file}"))?;
        }
        std::fs::write(dir.join("compose.yaml"), compose.replace("{image}", &image))?;

        let mut stack = Stack { project, dir, services: Vec::new(), _serial: serial };
        let services = stack.compose(&["config", "--services"])?;
        stack.services = String::from_utf8_lossy(&services.stdout).lines().map(str::to_owned).collect();
        stack.compose(&["up", "--detach", "--quiet-pull"])?;
        Ok(stack)
    }

    /// Replace a file the stack was created with (a node's config, say);
    /// the node reads it on its next start.
    pub fn write_file(&self, file: &str, content: &str) -> Result<()> {
        std::fs::write(self.dir.join(file), content).with_context(|| format!("write {file}"))
    }

    /// Move a stopped service to another address on the stack's network.
    pub fn set_ip(&self, service: &str, ip: &str) -> Result<()> {
        let out = self.compose(&["ps", "--all", "--quiet", service])?;
        let container = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let network = format!("{}_net", self.project);
        for args in [
            vec!["network", "disconnect", network.as_str(), container.as_str()],
            vec!["network", "connect", "--ip", ip, network.as_str(), container.as_str()],
        ] {
            let out = Command::new("docker").args(&args).output().context("cannot run docker")?;
            if !out.status.success() {
                bail!("docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
            }
        }
        Ok(())
    }

    fn compose(&self, args: &[&str]) -> Result<Output> {
        let out = Command::new("docker")
            .args(["compose", "--project-name", &self.project, "--file"])
            .arg(self.dir.join("compose.yaml"))
            .args(args)
            .output()
            .context("cannot run docker compose")?;
        if !out.status.success() {
            bail!("docker compose {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out)
    }
}

impl Runtime for Stack {
    fn exec(&self, node: &str, args: &[&str]) -> Result<Output> {
        let mut full = vec!["exec", "--no-TTY", node];
        full.extend_from_slice(args);
        let out = Command::new("docker")
            .args(["compose", "--project-name", &self.project, "--file"])
            .arg(self.dir.join("compose.yaml"))
            .args(&full)
            .output()
            .context("cannot run docker compose exec")?;
        Ok(out)
    }

    fn logs(&self, node: &str) -> Result<String> {
        let out = self.compose(&["logs", "--no-color", "--no-log-prefix", node])?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok(strip_ansi(&text))
    }

    fn stop(&self, node: &str) -> Result<()> {
        self.compose(&["stop", node]).map(drop)
    }

    fn start(&self, node: &str) -> Result<()> {
        self.compose(&["start", node]).map(drop)
    }

    fn addr(&self, node: &str, port: u16) -> Result<SocketAddr> {
        let out = self.compose(&["port", node, &port.to_string()])?;
        let published = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let host_port = published
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .with_context(|| format!("{node}:{port} is not published ({published:?})"))?;
        Ok(SocketAddr::from(([127, 0, 0, 1], host_port)))
    }

    fn procs(&self, node: &str) -> Result<Vec<Proc>> {
        const SCRIPT: &str = r#"for d in /proc/[0-9]*; do
  [ -r "$d/status" ] || continue
  printf '@@%s\n' "${d#/proc/}"
  grep -E '^(PPid|Uid):' "$d/status"
  tr '\0' ' ' < "$d/cmdline"; echo
done 2>/dev/null"#;
        let out = self.exec(node, &["sh", "-c", SCRIPT])?;
        Ok(parse_procs(&String::from_utf8_lossy(&out.stdout)))
    }
}

impl Drop for Stack {
    /// Saves every service's log to `target/e2e-logs/<project>/`, whatever
    /// the outcome — a test that returns `Err` does not panic, so the harness
    /// cannot tell a failure from a pass — then brings the project down.
    fn drop(&mut self) {
        let log_dir = repo_root().join("target/e2e-logs").join(&self.project);
        let _ = std::fs::remove_dir_all(&log_dir);
        let _ = std::fs::create_dir_all(&log_dir);
        for service in &self.services {
            if let Ok(logs) = self.logs(service) {
                let _ = std::fs::write(log_dir.join(format!("{service}.log")), logs);
            }
        }
        eprintln!("keel-e2e: logs of {} in {}", self.project, log_dir.display());

        if std::env::var_os("KEEL_E2E_KEEP").is_some() {
            eprintln!("keel-e2e: KEEL_E2E_KEEP set, leaving project {} running", self.project);
            return;
        }
        let _ = self.compose(&["down", "--volumes", "--remove-orphans", "--timeout", "5"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Bring down projects a previous run left behind (killed test process, no
/// Drop): they would hold the subnet and the fixed addresses.
fn remove_stale_projects() {
    let Ok(out) = Command::new("docker").args(["compose", "ls", "--all", "--quiet"]).output() else {
        return;
    };
    for project in String::from_utf8_lossy(&out.stdout).lines() {
        if project.starts_with(PROJECT_PREFIX) {
            let _ = Command::new("docker")
                .args(["compose", "--project-name", project, "down", "--volumes", "--remove-orphans", "--timeout", "1"])
                .output();
        }
    }
}

fn parse_procs(text: &str) -> Vec<Proc> {
    let mut procs = Vec::new();
    for block in text.split("@@").skip(1) {
        let mut lines = block.lines();
        let Some(pid) = lines.next().and_then(|l| l.trim().parse().ok()) else { continue };
        let (mut ppid, mut uid, mut cmd) = (0, u32::MAX, String::new());
        for line in lines {
            if let Some(v) = line.strip_prefix("PPid:") {
                ppid = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("Uid:") {
                uid = v.split_whitespace().next().and_then(|u| u.parse().ok()).unwrap_or(u32::MAX);
            } else if !line.trim().is_empty() {
                cmd = line.trim().to_owned();
            }
        }
        procs.push(Proc { pid, ppid, uid, cmd });
    }
    procs
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ ... final byte in @..~
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

// Logs

/// Value of `key=value` in a tracing log line.
pub fn log_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key}=");
    let mut rest = line;
    while let Some(at) = rest.find(&pat) {
        let before = rest[..at].chars().last();
        let value = &rest[at + pat.len()..];
        if before.is_none_or(|c| c == ' ') {
            return Some(value.split_whitespace().next().unwrap_or("").trim_matches('"'));
        }
        rest = value;
    }
    None
}

// Waiting

/// Poll `f` every 250ms until it returns `Some`, or fail after `timeout`
/// with the last error it returned.
pub fn wait_until<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    let deadline = Instant::now() + timeout;
    let mut last_err = None;
    loop {
        match f() {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => {}
            Err(e) => last_err = Some(e),
        }
        if Instant::now() >= deadline {
            match last_err {
                Some(e) => bail!("timed out after {timeout:?} waiting for {what}: {e:#}"),
                None => bail!("timed out after {timeout:?} waiting for {what}"),
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// HTTP

/// Minimal HTTP/1.1 GET: status code and body.
pub fn http_get(addr: SocketAddr, host: &str, path: &str) -> Result<(u16, String)> {
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(sock, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")?;
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("no HTTP status in response: {:?}", text.lines().next()))?;
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default();
    Ok((status, body))
}

// Control protocol (keelctl's mTLS)

/// The fixed TLS identity of every remote control listener.
const CONTROL_SERVER_NAME: &str = "keel-control";

/// Operator credentials from `keel credentials create`, used against any
/// node's published remote control port.
pub struct Control {
    kc: Keelconfig,
}

impl Control {
    pub fn from_keelconfig(yaml: &str) -> Result<Control> {
        Ok(Control { kc: Keelconfig::from_yaml(yaml)? })
    }

    /// Send one request and return its `data`, or the error the node answered.
    pub fn request(&self, endpoint: SocketAddr, request: &ControlRequest) -> Result<serde_json::Value> {
        let mut stream = self.connect(endpoint, true)?;
        keel_control::client::one_shot(&mut stream, request)
    }

    /// Same request without a client certificate; the node must refuse it.
    pub fn request_without_client_cert(&self, endpoint: SocketAddr, request: &ControlRequest) -> Result<serde_json::Value> {
        let mut stream = self.connect(endpoint, false)?;
        keel_control::client::one_shot(&mut stream, request)
    }

    fn connect(
        &self,
        endpoint: SocketAddr,
        client_cert: bool,
    ) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut Cursor::new(self.kc.ca_cert.as_bytes())) {
            roots.add(cert?)?;
        }
        let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
        let config = if client_cert {
            let certs = rustls_pemfile::certs(&mut Cursor::new(self.kc.client_cert.as_bytes()))
                .collect::<Result<Vec<_>, _>>()?;
            let key = rustls_pemfile::private_key(&mut Cursor::new(self.kc.client_key.as_bytes()))?
                .context("no private key in keelconfig")?;
            builder.with_client_auth_cert(certs, key)?
        } else {
            builder.with_no_client_auth()
        };
        let name = rustls::pki_types::ServerName::try_from(CONTROL_SERVER_NAME)?;
        let conn = rustls::ClientConnection::new(Arc::new(config), name)?;
        let addr = endpoint.to_socket_addrs()?.next().context("no address")?;
        let sock = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        sock.set_read_timeout(Some(Duration::from_secs(40)))?;
        Ok(rustls::StreamOwned::new(conn, sock))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_blocks() {
        let text = "@@1\nPPid:\t0\nUid:\t0\t0\t0\t0\n/sbin/tini -- keel \n@@7\nPPid:\t1\nUid:\t65534\t65534\t65534\t65534\n/usr/local/bin/keel --config x \n";
        let procs = parse_procs(text);
        assert_eq!(procs.len(), 2);
        assert_eq!((procs[1].pid, procs[1].ppid, procs[1].uid), (7, 1, 65534));
        assert!(procs[1].cmd.starts_with("/usr/local/bin/keel"));
    }

    #[test]
    fn reads_log_fields() {
        let line = "2026-09-24T10:00:00Z  INFO keel::process: master: worker started pid=12 index=1";
        assert_eq!(log_field(line, "pid"), Some("12"));
        assert_eq!(log_field(line, "index"), Some("1"));
        assert_eq!(log_field("xpid=3 pid=4", "pid"), Some("4"));
        assert_eq!(log_field(line, "missing"), None);
    }

    #[test]
    fn strips_escape_sequences() {
        assert_eq!(strip_ansi("\u{1b}[32m INFO\u{1b}[0m keel"), " INFO keel");
    }
}
