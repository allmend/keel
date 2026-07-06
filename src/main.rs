mod access_log;
mod acme;
mod backend;
mod cache;
mod cluster;
mod config;
mod control;
mod health;
mod l4;
mod metrics;
mod process;
mod proxy;
mod tls;
mod vhost;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;

const DEFAULT_SOCKET: &str = "/var/run/keel/keel.sock";

#[derive(Parser)]
#[command(name = "keel", about = "Fast, modern load balancer and reverse proxy")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "keel.yaml")]
    config: String,

    /// Load additional config files matching this directory glob (conf.d style)
    #[arg(long)]
    conf_dir: Option<String>,

    /// Control socket path (for CLI commands)
    #[arg(long, default_value = DEFAULT_SOCKET)]
    socket: String,

    /// Enable cluster mode
    #[arg(long)]
    cluster: bool,

    /// Bootstrap a new cluster (first node)
    #[arg(long, requires = "cluster")]
    bootstrap: bool,

    /// Join an existing cluster at this address
    #[arg(long, requires = "cluster")]
    join: Option<String>,

    /// Shared secret for cluster join / bootstrap
    #[arg(long)]
    secret: Option<String>,

    /// Path to cluster CA certificate (BYO CA mode)
    #[arg(long)]
    ca_cert: Option<String>,

    /// Path to cluster CA key (BYO CA mode)
    #[arg(long)]
    ca_key: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show node status
    Status,

    /// Cluster management commands
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },

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

    /// Operator credentials for remote control (keelctl)
    Credentials {
        #[command(subcommand)]
        command: CredentialsCommand,
    },
}

#[derive(Subcommand)]
enum CredentialsCommand {
    /// Issue a client certificate signed by the control CA and print a
    /// keelconfig (endpoint + CA + client cert/key) to stdout
    Create {
        /// Operator name — becomes the certificate CN and the audit-log identity
        name: String,
        /// host:port of a node's control.remote listener, written into the keelconfig
        #[arg(long)]
        endpoint: String,
    },
}

