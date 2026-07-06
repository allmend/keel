//! Synchronous control client: sends one request over any `Read + Write`
//! stream and renders the response for a terminal. Used by `keel` (over the
//! local Unix socket) and `keelctl` (over mTLS TCP).

use std::io::{BufRead, BufReader, Read, Write};

use anyhow::Result;

use crate::ControlRequest;

/// Send a request and return the first response's `data`, or the error.
pub fn one_shot<S: Read + Write>(stream: &mut S, request: &ControlRequest) -> Result<serde_json::Value> {
    let json = serde_json::to_string(request)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let val: serde_json::Value = serde_json::from_str(line.trim())?;
    require_ok(val)
}

fn require_ok(val: serde_json::Value) -> Result<serde_json::Value> {
    if val["ok"].as_bool() != Some(true) {
        let err = val["error"].as_str().unwrap_or("unknown error");
        anyhow::bail!("{err}");
    }
    Ok(val["data"].clone())
}

// Commands (print human output to stdout)

pub fn status<S: Read + Write>(stream: &mut S) -> Result<()> {
    let data = one_shot(stream, &ControlRequest::Status)?;

    let uptime = data["uptime_secs"].as_u64().unwrap_or(0);
    println!("keel — uptime {}", format_duration(uptime));

    if let Some(pools) = data["pools"].as_array() {
        if pools.is_empty() {
            println!("No pools configured.");
            return Ok(());
        }
        for pool in pools {
            let name = pool["name"].as_str().unwrap_or("?");
            let backends = pool["backends"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
            println!(
                "\n  {} ({} backend{})",
                name,
                backends.len(),
                if backends.len() == 1 { "" } else { "s" }
            );
            print_backends(backends);
        }
    }
    Ok(())
}

pub fn backend_list<S: Read + Write>(stream: &mut S, pool: &str) -> Result<()> {
    let data = one_shot(stream, &ControlRequest::BackendList { pool: pool.to_owned() })?;

    let name = data["pool"].as_str().unwrap_or(pool);
    let backends = data["backends"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    println!(
        "Pool: {} ({} backend{})",
        name,
        backends.len(),
        if backends.len() == 1 { "" } else { "s" }
    );
    print_backends(backends);
    Ok(())
}

pub fn backend_drain<S: Read + Write>(stream: &mut S, address: &str, wait: bool) -> Result<()> {
    let request = ControlRequest::BackendDrain { address: address.to_owned(), wait };
    let json = serde_json::to_string(&request)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let started = std::time::Instant::now();
    let reader = BufReader::new(stream);
    let mut first = true;

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(&line)?;

        if val["ok"].as_bool() != Some(true) {
            let err = val["error"].as_str().unwrap_or("unknown error");
            anyhow::bail!("{err}");
        }

        let data = &val["data"];
        if first {
            // Initial response: lists the pools affected
            let pools: Vec<&str> = data["pools"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            println!("Draining {} from pools: {}", address, pools.join(", "));
            if data["done"].as_bool() == Some(true) {
                println!("Drain complete.");
                return Ok(());
            }
            first = false;
        } else {
            // Streaming status update
            let conns = data["connections"].as_i64().unwrap_or(0);
            print!("\r  connections: {conns}   ");
            let _ = std::io::stdout().flush();

            if data["done"].as_bool() == Some(true) {
                println!();
                println!("Drain complete ({:.0}s elapsed).", started.elapsed().as_secs_f64());
                return Ok(());
            }
        }
    }

    Ok(())
}

pub fn cluster_status<S: Read + Write>(stream: &mut S) -> Result<()> {
    let data = one_shot(stream, &ControlRequest::ClusterStatus)?;

    let role = data["role"].as_str().unwrap_or("?");
    let node_id = data["node_id"].as_u64().unwrap_or(0);
    let term = data["term"].as_u64().unwrap_or(0);
    let leader = data["leader_id"].as_u64();
    let committed = data["last_committed"].as_u64().unwrap_or(0);

    println!("Cluster:");
    println!("  Node ID:   {node_id}");
    println!("  Role:      {role}");
    println!("  Term:      {term}");
    println!("  Leader:    {}", leader.map(|l| l.to_string()).unwrap_or_else(|| "none".to_owned()));
    println!("  Committed: {committed}");
    if let Some(nodes) = data["membership"].as_array() {
        println!("  Members:");
        for n in nodes {
            println!(
                "    [{}] {}  ({})",
                n["id"].as_u64().unwrap_or(0),
                n["addr"].as_str().unwrap_or("?"),
                n["role"].as_str().unwrap_or("?"),
            );
        }
    }
    Ok(())
}

/// Commands whose response is a single `message` string.
pub fn message<S: Read + Write>(stream: &mut S, request: &ControlRequest) -> Result<()> {
    let data = one_shot(stream, request)?;
    println!("{}", data["message"].as_str().unwrap_or("done"));
    Ok(())
}

// Rendering helpers

fn print_backends(backends: &[serde_json::Value]) {
    for b in backends {
        let addr = b["address"].as_str().unwrap_or("?");
        let state = b["state"].as_str().unwrap_or("?");
        let conns = b["connections"].as_i64().unwrap_or(0);
        println!("    {:<25}  {:<10}  {} conn", addr, state, conns);
    }
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}
