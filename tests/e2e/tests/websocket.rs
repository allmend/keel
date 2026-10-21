//! WebSocket through Keel: the upgrade works on plain and TLS listeners, an
//! open socket is a connection to its backend (it counts, and holds a drain
//! open), the cache stays out of it, the access log entry is written when it
//! closes, and shutdown closes it after `grace_period`.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use keel_e2e::{http_get, tls_connect, wait_until, Runtime, Stack};

const COMPOSE: &str = r#"
networks:
  net:
    ipam:
      config:
        - subnet: 172.29.91.0/24
services:
  backend:
    image: traefik/whoami
    networks: { net: { ipv4_address: 172.29.91.20 } }
  keel:
    image: {image}
    init: true
    environment: [NO_COLOR=1]
    volumes: ["./node.hcl:/etc/keel/node.hcl:ro", "./config:/etc/keel/config:ro"]
    ports: ["80", "443", "10790"]
    networks: { net: { ipv4_address: 172.29.91.10 } }
"#;

const NODE: &str = r#"
keel {
  workers = 2
  user    = "nobody"
  group   = "nogroup"
}

metrics {
  address = "0.0.0.0:10790"
}
"#;

// Caching is on for the vhost: an upgrade must never be stored or served
// from it.
const CONFIG: &str = r#"
keel {
  grace_period = "2s"
}

listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}
listener "https" {
  protocol  = "https"
  addresses = ["0.0.0.0"]
  port      = 443
}

access_log {
  dir = "-"
}

cache {
  memory = "8M"
}

pool "web" {
  backend "172.29.91.20:80" {}
}

vhost "ws.test" {
  pool = "web"

  tls {
    cert = "certs/ws.crt"
    key  = "certs/ws.key"
  }

  cache {
    enabled = true
    ttl     = "1m"
  }
}
"#;

const BACKEND: &str = "172.29.91.20:80";

/// Upgrade `stream` to a WebSocket on whoami's echo endpoint; the response
/// head.
fn upgrade(stream: &mut impl ReadWrite) -> Result<String> {
    write!(
        stream,
        "GET /echo HTTP/1.1\r\nHost: ws.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )?;
    stream.flush()?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    if !head.starts_with("HTTP/1.1 101") {
        bail!("no upgrade: {head}");
    }
    Ok(head)
}

/// Send one masked frame (`opcode`, short payload) as a client must.
fn send(stream: &mut impl Write, opcode: u8, payload: &[u8]) -> Result<()> {
    let mask = [0x12, 0x34, 0x56, 0x78];
    let mut frame = vec![0x80 | opcode, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    stream.write_all(&frame)?;
    Ok(stream.flush()?)
}

/// Read one unmasked short frame: opcode and payload.
fn receive(stream: &mut impl Read) -> Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head)?;
    let mut payload = vec![0u8; (head[1] & 0x7f) as usize];
    stream.read_exact(&mut payload)?;
    Ok((head[0] & 0x0f, payload))
}

/// A text message out and its echo back.
fn echo(stream: &mut (impl Read + Write), text: &str) -> Result<()> {
    send(stream, 0x1, text.as_bytes())?;
    let (opcode, payload) = receive(stream)?;
    assert_eq!((opcode, payload.as_slice()), (0x1, text.as_bytes()));
    Ok(())
}

