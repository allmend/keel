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
        Mode::Cli(cmd) => run_cli(cmd, &cli.socket),
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

fn run_cli(cmd: &Command, socket: &str) -> Result<()> {
    match cmd {
        Command::Status => cli_status(socket),
        Command::Backend { command } => match command {
            BackendCommand::List { pool } => cli_backend_list(socket, pool),
            BackendCommand::Drain { address, wait } => cli_backend_drain(socket, address, *wait),
            BackendCommand::Add { address, pool } => cli_backend_add(address, pool),
        },
        Command::Config { command } => match command {
            ConfigCommand::Reload => cli_config_reload(socket),
            ConfigCommand::Push { file } => cli_config_push(socket, file),
        },
        Command::Cluster { command: ClusterCommand::Status } => cli_cluster_status(socket),
        Command::Cluster { command: ClusterCommand::Demote } => cli_cluster_demote(socket),
        Command::Cluster { command: ClusterCommand::Stepdown { force } } => {
            cli_cluster_stepdown(socket, *force)
        }
    }
}

fn cli_status(socket: &str) -> Result<()> {
    use control::ControlRequest;

    let resp = send_request(socket, &ControlRequest::Status)?;
    let data = require_ok(&resp)?;

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
            println!("\n  {} ({} backend{})", name, backends.len(), if backends.len() == 1 { "" } else { "s" });
            print_backends(backends);
        }
    }
    Ok(())
}

fn cli_backend_list(socket: &str, pool: &str) -> Result<()> {
    use control::ControlRequest;

    let resp = send_request(socket, &ControlRequest::BackendList { pool: pool.to_owned() })?;
    let data = require_ok(&resp)?;

    let name = data["pool"].as_str().unwrap_or(pool);
    let backends = data["backends"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    println!("Pool: {} ({} backend{})", name, backends.len(), if backends.len() == 1 { "" } else { "s" });
    print_backends(backends);
    Ok(())
}

fn cli_backend_drain(socket: &str, address: &str, wait: bool) -> Result<()> {
    use control::ControlRequest;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let request = ControlRequest::BackendDrain { address: address.to_owned(), wait };
    let json = serde_json::to_string(&request)?;

    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("cannot connect to {socket}\nIs keel running?"))?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;

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

fn cli_cluster_status(socket: &str) -> Result<()> {
    use control::ControlRequest;

    let resp = send_request(socket, &ControlRequest::ClusterStatus)?;
    let data = require_ok(&resp)?;

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

fn cli_cluster_demote(socket: &str) -> Result<()> {
    use control::ControlRequest;

    let resp = send_request(socket, &ControlRequest::ClusterDemote)?;
    let data = require_ok(&resp)?;
    println!("{}", data["message"].as_str().unwrap_or("done"));
    Ok(())
}

fn cli_cluster_stepdown(socket: &str, force: bool) -> Result<()> {
    use control::ControlRequest;

    let resp = send_request(socket, &ControlRequest::ClusterStepdown { force })?;
    let data = require_ok(&resp)?;
    println!("{}", data["message"].as_str().unwrap_or("done"));
    Ok(())
}

fn cli_config_push(socket: &str, file: &str) -> Result<()> {
    use control::ControlRequest;

    let yaml = std::fs::read_to_string(file)
        .with_context(|| format!("cannot read {file}"))?;

    let resp = send_request(socket, &ControlRequest::ConfigPush { yaml })?;
    let data = require_ok(&resp)?;
    println!("{}", data["message"].as_str().unwrap_or("done"));
    Ok(())
}

fn cli_backend_add(address: &str, pool: &str) -> Result<()> {
    Err(anyhow::anyhow!(
        "Live backend addition is not supported in standalone mode.\n\
         Add '{address}' to pool '{pool}' in keel.yaml and run 'keel config reload'."
    ))
}

fn cli_config_reload(socket: &str) -> Result<()> {
    use control::ControlRequest;

    let resp = send_request(socket, &ControlRequest::ConfigReload)?;
    let data = require_ok(&resp)?;
    println!("{}", data["message"].as_str().unwrap_or("done"));
    Ok(())
}

// Helpers

fn send_request(socket: &str, request: &control::ControlRequest) -> Result<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let json = serde_json::to_string(request)?;

    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("cannot connect to {socket}\nIs keel running?"))?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    Ok(serde_json::from_str(line.trim())?)
}

fn require_ok(val: &serde_json::Value) -> Result<&serde_json::Value> {
    if val["ok"].as_bool() != Some(true) {
        let err = val["error"].as_str().unwrap_or("unknown error");
        anyhow::bail!("{err}");
    }
    Ok(&val["data"])
}

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
