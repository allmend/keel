//! Master-side control plane: fan one operator command out to every worker
//! and merge the answers.
//!
//! Workers are forked processes, so a backend's drain state, its connection
//! counter and its health live in whichever worker observed them — there is
//! no shared registry. Each worker therefore binds its own control socket
//! (`worker-<index>.sock`, next to the instance socket) and only the master
//! binds `keel.control_socket`. Before this split every worker bound the same
//! path and the last one to start owned it: `keel status` reported a single
//! worker's connection counts, and `keel backend drain` left the other
//! workers sending new traffic to the backend.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use keel_control::{ControlRequest, ControlResponse};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

/// How long one worker gets to answer before the master leaves it out of the
/// merged result. Generous: a worker under load still answers in microseconds,
/// so hitting this means the worker is wedged, not busy.
const WORKER_TIMEOUT: Duration = Duration::from_secs(5);

/// A replacement worker rebuilds its pools from config, so it must be told
/// again what is draining. It binds its socket a moment after the fork.
const REPLAY_ATTEMPTS: u32 = 40;
const REPLAY_INTERVAL: Duration = Duration::from_millis(250);

/// The directory holding the sockets the workers create.
///
/// A subdirectory of the instance socket's directory, not the directory
/// itself: the workers need to create files here, so it belongs to the
/// unprivileged worker user, and anyone who can write a directory can unlink
/// what is in it. Keeping the master's control socket out of a
/// worker-writable directory stops a compromised worker from replacing it
/// and answering operator commands in the master's place.
pub fn worker_socket_dir(control_socket: &str) -> std::path::PathBuf {
    Path::new(control_socket)
        .parent()
        .unwrap_or_else(|| Path::new("/var/run/keel"))
        .join("workers")
}

/// The per-worker control socket for worker `index`.
pub fn worker_socket_path(control_socket: &str, index: usize) -> String {
    worker_socket_dir(control_socket)
        .join(format!("worker-{index}.sock"))
        .to_string_lossy()
        .into_owned()
}

/// What a drain actually achieved across the workers.
pub struct DrainOutcome {
    pub pools: Vec<String>,
    /// Workers that applied it.
    pub acknowledged: usize,
    /// Workers the master expected to reach.
    pub workers: usize,
}

pub struct WorkerFanout {
    sockets: Vec<String>,
    started_at: Instant,
    /// Backends drained through this master, replayed into any worker the
    /// master restarts. Drains outlive a worker crash — an operator draining
    /// a backend for a rolling upgrade would otherwise see traffic return to
    /// it the moment a worker was replaced.
    drained: Mutex<Vec<String>>,
}

impl WorkerFanout {
    pub fn new(control_socket: &str, workers: usize) -> Self {
        WorkerFanout {
            sockets: (0..workers).map(|i| worker_socket_path(control_socket, i)).collect(),
            started_at: Instant::now(),
            drained: Mutex::new(Vec::new()),
        }
    }

    /// Send one request to every worker, in parallel. Workers that are down
    /// (mid-restart) or wedged are skipped with a warning rather than failing
    /// the whole command — a partial answer beats no answer for an operator
    /// who is trying to see what is going on.
    async fn ask_all(&self, request: &ControlRequest) -> Vec<ControlResponse> {
        let line = match serde_json::to_string(request) {
            Ok(l) => l,
            Err(e) => {
                warn!(error = %e, "control: cannot encode request for workers");
                return Vec::new();
            }
        };
        let mut calls = Vec::with_capacity(self.sockets.len());
        for path in &self.sockets {
            let (path, line) = (path.clone(), line.clone());
            calls.push(tokio::spawn(async move {
                let result = ask_one(&path, &line).await;
                (path, result)
            }));
        }

        let mut responses = Vec::with_capacity(calls.len());
        for call in calls {
            match call.await {
                Ok((_, Ok(response))) => responses.push(response),
                Ok((path, Err(e))) => {
                    warn!(worker = %path, error = %e, "control: worker did not answer")
                }
                Err(e) => warn!(error = %e, "control: worker query task failed"),
            }
        }
        responses
    }