#[derive(Subcommand)]
enum ClusterCommand {
    /// Show cluster status
    Status,
    /// Step down as leader and trigger a new election
    Demote,
    /// Gracefully leave the cluster: hand over leadership if leader, then
    /// commit this node's removal from the membership
    Stepdown {
        /// Proceed even if the remaining nodes would lose quorum
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum BackendCommand {
    /// List backends in a pool
    List {
        #[arg(long)]
        pool: String,
    },
    /// Add a backend to a pool
    Add {
        address: String,
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
    /// Reload config from disk (same as SIGHUP)
    Reload,
    /// Push a config file to the entire cluster via Raft
    Push {
        file: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("keel=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    match detect_mode(&cli) {
        Mode::Cli(cmd) => run_cli(cmd, &cli),
        Mode::ClusterBootstrap => run_server(cli),
        Mode::ClusterJoin => run_server(cli),
        Mode::Standalone => run_server(cli),
    }
}

enum Mode<'a> {
    Cli(&'a Command),
    ClusterBootstrap,
    ClusterJoin,
    Standalone,
}

fn detect_mode(cli: &Cli) -> Mode<'_> {
    if let Some(cmd) = &cli.command {
        return Mode::Cli(cmd);
    }
    if cli.cluster && cli.bootstrap {
        return Mode::ClusterBootstrap;
    }
    if cli.cluster && cli.join.is_some() {
        return Mode::ClusterJoin;
    }
    Mode::Standalone
}

fn run_server(cli: Cli) -> Result<()> {
    let cfg = config::load(&cli.config, cli.conf_dir.as_deref())
        .with_context(|| format!("failed to load config: {}", cli.config))?;

    if cli.cluster {
        run_cluster_server(cli, cfg)
    } else {
        info!(workers = cfg.keel.workers, user = cfg.keel.user, config = cli.config, "starting keel");
        process::run_master(cfg)
    }
}

fn run_cluster_server(cli: Cli, cfg: config::Config) -> Result<()> {
    let cluster_cfg = cfg.cluster.as_ref();
    let cluster_addr = cluster_cfg
        .map(|c| c.addr.clone())
        .unwrap_or_else(|| "0.0.0.0:7654".to_owned());

    let node_id = cluster_cfg
        .and_then(|c| c.node_id)
        .unwrap_or_else(|| derive_node_id(&cluster_addr));

    let secret = cli.secret.or_else(|| cluster_cfg.and_then(|c| c.secret.clone()));

    info!(node_id, cluster_addr, "starting keel in cluster mode");

    let opts = cluster::ClusterOpts {
        node_id,
        cluster_addr,
        secret,
        bootstrap: cli.bootstrap,
        join: cli.join,
    };

    let (handle, svc) = cluster::new_cluster(opts);
    proxy::run_cluster(&cfg, handle, svc)
}

fn derive_node_id(addr: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    addr.hash(&mut h);
    h.finish()
}

// Cli commands

fn run_cli(cmd: &Command, cli: &Cli) -> Result<()> {
    use keel_control::client;
    use keel_control::ControlRequest;

    // Commands that never touch the control socket.
    match cmd {
        Command::Credentials { command: CredentialsCommand::Create { name, endpoint } } => {
            return cli_credentials_create(cli, name, endpoint);
        }
        Command::Backend { command: BackendCommand::Add { address, pool } } => {
            return cli_backend_add(address, pool);
        }
        _ => {}
    }

    let mut stream = connect_socket(&cli.socket)?;
    match cmd {
        Command::Status => client::status(&mut stream),
        Command::Backend { command } => match command {
            BackendCommand::List { pool } => client::backend_list(&mut stream, pool),
            BackendCommand::Drain { address, wait } => {
                client::backend_drain(&mut stream, address, *wait)
            }
            BackendCommand::Add { .. } => unreachable!(),
        },
        Command::Config { command } => match command {
            ConfigCommand::Reload => client::message(&mut stream, &ControlRequest::ConfigReload),
            ConfigCommand::Push { file } => {
                let yaml = std::fs::read_to_string(file)
                    .with_context(|| format!("cannot read {file}"))?;
                client::message(&mut stream, &ControlRequest::ConfigPush { yaml })
            }
        },
        Command::Cluster { command: ClusterCommand::Status } => client::cluster_status(&mut stream),
        Command::Cluster { command: ClusterCommand::Demote } => {
            client::message(&mut stream, &ControlRequest::ClusterDemote)
        }
        Command::Cluster { command: ClusterCommand::Stepdown { force } } => {
            client::message(&mut stream, &ControlRequest::ClusterStepdown { force: *force })
        }
        Command::Credentials { .. } => unreachable!(),
    }
}

fn connect_socket(socket: &str) -> Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("cannot connect to {socket}\nIs keel running?"))
}

/// Issue an operator client cert from the control CA and print a keelconfig.
/// Runs on the node (needs the CA key on disk); the output goes to the
/// operator's workstation or CI secret store.
fn cli_credentials_create(cli: &Cli, name: &str, endpoint: &str) -> Result<()> {
    if name.is_empty()
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'@'))
    {
        anyhow::bail!("operator name must be non-empty and contain only [A-Za-z0-9.-_@]");
    }

    // ca_dir from config when available; the default otherwise, so credentials
    // can be created before keel.yaml gains a control block.
    let ca_dir = config::load(&cli.config, cli.conf_dir.as_deref())
        .ok()
        .and_then(|c| c.control.and_then(|c| c.remote).map(|r| r.ca_dir))
        .unwrap_or_else(config::default_control_ca_dir);

    let ca = control::ca::ControlCa::load_or_generate(&ca_dir)?;
    let (client_cert, client_key) = ca.issue_client(name)?;
    let kc = keel_control::keelconfig::Keelconfig {
        endpoint: endpoint.to_owned(),
        ca_cert: ca.ca_cert_pem.clone(),
        client_cert,
        client_key,
    };

    eprintln!("# keelconfig for '{name}' — contains a private key, store it like one.");
    eprintln!("# Save as ~/.keel/config (or point KEEL_CONFIG at it) and run: keelctl status");
    print!("{}", kc.to_yaml()?);
    Ok(())
}

fn cli_backend_add(address: &str, pool: &str) -> Result<()> {
    Err(anyhow::anyhow!(
        "Live backend addition is not supported in standalone mode.\n\
         Add '{address}' to pool '{pool}' in keel.yaml and run 'keel config reload'."
    ))
}
