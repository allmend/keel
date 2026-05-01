use crate::config::AccessLogConfig;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::sync::Mutex;

#[derive(Serialize)]
pub struct AccessLogEntry {
    pub timestamp: String,
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

pub struct AccessLogger {
    enabled: bool,
    dir: String,
    files: Mutex<HashMap<String, BufWriter<File>>>,
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

        let vhost = if entry.vhost.is_empty() { "unknown" } else { &entry.vhost };
        self.write_to(&format!("access_{vhost}.log"), &line);
        if entry.error.is_some() {
            self.write_to(&format!("error_{vhost}.log"), &line);
        }
    }

    fn write_to(&self, filename: &str, line: &str) {
        let mut files = self.files.lock().unwrap();
        if !files.contains_key(filename) {
            let path = format!("{}/{}", self.dir, filename);
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => {
                    files.insert(filename.to_owned(), BufWriter::new(f));
                }
                Err(e) => {
                    tracing::error!(path = path, error = %e, "failed to open access log file");
                    return;
                }
            }
        }
        if let Some(writer) = files.get_mut(filename) {
            let _ = writeln!(writer, "{line}");
            let _ = writer.flush();
        }
    }
}