    pub async fn status(&self) -> String {
        let responses = self.ask_all(&ControlRequest::Status).await;
        if responses.is_empty() {
            return ControlResponse::err("no worker answered");
        }
        // Uptime is the master's: workers come and go under it, and an
        // operator asking how long the instance has been up means the
        // instance, not the youngest worker.
        ControlResponse::ok(json!({
            "uptime_secs": self.started_at.elapsed().as_secs(),
            "pools": merge_pools(&responses),
        }))
    }

    pub async fn backend_list(&self, pool: &str) -> String {
        let request = ControlRequest::BackendList { pool: pool.to_owned() };
        let responses = self.ask_all(&request).await;
        if responses.is_empty() {
            return ControlResponse::err("no worker answered");
        }
        // Only a pool that no worker knows is really missing. One worker can
        // legitimately disagree — it may still be on the config a reload has
        // already given the others.
        if responses.iter().all(|r| !r.ok) {
            return ControlResponse::err(
                first_error(&responses).unwrap_or_else(|| format!("pool '{pool}' not found")),
            );
        }
        let backends = merge_backends(
            responses
                .iter()
                .filter(|r| r.ok)
                .filter_map(|r| {
                    r.data.as_ref().and_then(|d| d.get("backends")).and_then(Value::as_array)
                })
                .map(Vec::as_slice),
        );
        ControlResponse::ok(json!({ "pool": pool, "backends": backends }))
    }

    pub async fn drain(&self, address: &str) -> Result<DrainOutcome, String> {
        let request = ControlRequest::BackendDrain { address: address.to_owned(), wait: false };
        let responses = self.ask_all(&request).await;
        if responses.is_empty() {
            return Err("no worker answered".into());
        }
        // An unknown address is an error from every worker; report it once.
        if responses.iter().all(|r| !r.ok) {
            return Err(first_error(&responses).unwrap_or_else(|| "drain failed".into()));
        }

        let mut pools: Vec<String> = Vec::new();
        for r in &responses {
            let names = r.data.as_ref().and_then(|d| d.get("pools")).and_then(Value::as_array);
            for name in names.into_iter().flatten().filter_map(Value::as_str) {
                if !pools.iter().any(|p| p == name) {
                    pools.push(name.to_owned());
                }
            }
        }

        {
            let mut drained = self.drained.lock().unwrap();
            if !drained.iter().any(|a| a == address) {
                drained.push(address.to_owned());
            }
        }
        // A worker that did not answer is still sending traffic to the
        // backend. The master will replay the drain if it restarts that
        // worker, but until then the drain is only partly applied and the
        // operator has to know.
        Ok(DrainOutcome {
            pools,
            acknowledged: responses.iter().filter(|r| r.ok).count(),
            workers: self.sockets.len(),
        })
    }

    /// Open connections to a backend, summed over the workers.
    ///
    /// `None` when not one worker answered: that is "unknown", not "zero".
    /// Reporting zero would tell `drain --wait` the backend is finished while
    /// connections are still open on it.
    pub async fn connections_for(&self, address: &str) -> Option<i64> {
        let responses = self.ask_all(&ControlRequest::Status).await;
        if responses.is_empty() {
            return None;
        }
        Some(responses.iter().map(|r| connections_in(r, address)).sum())
    }

    /// Re-apply the current drains to a worker the master just replaced.
    ///
    /// Waits for the replacement's socket to come up — Pingora takes about a
    /// second to bootstrap — and replays whatever is draining at that moment.
    /// A drain issued after the worker is reachable needs no replay: it goes
    /// out through the normal fan-out.
    pub async fn replay_drains(&self, index: usize) {
        let Some(path) = self.sockets.get(index) else { return };
        let probe = match serde_json::to_string(&ControlRequest::Status) {
            Ok(p) => p,
            Err(_) => return,
        };

        for _ in 0..REPLAY_ATTEMPTS {
            tokio::time::sleep(REPLAY_INTERVAL).await;
            if let Err(e) = ask_one(path, &probe).await {
                debug!(worker = %path, error = %e, "control: waiting for replacement worker");
                continue;
            }

            let drained = self.drained.lock().unwrap().clone();
            if drained.is_empty() {
                return;
            }
            for address in &drained {
                let request =
                    ControlRequest::BackendDrain { address: address.clone(), wait: false };
                let Ok(line) = serde_json::to_string(&request) else { continue };
                match ask_one(path, &line).await {
                    Ok(_) => info!(worker = %path, backend = %address, "control: re-applied drain to restarted worker"),
                    Err(e) => warn!(worker = %path, backend = %address, error = %e, "control: could not re-apply drain to restarted worker"),
                }
            }
            return;
        }
        warn!(
            worker = %path,
            "control: replacement worker never answered — drain state not re-applied"
        );
    }
}

