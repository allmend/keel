mod access_log;
mod acme;
mod backend;
mod cache;
mod cluster;
mod config;
mod dns;
mod control;
mod health;
mod l4;
mod metrics;
mod process;
mod shm;
mod proxy;
mod proxy_protocol;
#[cfg(test)]
mod testutil;
mod tls;
mod udp;
mod vhost;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;

const DEFAULT_SOCKET: &str = "/var/run/keel/keel.sock";

#[derive(Parser)]
#[command(name = "keel", version, about = "Fast, modern load balancer and reverse proxy")]
struct Cli {
    /// Path to the node file; it names the replicated config directory
    #[arg(short, long, default_value = config::DEFAULT_NODE_PATH)]
    config: String,

    /// Control socket path (for CLI commands)
    #[arg(long, global = true, default_value = DEFAULT_SOCKET)]
    socket: String,

    /// Forced recovery after a permanently lost majority: keep this node's
    /// stored state and make it the only member of the cluster
    #[arg(long)]
    force_new_cluster: bool,

    /// No colors in output (also NO_COLOR)
    #[arg(long, global = true, env = "NO_COLOR", value_parser = clap::builder::FalseyValueParser::new())]
    no_color: bool,

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
    /// Replace the control CA: every keelconfig issued so far stops working,
    /// on every node of a cluster
    RevokeAll,
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
    /// Remove another member, dead or alive; its node ID is refused from then on
    Remove {
        /// Node ID, as `cluster status` shows it
        node_id: u64,
    },
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
    /// Push the node's config directory as the next version (same as SIGHUP)
    Reload,
    /// Push a config directory (or a single file, as its keel.hcl) as the next version
    Push {
        file: String,
    },
    /// Check that the node file and its config directory load as a start or
    /// reload would. Silent: exit 0 when they can go live, 1 (error on stderr)
    /// when not
    Validate {
        /// Node file (default: --config); its keel.config_dir is loaded
        node: Option<String>,
        /// Print every file that is loaded
        #[arg(long)]
        full: bool,
        /// Show what changed since the version the running node serves, and
        /// whether it applies on reload or needs a restart
        #[arg(long)]
        diff: bool,
        /// List files not in canonical format
        #[arg(long)]
        list: bool,
        /// Rewrite files in canonical format, with units on bare durations
        #[arg(long)]
        write: bool,
        /// Exit 3 when a file is not in canonical format
        #[arg(long)]
        check: bool,
    },
}

fn main() -> Result<()> {
    // RUST_LOG governs when set. Appending a keel directive instead would
    // override it, which silently made every debug! in keel unreachable.
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("keel=info,pingora_core::listeners=warn")),
        )
        .with_ansi(!cli.no_color)
        .init();

    match &cli.command {
        Some(cmd) => run_cli(cmd, &cli),
        None => run_server(&cli),
    }
}

fn run_server(cli: &Cli) -> Result<()> {
    let cfg = config::load(&cli.config)
        .with_context(|| format!("failed to load config: {}", cli.config))?;
    info!(workers = cfg.keel.workers, user = cfg.keel.user, control_user = cfg.keel.control_user, config = cli.config, "starting keel");
    process::run(cfg, cli.force_new_cluster)
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
        Command::Config { command: ConfigCommand::Validate { node, full, diff, list, write, check } } => {
            let opts = config::validate::Options {
                node: node.clone().unwrap_or_else(|| cli.config.clone()),
                socket: cli.socket.clone(),
                full: *full,
                diff: *diff,
                list: *list,
                write: *write,
                check: *check,
                color: !cli.no_color,
            };
            std::process::exit(config::validate::run(&opts)?);
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
        },
        Command::Config { command } => match command {
            ConfigCommand::Reload => client::message(&mut stream, &ControlRequest::ConfigReload),
            ConfigCommand::Push { file } => {
                let files = keel_control::push_file_set(std::path::Path::new(file))?;
                client::message(&mut stream, &ControlRequest::ConfigPush { files })
            }
            ConfigCommand::Validate { .. } => unreachable!(),
        },
        Command::Cluster { command: ClusterCommand::Status } => client::cluster_status(&mut stream),
        Command::Cluster { command: ClusterCommand::Demote } => {
            client::message(&mut stream, &ControlRequest::ClusterDemote)
        }
        Command::Cluster { command: ClusterCommand::Stepdown { force } } => {
            client::message(&mut stream, &ControlRequest::ClusterStepdown { force: *force })
        }
        Command::Cluster { command: ClusterCommand::Remove { node_id } } => {
            client::message(&mut stream, &ControlRequest::ClusterRemove { node_id: *node_id })
        }
        Command::Credentials { command: CredentialsCommand::RevokeAll } => {
            client::message(&mut stream, &ControlRequest::CredentialsRevokeAll)
        }
        Command::Credentials { command: CredentialsCommand::Create { .. } } => unreachable!(),
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

    // The state directory from the node's config when it loads; the default
    // otherwise, so credentials can be created before a config exists.
    let ca_dir = config::load(&cli.config)
        .map(|c| c.keel.control_ca_dir())
        .unwrap_or_else(|_| config::KeelConfig::default().control_ca_dir());

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
