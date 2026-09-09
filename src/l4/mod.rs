//! L4 TCP proxying.
//!
//! A listener with `tcp_pool` proxies a TCP connection to a backend selected
//! from the named pool. `tls_mode` decides what Keel does with TLS:
//!
//! - **passthrough** (default): bytes are spliced untouched. If the client
//!   and backend speak TLS, they do so end to end and Keel holds no key.
//! - **terminate**: Keel terminates TLS with a certificate from the shared
//!   store (selected by SNI, falling back to the listener's `tls_host`) and
//!   forwards plaintext to the backend.
//! - **reencrypt**: as terminate, then a new TLS connection to the backend.
//!   The backend certificate is verified only with `tls_verify: true`, the
//!   NLB behaviour: wire encryption, not backend authentication.
//!
//! Backend selection, weights, health checks, drain, and connection counting
//! all come from the shared `PoolRegistry` — a TCP connection counts exactly
//! like an HTTP connection for `keel backend drain --wait`. The consistent
//! hashing key is the client IP:port, so a client keeps hitting the same
//! backend while the pool composition is stable.
//!
//! One access log entry is written per connection (not per request — there is
//! no request concept at L4) to `access_tcp_<pool>.log`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use pingora::apps::ServerApp;
use pingora::protocols::Stream;
use pingora::server::ShutdownWatch;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tracing::{debug, warn};

use crate::access_log::{AccessLogger, TcpLogEntry};
use crate::backend::PoolRegistry;
use crate::config::TlsMode;

pub struct TcpProxyApp {
    pub listener: String,
    pub pool: String,
    pub pools: Arc<PoolRegistry>,
    pub access_logger: Arc<AccessLogger>,
    pub mode: TlsMode,
    /// Present for terminate and reencrypt: serves the store's certificates.
    pub server_tls: Option<Arc<rustls::ServerConfig>>,
    /// Present for reencrypt: how Keel connects to the backend.
    pub client_tls: Option<Arc<rustls::ClientConfig>>,
    /// Configured hostname per resolved backend, used as SNI and, with
    /// `tls_verify`, as the name the backend certificate must match.
    pub backend_names: HashMap<SocketAddr, String>,
    /// Expect a PROXY protocol header before anything else on the stream.
    pub proxy_protocol: bool,
}

/// What one connection did, for accounting and the access log.
#[derive(Default)]
pub struct Outcome {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub error: Option<&'static str>,
    pub tls: Option<TlsInfo>,
}

/// Negotiated with the client when Keel terminated TLS.
#[derive(Default, Clone)]
pub struct TlsInfo {
    pub sni: Option<String>,
    pub version: Option<String>,
    pub cipher: Option<String>,
}

impl TcpProxyApp {
    /// Proxy one accepted connection to `backend` according to `mode`.
    /// Generic over the client stream so tests can drive it with plain
    /// `TcpStream`s instead of Pingora's `Stream`.
    pub async fn splice<S>(&self, client: S, backend: SocketAddr, shutdown: &ShutdownWatch) -> Outcome
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut out = Outcome::default();