/// One request, one response line, over one worker socket.
async fn ask_one(path: &str, line: &str) -> anyhow::Result<ControlResponse> {
    let exchange = async {
        let stream = UnixStream::connect(path).await?;
        let (reader, mut writer) = tokio::io::split(stream);
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut response = String::new();
        BufReader::new(reader).read_line(&mut response).await?;
        let parsed: ControlResponse = serde_json::from_str(response.trim())?;
        Ok::<_, anyhow::Error>(parsed)
    };
    tokio::time::timeout(WORKER_TIMEOUT, exchange)
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {WORKER_TIMEOUT:?}"))?
}

/// The first failure's message. A response can be `ok: false` with no text,
/// so fall back to a generic message rather than reporting no error at all.
fn first_error(responses: &[ControlResponse]) -> Option<String> {
    responses
        .iter()
        .find(|r| !r.ok)
        .map(|r| r.error.clone().unwrap_or_else(|| "worker reported a failure".into()))
}

/// Connections a single worker reports for one backend address, across pools.
fn connections_in(response: &ControlResponse, address: &str) -> i64 {
    let Some(pools) = response.data.as_ref().and_then(|d| d.get("pools")).and_then(Value::as_array)
    else {
        return 0;
    };
    pools
        .iter()
        .filter_map(|p| p.get("backends").and_then(Value::as_array))
        .flatten()
        .filter(|b| b.get("address").and_then(Value::as_str) == Some(address))
        .filter_map(|b| b.get("connections").and_then(Value::as_i64))
        .sum()
}

/// Merge the per-worker `pools` arrays of a `status` response into one.
fn merge_pools(responses: &[ControlResponse]) -> Vec<Value> {
    // Order follows the first worker that reported each pool. Workers emit
    // pools from a BTreeMap, so that is alphabetical; preserving their order
    // rather than re-sorting keeps the merge faithful to whatever the workers
    // send.
    let mut order: Vec<String> = Vec::new();
    let mut by_pool: HashMap<String, Vec<&Vec<Value>>> = HashMap::new();

    for response in responses {
        let Some(pools) =
            response.data.as_ref().and_then(|d| d.get("pools")).and_then(Value::as_array)
        else {
            continue;
        };
        for pool in pools {
            let Some(name) = pool.get("name").and_then(Value::as_str) else { continue };
            let Some(backends) = pool.get("backends").and_then(Value::as_array) else { continue };
            if !by_pool.contains_key(name) {
                order.push(name.to_owned());
            }
            by_pool.entry(name.to_owned()).or_default().push(backends);
        }
    }

    order
        .into_iter()
        .map(|name| {
            let backends = merge_backends(by_pool[&name].iter().copied().map(Vec::as_slice));
            json!({ "name": name, "backends": backends })
        })
        .collect()
}

/// Merge the same backend as seen by several workers into one entry.
fn merge_backends<'a>(views: impl Iterator<Item = &'a [Value]>) -> Vec<Value> {
    let mut order: Vec<String> = Vec::new();
    let mut merged: HashMap<String, Value> = HashMap::new();

    for backends in views {
        for backend in backends {
            let Some(address) = backend.get("address").and_then(Value::as_str) else { continue };
            match merged.get_mut(address) {
                None => {
                    order.push(address.to_owned());
                    merged.insert(address.to_owned(), backend.clone());
                }
                Some(existing) => fold_backend(existing, backend),
            }
        }
    }

    order.into_iter().filter_map(|a| merged.remove(&a)).collect()
}

