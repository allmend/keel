use crate::config::AccessLogConfig;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often an open log file is checked against its path, to notice
/// rotation.
const ROTATION_CHECK: Duration = Duration::from_secs(1);

#[derive(Serialize)]
pub struct AccessLogEntry {
    pub timestamp: String,
    /// Always `http`; TCP and UDP entries say `tcp`, `tls` or `udp`.
    pub r#type: &'static str,
    pub method: String,
    pub uri: String,
    pub protocol: String,
    pub status: u16,
    pub client_addr: Option<String>,
    pub vhost: String,
    pub pool: String,
    pub backend_addr: Option<String>,
    pub bytes_in: usize,
    pub bytes_out: usize,
    pub duration_ms: f64,
    pub backend_duration_ms: Option<f64>,
    pub user_agent: Option<String>,
    pub tls: bool,
    pub error: Option<String>,
}

/// One entry per L4 TCP connection. No method/uri/status/vhost — those are
/// HTTP concepts with no meaning at L4; routing is pool-based. In passthrough
/// mode the stream is opaque, so the TLS fields stay null.
#[derive(Serialize)]
pub struct TcpLogEntry {
    pub timestamp: String,
    pub r#type: &'static str,
    pub client_addr: Option<String>,
    pub listener: String,
    pub pool: String,
    pub backend_addr: Option<String>,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub duration_ms: f64,
    /// Keel terminated TLS on this connection (`tls_mode` terminate or
    /// reencrypt); the `tls_*` fields are then filled from the handshake.
    pub tls: bool,
    pub tls_sni: Option<String>,
    pub tls_version: Option<String>,
    pub tls_cipher: Option<String>,
    pub error: Option<String>,
}

/// One entry per UDP flow (client `ip:port` → backend), written when the flow
/// expires or is closed. Datagram counts have no TCP/HTTP equivalent; there
/// are no TLS fields because UDP carries none in v1.
#[derive(Serialize)]
pub struct UdpLogEntry {
    pub timestamp: String,
    pub r#type: &'static str,
    pub client_addr: String,
    pub listener: String,
    pub pool: String,
    pub backend_addr: Option<String>,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub packets_in: u64,
    pub packets_out: u64,
    pub duration_ms: f64,
    pub error: Option<String>,
}

pub struct AccessLogger {
    enabled: bool,
    dir: String,
    files: Mutex<HashMap<String, Open>>,
}

/// A log file Keel writes, and the inode it was opened as.
struct Open {
    file: File,
    ino: u64,
    checked: Instant,
}

impl AccessLogger {
    pub fn new(cfg: &AccessLogConfig) -> Self {
        if cfg.enabled && cfg.dir != "-" {
            if let Err(e) = std::fs::create_dir_all(&cfg.dir) {
                tracing::error!(dir = cfg.dir, error = %e, "failed to create access log directory");
            }
        }
        Self {
            enabled: cfg.enabled,
            dir: cfg.dir.clone(),
            files: Mutex::new(HashMap::new()),
        }
    }

    /// Whether entries are written at all; callers skip building them when not.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn log(&self, entry: &AccessLogEntry) {
        if !self.enabled {
            return;
        }