        // Client side: terminate when configured, before touching the backend,
        // so a failed handshake never costs an upstream connection.
        let mut terminated = None;
        let mut plain = Some(client);
        if let Some(cfg) = &self.server_tls {
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::clone(cfg));
            match acceptor.accept(plain.take().unwrap()).await {
                Ok(tls) => {
                    let (_, conn) = tls.get_ref();
                    out.tls = Some(TlsInfo {
                        sni: conn.server_name().map(str::to_owned),
                        version: conn.protocol_version().map(|v| format!("{v:?}").replace('_', ".")),
                        cipher: conn.negotiated_cipher_suite().map(|c| format!("{:?}", c.suite())),
                    });
                    terminated = Some(tls);
                }
                Err(e) => {
                    debug!(pool = self.pool, error = %e, "tcp: client TLS handshake failed");
                    out.error = Some("tls_handshake");
                    return out;
                }
            }
        }

        let upstream = match TcpStream::connect(backend).await {
            Ok(s) => {
                self.pools.report_success(&self.pool, backend);
                s
            }
            Err(e) => {
                warn!(pool = self.pool, backend = %backend, error = %e, "tcp: upstream connect failed");
                self.pools.report_failure(&self.pool, backend, "connect");
                out.error = Some("upstream_connect");
                return out;
            }
        };

        // Backend side: re-encrypt when configured.
        let (bytes_in, bytes_out, error) = match (&self.client_tls, terminated, plain) {
            (Some(cfg), Some(mut client), _) => {
                let name = self.backend_server_name(backend);
                let connector = tokio_rustls::TlsConnector::from(Arc::clone(cfg));
                match connector.connect(name, upstream).await {
                    Ok(mut tls_upstream) => copy(&mut client, &mut tls_upstream, shutdown).await,
                    Err(e) => {
                        warn!(pool = self.pool, backend = %backend, error = %e, "tcp: upstream TLS handshake failed");
                        (0, 0, Some("upstream_tls"))
                    }
                }
            }
            (None, Some(mut client), _) => {
                let mut upstream = upstream;
                copy(&mut client, &mut upstream, shutdown).await
            }
            (_, None, Some(mut client)) => {
                let mut upstream = upstream;
                copy(&mut client, &mut upstream, shutdown).await
            }
            (_, None, None) => unreachable!("client stream is either terminated or plain"),
        };
        out.bytes_in = bytes_in;
        out.bytes_out = bytes_out;
        out.error = error;
        out
    }

    /// SNI for the backend: its configured hostname when it is one, else its IP.
    fn backend_server_name(&self, backend: SocketAddr) -> ServerName<'static> {
        self.backend_names
            .get(&backend)
            .and_then(|h| ServerName::try_from(h.clone()).ok())
            .unwrap_or_else(|| ServerName::IpAddress(backend.ip().into()))
    }

    fn log(&self, client_addr: Option<String>, backend: Option<SocketAddr>, out: &Outcome, started: std::time::Instant) {
        let tls = out.tls.clone().unwrap_or_default();
        self.access_logger.log_tcp(&TcpLogEntry {
            timestamp: Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            r#type: match self.mode {
                TlsMode::Passthrough => "tcp",
                TlsMode::Terminate | TlsMode::Reencrypt => "tls",
            },
            client_addr,
            listener: self.listener.clone(),
            pool: self.pool.clone(),
            backend_addr: backend.map(|b| b.to_string()),
            bytes_in: out.bytes_in,
            bytes_out: out.bytes_out,
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            tls: out.tls.is_some(),
            tls_sni: tls.sni,
            tls_version: tls.version,
            tls_cipher: tls.cipher,
            error: out.error.map(str::to_owned),
        });
    }
}

/// Splice until both directions have closed, an I/O error occurs, or the
/// server shuts down (graceful shutdown closes L4 connections — there is no
/// in-flight request boundary to wait for). Returns (client→backend bytes,
/// backend→client bytes, error).
async fn copy<A, B>(client: &mut A, upstream: &mut B, shutdown: &ShutdownWatch) -> (u64, u64, Option<&'static str>)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut ur, mut uw) = tokio::io::split(upstream);
    // Each direction half-closes its destination when its source ends, so a
    // client that finishes sending still receives the backend's tail.
    let c2u = async {
        let r = pump(&mut cr, &mut uw).await;
        let _ = uw.shutdown().await;
        r
    };
    let u2c = async {
        let r = pump(&mut ur, &mut cw).await;
        let _ = cw.shutdown().await;
        r
    };
    let mut shutdown = shutdown.clone();
    tokio::select! {
        ((bytes_in, e1), (bytes_out, e2)) = async { tokio::join!(c2u, u2c) } => (bytes_in, bytes_out, e1.or(e2)),
        _ = shutdown.changed() => (0, 0, Some("shutdown")),
    }
}