/// Fold one worker's view of a backend into the merged entry.
fn fold_backend(into: &mut Value, other: &Value) {
    let sum = into.get("connections").and_then(Value::as_i64).unwrap_or(0)
        + other.get("connections").and_then(Value::as_i64).unwrap_or(0);
    into["connections"] = json!(sum);

    // Drain state: the least-drained view wins. Workers agree once a drain
    // has been fanned out; while they disagree — a worker restarted and is
    // being re-drained — "active" is the honest answer, because that worker
    // is still opening new connections to the backend.
    let other_state = other.get("state").and_then(Value::as_str).unwrap_or("unknown");
    if state_rank(other_state) < state_rank(into.get("state").and_then(Value::as_str).unwrap_or("unknown")) {
        into["state"] = json!(other_state);
    }

    // Health: the worst view wins, with its reason. Each worker probes
    // independently, and a backend that one worker will not route to is
    // degraded from the operator's point of view.
    let other_health = other.get("health").and_then(Value::as_str).unwrap_or("unchecked");
    if health_rank(other_health) < health_rank(into.get("health").and_then(Value::as_str).unwrap_or("unchecked")) {
        into["health"] = json!(other_health);
        into["health_reason"] = other.get("health_reason").cloned().unwrap_or(Value::Null);
    }
}

/// Lower = less drained.
fn state_rank(state: &str) -> u8 {
    match state {
        "active" => 0,
        "draining" => 1,
        "removed" => 2,
        _ => 3,
    }
}

