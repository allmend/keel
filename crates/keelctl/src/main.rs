//! keelctl — remote control for Keel over mTLS TCP.
//!
//! Credentials come from a keelconfig file (created on a node with
//! `keel credentials create <name> --endpoint <host:port>`), resolved in
//! order: `--config` → `KEEL_CONFIG` env → `./keelconfig` → `~/.keel/config`.
//!
//! The TLS identity dialed is the fixed name "keel-control", verified
//! against the control CA in the keelconfig — the endpoint address is pure
//! transport, so IPs, DNS names, and port-forwards all work unchanged.

use std::io::Cursor;
use std::net::TcpStream;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use keel_control::keelconfig::Keelconfig;
use keel_control::{client, ControlRequest};

#[derive(Parser)]
#[command(name = "keelctl", about = "Remote control for Keel", version)]
struct Cli {
    /// Path to a keelconfig file (default: $KEEL_CONFIG, ./keelconfig, ~/.keel/config)
    #[arg(short, long)]
    config: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show node status
    Status,
    /// Backend pool management
    Backend {
        #[command(subcommand)]
        command: BackendCommand,
    },
    /// Config management
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Cluster management
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },
    /// Operator credentials
    Credentials {
        #[command(subcommand)]
        command: CredentialsCommand,
    },
}

#[derive(Subcommand)]
enum CredentialsCommand {
    /// Replace the control CA: every keelconfig issued so far stops working,
    /// this one included
    RevokeAll,
}

#[derive(Subcommand)]
enum BackendCommand {
    /// List backends in a pool
    List {
        #[arg(long)]
        pool: String,
    },
    /// Drain a backend (stop new connections, wait for active to finish)
    Drain {
        address: String,
        /// Block until drain is complete, streaming live status
        #[arg(long)]
        wait: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Apply the node's config directory (same as SIGHUP); in a cluster, push it as the new version
    Reload,
    /// Push a config directory (or a single file, as its keel.yaml) as the cluster's new config version
    Push { file: String },
}

#[derive(Subcommand)]
enum ClusterCommand {
    /// Show cluster status
    Status,
    /// Step down as leader and trigger a new election
    Demote,
    /// Gracefully leave the cluster
    Stepdown {
        /// Proceed even if the remaining nodes would lose quorum
        #[arg(long)]
        force: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let kc = Keelconfig::load(cli.config.as_deref())?;
    let mut stream = connect(&kc)?;

    match &cli.command {
        Command::Status => client::status(&mut stream),
        Command::Backend { command } => match command {
            BackendCommand::List { pool } => client::backend_list(&mut stream, pool),
            BackendCommand::Drain { address, wait } => {
                client::backend_drain(&mut stream, address, *wait)
            }
        },
        Command::Config { command } => match command {
            ConfigCommand::Reload => client::message(&mut stream, &ControlRequest::ConfigReload),
            ConfigCommand::Push { file } => {
                let files = keel_control::push_file_set(std::path::Path::new(file))?;
                client::message(&mut stream, &ControlRequest::ConfigPush { files })
            }
        },
        Command::Cluster { command } => match command {
            ClusterCommand::Status => client::cluster_status(&mut stream),
            ClusterCommand::Demote => client::message(&mut stream, &ControlRequest::ClusterDemote),
            ClusterCommand::Stepdown { force } => {
                client::message(&mut stream, &ControlRequest::ClusterStepdown { force: *force })
            }
        },
        Command::Credentials { command: CredentialsCommand::RevokeAll } => {
            client::message(&mut stream, &ControlRequest::CredentialsRevokeAll)
        }
    }
}

/// The fixed TLS identity of every Keel remote control listener.
const SERVER_NAME: &str = "keel-control";

fn connect(kc: &Keelconfig) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut Cursor::new(kc.ca_cert.as_bytes())) {
        let cert = cert.context("invalid ca_cert in keelconfig")?;
        roots.add(cert).map_err(|e| anyhow::anyhow!("invalid ca_cert in keelconfig: {e}"))?;
    }

    let client_certs: Vec<_> = rustls_pemfile::certs(&mut Cursor::new(kc.client_cert.as_bytes()))
        .collect::<Result<_, _>>()
        .context("invalid client_cert in keelconfig")?;
    let client_key = rustls_pemfile::private_key(&mut Cursor::new(kc.client_key.as_bytes()))
        .context("invalid client_key in keelconfig")?
        .context("no private key found in keelconfig client_key")?;

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(client_certs, client_key)
        .context("build TLS client config")?;

    let server_name = rustls::pki_types::ServerName::try_from(SERVER_NAME)
        .expect("static server name is valid");
    let conn = rustls::ClientConnection::new(Arc::new(tls_config), server_name)
        .context("TLS client init")?;
    let sock = TcpStream::connect(&kc.endpoint)
        .with_context(|| format!("cannot connect to {}\nIs keel's control.remote listener reachable?", kc.endpoint))?;

    Ok(rustls::StreamOwned::new(conn, sock))
}