/// Copy one direction, counting bytes. A peer that drops the TCP connection
/// without a TLS close_notify surfaces as `UnexpectedEof` from rustls; many
/// TCP clients close that way, so it counts as a normal end, not an error.
async fn pump<R, W>(r: &mut R, w: &mut W) -> (u64, Option<&'static str>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = vec![0u8; 16 * 1024];
    let mut total = 0u64;
    loop {
        match r.read(&mut buf).await {
            Ok(0) => return (total, None),
            Ok(n) => {
                if let Err(e) = w.write_all(&buf[..n]).await {
                    debug!(error = %e, "tcp: write failed");
                    return (total, Some("io"));
                }
                total += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return (total, None),
            Err(e) => {
                debug!(error = %e, "tcp: read failed");
                return (total, Some("io"));
            }
        }
    }
}

#[async_trait]
impl ServerApp for TcpProxyApp {
    async fn process_new(
        self: &Arc<Self>,
        session: Stream,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        let started = std::time::Instant::now();
        let mut session = session;
        let mut client_addr = session
            .get_socket_digest()
            .and_then(|d| d.peer_addr().map(|a| a.to_string()));

        // The header comes before TLS on the wire, so it is read here, ahead
        // of any termination in `splice`.
        if self.proxy_protocol {
            match crate::proxy_protocol::read_from_stream(&mut session).await {
                Ok(Some((src, _))) => client_addr = Some(src.to_string()),
                Ok(None) => {}
                Err(e) => {
                    debug!(pool = self.pool, listener = self.listener, error = ?e, "tcp: PROXY header rejected");
                    crate::metrics::record_proxy_protocol_error(&self.listener);
                    crate::metrics::record_tcp_error(&self.pool, "proxy_protocol");
                    let out = Outcome { error: Some("proxy_protocol"), ..Default::default() };
                    self.log(client_addr, None, &out, started);
                    return None;
                }
            }
        }

        // Client address doubles as the consistent-hash key (session affinity).
        let key = client_addr.clone().unwrap_or_default();
        let Some(backend) = self.pools.select(&self.pool, key.as_bytes()) else {
            warn!(pool = self.pool, listener = self.listener, "tcp: no healthy backend");
            crate::metrics::record_tcp_error(&self.pool, "no_backend");
            let out = Outcome { error: Some("no_backend"), ..Default::default() };
            self.log(client_addr, None, &out, started);
            return None;
        };
        crate::metrics::record_tcp_connection(&self.pool, &backend.to_string());

        let out = self.splice(session, backend, shutdown).await;

        self.pools.release(&self.pool, backend);
        crate::metrics::add_tcp_bytes(&self.pool, &backend.to_string(), out.bytes_in, out.bytes_out);
        if let Some(reason) = out.error {
            crate::metrics::record_tcp_error(&self.pool, reason);
        }
        self.log(client_addr, Some(backend), &out, started);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::CertStore;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn self_signed(name: &str) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec![name.to_owned()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    /// Plain TCP echo backend.
    async fn echo_backend() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        addr
    }

    /// TLS echo backend presenting `cert_pem`.
    async fn tls_echo_backend(cert_pem: &str, key_pem: &str) -> SocketAddr {
        let store = CertStore::with_test_cert("backend.example.test", cert_pem, key_pem);
        let cfg = store.rustls_server_config(&[]).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (s, _) = l.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut tls) = acceptor.accept(s).await {
                        let (mut r, mut w) = tokio::io::split(&mut tls);
                        let _ = tokio::io::copy(&mut r, &mut w).await;
                    }
                });
            }
        });
        addr
    }

    fn app(mode: TlsMode, server_tls: Option<Arc<rustls::ServerConfig>>, client_tls: Option<Arc<rustls::ClientConfig>>, backend: SocketAddr) -> Arc<TcpProxyApp> {
        let pools = Arc::new(PoolRegistry::new(HashMap::new(), HashMap::new(), HashMap::new()));
        let access_logger = Arc::new(AccessLogger::new(&crate::config::AccessLogConfig { enabled: false, dir: "-".into() }));
        let mut backend_names = HashMap::new();
        backend_names.insert(backend, "backend.example.test".to_owned());
        Arc::new(TcpProxyApp {
            listener: "test".into(),
            pool: "p".into(),
            pools,
            access_logger,
            mode,
            server_tls,
            client_tls,
            backend_names,
            proxy_protocol: false,
        })
    }

    /// Accept one client on a fresh listener and run `splice` for it. The
    /// shutdown sender is returned so the caller keeps it alive: dropping it
    /// would read as a shutdown signal.
    async fn serve_one(
        app: Arc<TcpProxyApp>,
        backend: SocketAddr,
    ) -> (SocketAddr, tokio::task::JoinHandle<Outcome>, tokio::sync::watch::Sender<bool>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let h = tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            app.splice(s, backend, &rx).await
        });
        (addr, h, tx)
    }

    async fn tls_client(addr: SocketAddr, sni: &'static str) -> tokio_rustls::client::TlsStream<TcpStream> {
        let tcp = TcpStream::connect(addr).await.unwrap();
        let name = ServerName::try_from(sni).unwrap();
        tokio_rustls::TlsConnector::from(crate::tls::insecure_client_config()).connect(name, tcp).await.unwrap()
    }

    #[tokio::test]
    async fn passthrough_splices_bytes() {
        let backend = echo_backend().await;
        let (addr, h, _keep) = serve_one(app(TlsMode::Passthrough, None, None, backend), backend).await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
        drop(c);
        let out = h.await.unwrap();
        assert_eq!((out.bytes_in, out.bytes_out, out.error), (5, 5, None));
        assert!(out.tls.is_none());
    }

    #[tokio::test]
    async fn terminate_serves_the_store_certificate_and_forwards_plaintext() {
        let (cert, key) = self_signed("cache.example.test");
        let store = CertStore::with_test_cert("cache.example.test", &cert, &key);
        let server_tls = store.rustls_server_config_with_default(&[], "cache.example.test").unwrap();
        let backend = echo_backend().await;
        let (addr, h, _keep) = serve_one(app(TlsMode::Terminate, Some(server_tls), None, backend), backend).await;

        let mut c = tls_client(addr, "cache.example.test").await;
        c.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        let served = c.get_ref().1.peer_certificates().unwrap()[0].to_vec();
        drop(c);
        let out = h.await.unwrap();
        assert_eq!(out.error, None);
        let tls = out.tls.expect("terminated");
        assert_eq!(tls.sni.as_deref(), Some("cache.example.test"));
        assert_eq!(tls.version.as_deref(), Some("TLSv1.3"));
        assert!(tls.cipher.is_some());
        let expected = rustls_pemfile::certs(&mut cert.as_bytes()).next().unwrap().unwrap();
        assert_eq!(served, expected.to_vec());
    }

    #[tokio::test]
    async fn terminate_uses_tls_host_when_client_sends_no_matching_sni() {
        let (cert, key) = self_signed("cache.example.test");
        let store = CertStore::with_test_cert("cache.example.test", &cert, &key);
        let server_tls = store.rustls_server_config_with_default(&[], "cache.example.test").unwrap();
        let backend = echo_backend().await;
        let (addr, h, _keep) = serve_one(app(TlsMode::Terminate, Some(server_tls), None, backend), backend).await;
        let mut c = tls_client(addr, "unknown.example.test").await;
        c.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        c.read_exact(&mut buf).await.unwrap();
        drop(c);
        assert_eq!(h.await.unwrap().error, None);
    }

    #[tokio::test]
    async fn reencrypt_talks_tls_to_the_backend_and_verifies_when_asked() {
        let (front_cert, front_key) = self_signed("cache.example.test");
        let (back_cert, back_key) = self_signed("backend.example.test");
        let store = CertStore::with_test_cert("cache.example.test", &front_cert, &front_key);
        let server_tls = store.rustls_server_config_with_default(&[], "cache.example.test").unwrap();
        let backend = tls_echo_backend(&back_cert, &back_key).await;

        // No verification: any backend certificate is accepted.
        let (addr, h, _keep) = serve_one(app(TlsMode::Reencrypt, Some(server_tls.clone()), Some(crate::tls::insecure_client_config()), backend), backend).await;
        let mut c = tls_client(addr, "cache.example.test").await;
        c.write_all(b"secret").await.unwrap();
        let mut buf = [0u8; 6];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"secret");
        drop(c);
        assert_eq!(h.await.unwrap().error, None);

        // Verification with the backend's certificate as the CA succeeds…
        let verifying = crate::tls::verifying_client_config(Some(back_cert.as_bytes())).unwrap();
        let (addr, h, _keep) = serve_one(app(TlsMode::Reencrypt, Some(server_tls.clone()), Some(verifying), backend), backend).await;
        let mut c = tls_client(addr, "cache.example.test").await;
        c.write_all(b"ok").await.unwrap();
        let mut buf = [0u8; 2];
        c.read_exact(&mut buf).await.unwrap();
        drop(c);
        assert_eq!(h.await.unwrap().error, None);

        // …and without that CA the upstream handshake is refused.
        let strict = crate::tls::verifying_client_config(None).unwrap();
        let (addr, h, _keep) = serve_one(app(TlsMode::Reencrypt, Some(server_tls), Some(strict), backend), backend).await;
        let c = tls_client(addr, "cache.example.test").await;
        let out = h.await.unwrap();
        drop(c);
        assert_eq!(out.error, Some("upstream_tls"));
    }

    #[tokio::test]
    async fn plain_client_on_terminate_listener_fails_the_handshake() {
        let (cert, key) = self_signed("cache.example.test");
        let store = CertStore::with_test_cert("cache.example.test", &cert, &key);
        let server_tls = store.rustls_server_config_with_default(&[], "cache.example.test").unwrap();
        let backend = echo_backend().await;
        let (addr, h, _keep) = serve_one(app(TlsMode::Terminate, Some(server_tls), None, backend), backend).await;
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
        let out = h.await.unwrap();
        assert_eq!(out.error, Some("tls_handshake"));
    }
}