/// Close handshake: our close frame, the backend's in reply.
fn close(stream: &mut (impl Read + Write)) -> Result<()> {
    send(stream, 0x8, &[0x03, 0xe8])?;
    assert_eq!(receive(stream)?.0, 0x8);
    Ok(())
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// The value of the first sample line starting with `series`.
fn sample(stack: &Stack, series: &str) -> Result<f64> {
    let (status, body) = http_get(stack.addr("keel", 10790)?, "metrics", "/metrics")?;
    anyhow::ensure!(status == 200, "GET /metrics: {status}");
    let line = body.lines().find(|l| l.starts_with(series)).ok_or_else(|| anyhow::anyhow!("no {series} in:\n{body}"))?;
    Ok(line.rsplit(' ').next().unwrap_or_default().parse()?)
}

fn wait_for_sample(stack: &Stack, series: &str, want: f64) -> Result<()> {
    wait_until(&format!("{series} = {want}"), Duration::from_secs(10), || Ok((sample(stack, series)? == want).then_some(())))
}

#[test]
#[ignore = "needs Docker"]
fn websockets_pass_through_count_as_connections_and_are_logged_on_close() -> Result<()> {
    let key = rcgen::KeyPair::generate()?;
    let cert = rcgen::CertificateParams::new(vec!["ws.test".to_owned()])?.self_signed(&key)?;
    let stack = Stack::up(
        "websocket",
        COMPOSE,
        &[
            ("node.hcl", NODE.to_owned()),
            ("config/keel.hcl", CONFIG.to_owned()),
            ("config/certs/ws.crt", cert.pem()),
            ("config/certs/ws.key", key.serialize_pem()),
        ],
    )?;
    let plain = stack.addr("keel", 80)?;
    wait_until("HTTP 200 through keel", Duration::from_secs(60), || {
        Ok(http_get(plain, "ws.test", "/").ok().filter(|(status, _)| *status == 200))
    })?;
    let active = format!(r#"keel_active_connections{{pool="web",backend="{BACKEND}"}}"#);
    let drain_state = format!(r#"keel_backend_drain_state{{pool="web",backend="{BACKEND}"}}"#);

    // Plain listener, twice: the second upgrade reaches the backend too,
    // and the cache never answers.
    for _ in 0..2 {
        let mut ws = std::net::TcpStream::connect(plain)?;
        ws.set_read_timeout(Some(Duration::from_secs(5)))?;
        let head = upgrade(&mut ws)?;
        assert!(!head.to_ascii_lowercase().contains("x-cache"), "{head}");
        echo(&mut ws, "hello")?;
        close(&mut ws)?;
    }

    // TLS listener, HTTP/1.1 offered: held open for a second, it counts as
    // a connection and keeps a drain from completing.
    let mut ws = tls_connect(stack.addr("keel", 443)?, "ws.test", &[b"http/1.1"])?;
    upgrade(&mut ws)?;
    let opened = Instant::now();
    echo(&mut ws, "over tls")?;
    wait_for_sample(&stack, &active, 1.0)?;
    let out = stack.exec("keel", &["keel", "backend", "drain", BACKEND])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_sample(&stack, &drain_state, 1.0)?;
    std::thread::sleep(Duration::from_secs(1));
    echo(&mut ws, "still open while draining")?;
    assert_eq!(sample(&stack, &drain_state)?, 1.0, "draining, not removed, while the socket is open");
    close(&mut ws)?;
    ws.conn.send_close_notify();
    ws.flush()?;
    drop(ws);
    let held = opened.elapsed();
    wait_for_sample(&stack, &active, 0.0)?;
    wait_for_sample(&stack, &drain_state, 2.0)?;

    // A client gone without closing: its side failed, not the backend's.
    let out = stack.exec("keel", &["keel", "backend", "activate", BACKEND])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_sample(&stack, &drain_state, 0.0)?;
    let mut ws = tls_connect(stack.addr("keel", 443)?, "ws.test", &[b"http/1.1"])?;
    upgrade(&mut ws)?;
    echo(&mut ws, "gone")?;
    drop(ws);

    // One access entry per socket, written when it closed.
    let (logs, entries) = wait_until("four entries", Duration::from_secs(10), || {
        let logs = stack.logs("keel")?;
        let entries: Vec<serde_json::Value> = logs
            .lines()
            .filter(|l| l.contains(r#""uri":"/echo""#))
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        Ok((entries.len() == 4).then_some((logs, entries)))
    })?;
    assert_eq!(entries[3]["error"].as_str(), Some("downstream"), "{logs}");
    for e in &entries[..3] {
        assert_eq!((e["status"].as_u64(), e["pool"].as_str(), e["error"].as_str()), (Some(101), Some("web"), None), "{e}");
        assert!(e["bytes_in"].as_u64() > Some(0) && e["bytes_out"].as_u64() > Some(0), "{e}");
    }
    let tls = &entries[2];
    assert_eq!(tls["tls"].as_bool(), Some(true), "{tls}");
    let duration = tls["duration_ms"].as_f64().unwrap_or_default();
    assert!(duration >= 1000.0 && duration <= held.as_secs_f64() * 1000.0 + 500.0, "{tls}");

    // Shutdown: an open socket keeps working, then is closed.
    let mut ws = tls_connect(stack.addr("keel", 443)?, "ws.test", &[b"http/1.1"])?;
    upgrade(&mut ws)?;
    // Taken after the signal is delivered, so a little under grace_period.
    stack.signal("keel", "TERM")?;
    let signalled = Instant::now();
    std::thread::sleep(Duration::from_millis(500));
    echo(&mut ws, "after the signal")?;
    ws.sock.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut rest = Vec::new();
    let _ = ws.read_to_end(&mut rest);
    let closed = signalled.elapsed();
    assert!(closed >= Duration::from_millis(1500) && closed < Duration::from_secs(5), "closed {closed:?} after SIGTERM, grace_period 2s");
    Ok(())
}