        let line = match serde_json::to_string(entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "access log serialization failed");
                return;
            }
        };

        if self.dir == "-" {
            println!("{line}");
            return;
        }

        let vhost = sanitize_vhost(&entry.vhost);
        self.write_to(&format!("access_{vhost}.log"), &line);
        if entry.error.is_some() {
            self.write_to(&format!("error_{vhost}.log"), &line);
        }
    }

    /// One file per pool: `access_tcp_<pool>.log`. L4 has no vhost concept.
    pub fn log_tcp(&self, entry: &TcpLogEntry) {
        if !self.enabled {
            return;
        }
        let line = match serde_json::to_string(entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "tcp access log serialization failed");
                return;
            }
        };
        if self.dir == "-" {
            println!("{line}");
            return;
        }
        // Pool names come from validated config, sanitized as a final guard.
        self.write_to(&format!("access_tcp_{}.log", sanitize_vhost(&entry.pool)), &line);
    }

    /// One file per pool: `access_udp_<pool>.log`. One entry per flow.
    pub fn log_udp(&self, entry: &UdpLogEntry) {
        if !self.enabled {
            return;
        }
        let line = match serde_json::to_string(entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "udp access log serialization failed");
                return;
            }
        };
        if self.dir == "-" {
            println!("{line}");
            return;
        }
        self.write_to(&format!("access_udp_{}.log", sanitize_vhost(&entry.pool)), &line);
    }

    /// Append one line. A file renamed or removed under Keel (logrotate)
    /// is noticed within a second and reopened at its path; nothing to signal.
    fn write_to(&self, filename: &str, line: &str) {
        // filename is always built from a sanitized vhost — see sanitize_vhost.
        let path = format!("{}/{}", self.dir, filename);
        let now = Instant::now();
        let mut files = self.files.lock().unwrap();
        if let Some(open) = files.get_mut(filename) {
            if now.duration_since(open.checked) >= ROTATION_CHECK {
                open.checked = now;
                if std::fs::metadata(&path).map(|m| m.ino()).ok() != Some(open.ino) {
                    files.remove(filename);
                }
            }
        }
        if !files.contains_key(filename) {
            let opened = OpenOptions::new().create(true).append(true).open(&path);
            match opened.and_then(|file| Ok(Open { ino: file.metadata()?.ino(), file, checked: now })) {
                Ok(open) => {
                    files.insert(filename.to_owned(), open);
                }
                Err(e) => {
                    tracing::error!(path = path, error = %e, "failed to open access log file");
                    return;
                }
            }
        }
        if let Some(open) = files.get_mut(filename) {
            let _ = writeln!(open.file, "{line}");
        }
    }
}

/// Reduce a vhost label to a safe filename component. The proxy already maps
/// requests to a bounded set of configured hosts, but this is a final guard so
/// no path separator, `..`, or control character can ever reach the filesystem.
fn sanitize_vhost(vhost: &str) -> String {
    if vhost.is_empty() {
        return "unknown".to_owned();
    }
    let cleaned: String = vhost
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '*') { c } else { '_' })
        .collect();
    // A name consisting only of dots ("." / "..") would be filesystem-unsafe.
    if cleaned.chars().all(|c| c == '.') {
        "unknown".to_owned()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rotated_file_is_reopened_at_its_path() {
        let dir = std::env::temp_dir().join(format!("keel-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logger = AccessLogger::new(&AccessLogConfig { enabled: true, dir: dir.display().to_string() });
        logger.write_to("access_a.log", "one");
        std::fs::rename(dir.join("access_a.log"), dir.join("access_a.log.1")).unwrap();
        logger.write_to("access_a.log", "two");
        std::thread::sleep(ROTATION_CHECK);
        logger.write_to("access_a.log", "three");
        assert_eq!(std::fs::read_to_string(dir.join("access_a.log.1")).unwrap(), "one\ntwo\n", "until the check");
        assert_eq!(std::fs::read_to_string(dir.join("access_a.log")).unwrap(), "three\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_traversal() {
        // Dots are kept (valid in hostnames) but every path separator becomes '_',
        // so no traversal sequence can survive.
        let cleaned = sanitize_vhost("../../etc/cron.d/x");
        assert!(!cleaned.contains('/'));
        assert_eq!(cleaned, ".._.._etc_cron.d_x");
        assert_eq!(sanitize_vhost("a/b"), "a_b");
        // A name that is only dots would be filesystem-unsafe ("." / "..").
        assert_eq!(sanitize_vhost(".."), "unknown");
        assert_eq!(sanitize_vhost(""), "unknown");
        // Control characters and null bytes are scrubbed.
        assert_eq!(sanitize_vhost("a\nb\0c"), "a_b_c");
    }

    #[test]
    fn keeps_normal_hosts() {
        assert_eq!(sanitize_vhost("api.example.com"), "api.example.com");
        assert_eq!(sanitize_vhost("*"), "*");
        assert_eq!(sanitize_vhost("unmatched"), "unmatched");
    }
}