/// Lower = worse.
fn health_rank(health: &str) -> u8 {
    match health {
        "ejected" => 0,
        "unhealthy" => 1,
        "healthy" => 2,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(pools: Value) -> ControlResponse {
        ControlResponse {
            ok: true,
            data: Some(json!({ "uptime_secs": 1, "pools": pools })),
            error: None,
        }
    }

    fn backend(address: &str, state: &str, conns: i64, health: &str) -> Value {
        json!({
            "address": address,
            "state": state,
            "connections": conns,
            "health": health,
            "health_reason": Value::Null,
        })
    }

    #[test]
    fn connections_sum_across_workers() {
        let a = resp(json!([{"name": "web", "backends": [backend("10.0.0.1:80", "active", 12, "healthy")]}]));
        let b = resp(json!([{"name": "web", "backends": [backend("10.0.0.1:80", "active", 30, "healthy")]}]));
        let merged = merge_pools(&[a, b]);
        assert_eq!(merged[0]["backends"][0]["connections"], json!(42));
    }

    #[test]
    fn least_drained_state_wins() {
        // A restarted worker reports "active" again; the drain is not complete.
        let a = resp(json!([{"name": "web", "backends": [backend("10.0.0.1:80", "draining", 3, "healthy")]}]));
        let b = resp(json!([{"name": "web", "backends": [backend("10.0.0.1:80", "active", 5, "healthy")]}]));
        let merged = merge_pools(&[a, b]);
        assert_eq!(merged[0]["backends"][0]["state"], json!("active"));
        assert_eq!(merged[0]["backends"][0]["connections"], json!(8));
    }

    #[test]
    fn worst_health_wins_with_its_reason() {
        let a = resp(json!([{"name": "web", "backends": [backend("10.0.0.1:80", "active", 1, "healthy")]}]));
        let mut unhealthy = backend("10.0.0.1:80", "active", 2, "unhealthy");
        unhealthy["health_reason"] = json!("connect refused");
        let b = resp(json!([{"name": "web", "backends": [unhealthy]}]));
        let merged = merge_pools(&[a, b]);
        assert_eq!(merged[0]["backends"][0]["health"], json!("unhealthy"));
        assert_eq!(merged[0]["backends"][0]["health_reason"], json!("connect refused"));
    }

    #[test]
    fn pool_and_backend_order_follows_first_worker() {
        let a = resp(json!([
            {"name": "web", "backends": [backend("10.0.0.2:80", "active", 1, "healthy"), backend("10.0.0.1:80", "active", 1, "healthy")]},
            {"name": "api", "backends": [backend("10.0.0.9:80", "active", 1, "healthy")]},
        ]));
        let b = resp(json!([
            {"name": "api", "backends": [backend("10.0.0.9:80", "active", 1, "healthy")]},
            {"name": "web", "backends": [backend("10.0.0.1:80", "active", 1, "healthy"), backend("10.0.0.2:80", "active", 1, "healthy")]},
        ]));
        let merged = merge_pools(&[a, b]);
        assert_eq!(merged[0]["name"], json!("web"));
        assert_eq!(merged[1]["name"], json!("api"));
        assert_eq!(merged[0]["backends"][0]["address"], json!("10.0.0.2:80"));
    }

    #[test]
    fn connections_for_address_ignores_other_backends() {
        let r = resp(json!([{"name": "web", "backends": [
            backend("10.0.0.1:80", "draining", 4, "healthy"),
            backend("10.0.0.2:80", "active", 9, "healthy"),
        ]}]));
        assert_eq!(connections_in(&r, "10.0.0.1:80"), 4);
    }

    /// A worker can answer `ok: false` without a message; losing that turned
    /// a failure into "no error" and let it be merged as data.
    #[test]
    fn first_error_falls_back_when_a_failure_carries_no_message() {
        let responses =
            vec![ControlResponse { ok: false, data: None, error: None }];
        assert_eq!(first_error(&responses).as_deref(), Some("worker reported a failure"));
    }

    /// Serve one canned response on a Unix socket, like a worker would.
    async fn fake_worker(path: String, response: String) {
        let listener = tokio::net::UnixListener::bind(&path).expect("bind fake worker");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let response = response.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = tokio::io::split(stream);
                    let mut line = String::new();
                    let mut reader = BufReader::new(reader);
                    let _ = reader.read_line(&mut line).await;
                    let _ = writer.write_all(response.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                    let _ = writer.flush().await;
                });
            }
        });
    }

    fn status_response(address: &str, conns: i64) -> String {
        ControlResponse::ok(json!({
            "uptime_secs": 1,
            "pools": [{"name": "web", "backends": [backend(address, "active", conns, "healthy")]}],
        }))
    }

    fn fanout_over(dir: &std::path::Path, workers: usize) -> WorkerFanout {
        WorkerFanout {
            sockets: (0..workers)
                .map(|i| dir.join(format!("w{i}.sock")).to_string_lossy().into_owned())
                .collect(),
            started_at: Instant::now(),
            drained: Mutex::new(Vec::new()),
        }
    }

    /// Reporting zero when nothing answered would tell `drain --wait` the
    /// backend is finished while it may still be carrying connections.
    #[tokio::test]
    async fn connections_are_unknown_rather_than_zero_when_no_worker_answers() {
        let dir = std::env::temp_dir().join(format!("keel-fanout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fanout = fanout_over(&dir, 2); // nothing is listening on either path

        assert_eq!(fanout.connections_for("10.0.0.1:80").await, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn connections_sum_over_the_workers_that_answer() {
        let dir = std::env::temp_dir().join(format!("keel-fanout-sum-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fanout = fanout_over(&dir, 3);
        fake_worker(fanout.sockets[0].clone(), status_response("10.0.0.1:80", 4)).await;
        fake_worker(fanout.sockets[1].clone(), status_response("10.0.0.1:80", 7)).await;
        // sockets[2] is never bound — an unreachable worker is skipped.

        assert_eq!(fanout.connections_for("10.0.0.1:80").await, Some(11));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A drain that reached only some workers must not read as fully applied:
    /// the others keep sending new connections to the backend.
    #[tokio::test]
    async fn drain_reports_how_many_workers_applied_it() {
        let dir = std::env::temp_dir().join(format!("keel-fanout-drain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fanout = fanout_over(&dir, 3);
        let ok = ControlResponse::ok(json!({ "pools": ["web"], "done": true }));
        fake_worker(fanout.sockets[0].clone(), ok.clone()).await;
        fake_worker(fanout.sockets[1].clone(), ok).await;
        // sockets[2] is down.

        let outcome = fanout.drain("10.0.0.1:80").await.expect("drain applied somewhere");
        assert_eq!(outcome.pools, vec!["web".to_owned()]);
        assert_eq!(outcome.acknowledged, 2);
        assert_eq!(outcome.workers, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn drain_fails_when_no_worker_answers() {
        let dir = std::env::temp_dir().join(format!("keel-fanout-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fanout = fanout_over(&dir, 2);
        assert!(fanout.drain("10.0.0.1:80").await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_sockets_live_in_a_subdirectory_of_the_instance_socket() {
        assert_eq!(
            worker_socket_path("/var/run/keel/keel.sock", 2),
            "/var/run/keel/workers/worker-2.sock"
        );
    }
}
