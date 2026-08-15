use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use router_cluster::{Command as StoreCommand, Store};
use router_core::config::{Config, Format};
use router_server::AppState;

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum Mode {
    Managed,
    File,
}

#[derive(Parser)]
#[command(name = "caret-router", version, about = "A fast LLM gateway")]
struct Cli {
    /// TOML or JSON config. In managed mode it seeds an empty store.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Directory for the embedded store and usage history.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Bind the cluster port (Raft, join, peer APIs), e.g. 0.0.0.0:9444.
    #[arg(long, global = true)]
    cluster_listen: Option<String>,

    /// Join through any live subset of an existing cluster.
    #[arg(long = "join", global = true)]
    cluster_join: Vec<String>,

    /// The cluster's join token, or an `env.*` reference to it.
    #[arg(long, global = true)]
    cluster_token: Option<String>,

    /// Managed mode permits console writes; file mode is read-only.
    #[arg(long, global = true, value_enum, default_value = "managed")]
    mode: Mode,

    /// Reload automatically when the file-mode config changes.
    #[arg(long)]
    watch: bool,

    /// Human-friendly log output instead of JSON lines.
    #[arg(long)]
    dev: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Validate a config file and exit.
    Check { path: PathBuf },
    /// Import or export the managed configuration document.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage encrypted store.* values while the gateway is stopped.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Create and manage virtual keys.
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Inspect and manage cluster membership.
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },
}

#[derive(Subcommand)]
enum ClusterCommand {
    /// Print this cluster's join token. Treat it as a secret.
    Token {
        /// Mint a new token. Existing members keep working only once they
        /// are restarted with it, so roll one node at a time.
        #[arg(long)]
        rotate: bool,
    },
    /// Members, roles, applied version, and lag.
    Status,
    /// Remove a node that is never coming back.
    Remove { id: String },
}

#[derive(Subcommand)]
enum KeyCommand {
    /// Issue a key. The full `ck-…` value is printed exactly once.
    Create {
        #[arg(long)]
        name: String,
        /// Comma-separated models and/or aliases; omit for all models.
        #[arg(long, value_delimiter = ',')]
        models: Vec<String>,
        /// Spend cap as `AMOUNT/PERIOD`, e.g. `250/monthly`.
        #[arg(long, value_name = "USD/PERIOD")]
        budget_usd: Option<String>,
        #[arg(long)]
        rpm: Option<u64>,
        #[arg(long)]
        tpm: Option<u64>,
        /// RFC 3339 UTC, e.g. `2027-01-01T00:00:00Z`.
        #[arg(long)]
        expires: Option<String>,
    },
    /// List keys (ids and attributes; never secrets).
    Ls,
    /// Issue a new secret for an existing key, honoring an overlap window.
    Rotate {
        id: String,
        /// Hours the previous secret keeps working.
        #[arg(long, default_value_t = 24)]
        grace_hours: u64,
    },
    /// Disable a key without deleting its attributes or history.
    Disable { id: String },
    /// Re-enable a disabled key.
    Enable { id: String },
    /// Delete a key outright.
    Rm { id: String },
    /// Hash a secret for a `secret_hash` entry in a file-mode config.
    Hash {
        /// The secret to hash; omit to read it from stdin.
        secret: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print the managed TOML document.
    Export,
    /// Validate and replace the managed document from a file.
    Import { path: PathBuf },
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Read a secret from stdin, or from the named environment variable.
    Set {
        name: String,
        #[arg(long)]
        from_env: Option<String>,
    },
    /// Delete one stored secret.
    Delete { name: String },
    /// List secret names. Values are never printed.
    List,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.dev);
    match &cli.command {
        Some(Command::Check { path }) => check(path),
        Some(Command::Config { command }) => config_command(&cli, command),
        Some(Command::Secret { command }) => secret_command(&cli, command),
        Some(Command::Key { command }) => key_command(&cli, command),
        Some(Command::Cluster { command }) => cluster_command(&cli, command),
        None => run(cli),
    }
}

fn data_dir(cli: &Cli) -> PathBuf {
    cli.data_dir
        .clone()
        .or_else(|| std::env::var_os("CARET_DATA_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".caret-router")
        })
}

fn check(path: &Path) -> ExitCode {
    match Config::load(path) {
        Ok(config) => {
            println!(
                "OK: {} provider(s), {} alias(es), {} fallback chain(s)",
                config.providers.len(),
                config.aliases.len(),
                config.fallbacks.len(),
            );
            ExitCode::SUCCESS
        }
        Err(err) => fail(err),
    }
}

fn open_store(cli: &Cli) -> Result<Store, ExitCode> {
    Store::open(&data_dir(cli)).map_err(fail)
}

/// For commands that only read. Falls back to a read-only handle when the
/// gateway holds the lock, so an operator never has to stop the service to
/// look at its own configuration.
fn open_store_for_reading(cli: &Cli) -> Result<Store, ExitCode> {
    let dir = data_dir(cli);
    match Store::open(&dir) {
        Ok(store) => Ok(store),
        Err(router_cluster::StoreError::Locked) => Store::open_read_only(&dir).map_err(fail),
        Err(err) => Err(fail(err)),
    }
}

fn config_command(cli: &Cli, command: &ConfigCommand) -> ExitCode {
    let store = match if matches!(command, ConfigCommand::Export) {
        open_store_for_reading(cli)
    } else {
        open_store(cli)
    } {
        Ok(store) => store,
        Err(code) => return code,
    };
    match command {
        ConfigCommand::Export => {
            let (state, _) = store.read();
            match state.config_text {
                Some(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                None => fail("managed store has no configuration"),
            }
        }
        ConfigCommand::Import { path } => {
            let text = match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(err) => return fail(err),
            };
            if let Err(err) = validate_store_config(&store, &text, format_for(path)) {
                return fail(err);
            }
            match store.commit(None, StoreCommand::PutConfig { text }) {
                Ok(version) => {
                    println!("Imported managed configuration at version {version}");
                    ExitCode::SUCCESS
                }
                Err(err) => fail(err),
            }
        }
    }
}

fn secret_command(cli: &Cli, command: &SecretCommand) -> ExitCode {
    let store = match open_store(cli) {
        Ok(store) => store,
        Err(code) => return code,
    };
    match command {
        SecretCommand::Set { name, from_env } => {
            if !valid_secret_name(name) {
                return fail(
                    "secret name must contain only letters, digits, dot, dash, or underscore",
                );
            }
            let value = match from_env {
                Some(var) => match std::env::var(var) {
                    Ok(value) if !value.is_empty() => value,
                    Ok(_) => return fail(format!("environment variable {var} is empty")),
                    Err(_) => return fail(format!("environment variable {var} is not set")),
                },
                None => {
                    let mut value = String::new();
                    if let Err(err) = std::io::stdin().read_to_string(&mut value) {
                        return fail(err);
                    }
                    value.trim_end_matches(['\r', '\n']).to_owned()
                }
            };
            if value.is_empty() {
                return fail("secret value must not be empty");
            }
            let sealed = store.seal_secret(&value);
            match store.commit(
                None,
                StoreCommand::PutSecret {
                    name: name.clone(),
                    sealed,
                },
            ) {
                Ok(version) => {
                    println!("Stored {name} at version {version}");
                    ExitCode::SUCCESS
                }
                Err(err) => fail(err),
            }
        }
        SecretCommand::Delete { name } => {
            match store.commit(None, StoreCommand::DeleteSecret { name: name.clone() }) {
                Ok(version) => {
                    println!("Deleted {name} at version {version}");
                    ExitCode::SUCCESS
                }
                Err(err) => fail(err),
            }
        }
        SecretCommand::List => {
            let (state, _) = store.read();
            for name in state.secrets.keys() {
                println!("{name}");
            }
            ExitCode::SUCCESS
        }
    }
}

fn key_command(cli: &Cli, command: &KeyCommand) -> ExitCode {
    use router_core::vkey::{self, PrevSecret, RateLimit, VirtualKeyDef};

    // Hashing needs no store — it is the file-mode authoring path.
    if let KeyCommand::Hash { secret } = command {
        let value = match secret {
            Some(value) => value.clone(),
            None => {
                let mut buf = String::new();
                if let Err(err) = std::io::stdin().read_to_string(&mut buf) {
                    return fail(err);
                }
                buf.trim_end_matches(['\r', '\n']).to_owned()
            }
        };
        if value.is_empty() {
            return fail("secret must not be empty");
        }
        println!("{}", vkey::hash_secret(&value));
        return ExitCode::SUCCESS;
    }

    let store = match if matches!(command, KeyCommand::Ls) {
        open_store_for_reading(cli)
    } else {
        open_store(cli)
    } {
        Ok(store) => store,
        Err(code) => return code,
    };

    // Mutate one key in place, then commit the whole definition back.
    let edit = |id: &str, f: &dyn Fn(&mut VirtualKeyDef)| -> Result<(), String> {
        let (state, _) = store.read();
        let mut def = state
            .virtual_keys
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no key with id `{id}`"))?;
        f(&mut def);
        store
            .commit(None, StoreCommand::PutVirtualKey { def })
            .map(|_| ())
            .map_err(|err| err.to_string())
    };

    match command {
        KeyCommand::Hash { .. } => unreachable!("handled above"),
        KeyCommand::Create {
            name,
            models,
            budget_usd,
            rpm,
            tpm,
            expires,
        } => {
            let budget = match budget_usd.as_deref().map(parse_budget).transpose() {
                Ok(budget) => budget,
                Err(err) => return fail(err),
            };
            let expires_ms = match expires.as_deref().map(|s| {
                vkey::parse_rfc3339_utc_ms(s)
                    .ok_or_else(|| format!("`{s}` is not RFC 3339 UTC (e.g. 2027-01-01T00:00:00Z)"))
            }) {
                Some(Ok(ms)) => Some(ms),
                Some(Err(err)) => return fail(err),
                None => None,
            };
            let rate = (rpm.is_some() || tpm.is_some()).then_some(RateLimit {
                rpm: *rpm,
                tpm: *tpm,
            });
            let generated = vkey::generate();
            let def = VirtualKeyDef {
                id: generated.id.clone(),
                name: name.clone(),
                secret_hash: vkey::hash_secret(&generated.secret),
                prev_secret: None,
                models: models.clone(),
                budget,
                rate,
                expires_ms,
                tags: Default::default(),
                enabled: true,
                created_ms: vkey::unix_now_ms(),
            };
            match store.commit(None, StoreCommand::PutVirtualKey { def }) {
                Ok(_) => {
                    println!("{}", generated.full());
                    eprintln!(
                        "Key `{name}` created with id {}. This secret is shown once — store it now.",
                        generated.id
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => fail(err),
            }
        }
        KeyCommand::Ls => {
            let (state, _) = store.read();
            if state.virtual_keys.is_empty() {
                println!("No virtual keys. Create one with `caret-router key create --name NAME`.");
                return ExitCode::SUCCESS;
            }
            println!(
                "{:<8} {:<24} {:<8} {:<14} {:<12} SCOPE",
                "ID", "NAME", "STATE", "BUDGET", "LIMITS"
            );
            for def in state.virtual_keys.values() {
                let budget = def
                    .budget
                    .map(|b| format!("${:.0}/{}", b.usd, period_name(b.period)))
                    .unwrap_or_else(|| "-".into());
                let limits = def
                    .rate
                    .map(|r| match (r.rpm, r.tpm) {
                        (Some(rpm), Some(tpm)) => format!("{rpm}rpm {tpm}tpm"),
                        (Some(rpm), None) => format!("{rpm}rpm"),
                        (None, Some(tpm)) => format!("{tpm}tpm"),
                        (None, None) => "-".into(),
                    })
                    .unwrap_or_else(|| "-".into());
                let scope = if def.models.is_empty() {
                    "all models".to_owned()
                } else {
                    def.models.join(",")
                };
                let state_label = if !def.enabled {
                    "disabled"
                } else if def.expires_ms.is_some_and(|e| e <= vkey::unix_now_ms()) {
                    "expired"
                } else {
                    "active"
                };
                println!(
                    "{:<8} {:<24} {:<8} {:<14} {:<12} {}",
                    def.id, def.name, state_label, budget, limits, scope
                );
            }
            ExitCode::SUCCESS
        }
        KeyCommand::Rotate { id, grace_hours } => {
            let secret = vkey::generate_secret();
            let valid_until_ms = vkey::unix_now_ms() + grace_hours * 3_600_000;
            let new_hash = vkey::hash_secret(&secret);
            let result = edit(id, &|def: &mut VirtualKeyDef| {
                def.prev_secret = Some(PrevSecret {
                    secret_hash: def.secret_hash.clone(),
                    valid_until_ms,
                });
                def.secret_hash = new_hash.clone();
            });
            match result {
                Ok(()) => {
                    println!("ck-{id}-{secret}");
                    eprintln!(
                        "Rotated. The previous secret keeps working for {grace_hours}h so \
                         deployments can roll without a hard cut."
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => fail(err),
            }
        }
        KeyCommand::Disable { id } => {
            match edit(id, &|def: &mut VirtualKeyDef| def.enabled = false) {
                Ok(()) => {
                    println!("Disabled {id}");
                    ExitCode::SUCCESS
                }
                Err(err) => fail(err),
            }
        }
        KeyCommand::Enable { id } => {
            match edit(id, &|def: &mut VirtualKeyDef| def.enabled = true) {
                Ok(()) => {
                    println!("Enabled {id}");
                    ExitCode::SUCCESS
                }
                Err(err) => fail(err),
            }
        }
        KeyCommand::Rm { id } => {
            let (state, _) = store.read();
            if !state.virtual_keys.contains_key(id) {
                return fail(format!("no key with id `{id}`"));
            }
            match store.commit(None, StoreCommand::DeleteVirtualKey { id: id.clone() }) {
                Ok(_) => {
                    println!("Removed {id}");
                    ExitCode::SUCCESS
                }
                Err(err) => fail(err),
            }
        }
    }
}

fn period_name(period: router_core::vkey::BudgetPeriod) -> &'static str {
    use router_core::vkey::BudgetPeriod::*;
    match period {
        Daily => "daily",
        Weekly => "weekly",
        Monthly => "monthly",
    }
}

/// `250/monthly` -> a budget. The period defaults to monthly.
fn parse_budget(spec: &str) -> Result<router_core::vkey::Budget, String> {
    use router_core::vkey::{Budget, BudgetPeriod};
    let (amount, period) = match spec.split_once('/') {
        Some((amount, period)) => (amount, period),
        None => (spec, "monthly"),
    };
    let usd: f64 = amount
        .trim()
        .parse()
        .map_err(|_| format!("`{amount}` is not a number (expected e.g. `250/monthly`)"))?;
    if !(usd.is_finite() && usd > 0.0) {
        return Err("budget must be a positive number".into());
    }
    let period = match period.trim() {
        "daily" | "day" => BudgetPeriod::Daily,
        "weekly" | "week" => BudgetPeriod::Weekly,
        "monthly" | "month" => BudgetPeriod::Monthly,
        other => {
            return Err(format!(
                "unknown period `{other}` (expected daily, weekly, or monthly)"
            ));
        }
    };
    Ok(Budget { usd, period })
}

fn cluster_command(cli: &Cli, command: &ClusterCommand) -> ExitCode {
    let needs_write = matches!(command, ClusterCommand::Token { rotate: true });
    let store = match if needs_write {
        open_store(cli)
    } else {
        open_store_for_reading(cli)
    } {
        Ok(store) => store,
        Err(code) => return code,
    };
    match command {
        ClusterCommand::Token { rotate } => {
            if *rotate {
                let fresh = router_cluster::token::JoinToken::generate();
                if let Err(err) = store.adopt_join_token(&fresh) {
                    return fail(err);
                }
                println!("{}", fresh.encode());
                eprintln!(
                    "Rotated. Restart each member with the new token, one at a time — \
                     a node still holding the old one cannot be reached by the others."
                );
                return ExitCode::SUCCESS;
            }
            match store.join_token() {
                Ok(token) => {
                    println!("{}", token.encode());
                    eprintln!(
                        "This token is a credential: anyone holding it can join the cluster."
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => fail(err),
            }
        }
        // Status and remove need a running cluster, which holds the data
        // dir's lock — so they go over the admin API of the local node
        // rather than opening the store a second time.
        ClusterCommand::Status => {
            eprintln!(
                "Cluster status is served by the running node: \
                 curl -s localhost:8080/admin/api/fleet (admin key required), \
                 or open the console's Cluster page."
            );
            let (state, version) = store.read();
            println!("Local applied version: {version}");
            println!(
                "Cluster id: {}",
                state
                    .settings
                    .get("cluster_id")
                    .map(String::as_str)
                    .unwrap_or("(single node — no cluster configured)")
            );
            ExitCode::SUCCESS
        }
        ClusterCommand::Remove { id } => {
            eprintln!(
                "Removing a member changes replicated membership, so it must go through \
                 the running leader: POST /admin/api/cluster/remove {{\"id\": {id}}}."
            );
            ExitCode::FAILURE
        }
    }
}

/// Bring up the cluster port and join or bootstrap, if configured.
/// Returns whether this node is clustered. CLI flags win over the config
/// file, so an operator can cluster a file-managed deployment without
/// editing the file their deploy tool owns.
async fn start_cluster(
    cli: &Cli,
    state: &Arc<AppState>,
    store: &Arc<Store>,
    data_dir: &Path,
) -> Result<bool, String> {
    use router_cluster::raft::ClusterNode;
    use router_cluster::raft::server::{self as cluster_server, JoinRequest};
    use router_cluster::token::JoinToken;

    let config = state.config.load();
    let listen = cli
        .cluster_listen
        .clone()
        .or_else(|| config.cluster.listen.clone());
    let Some(listen) = listen else {
        return Ok(false); // single node: nothing to bind, nothing to change
    };
    let seeds: Vec<String> = if cli.cluster_join.is_empty() {
        config.cluster.join.clone()
    } else {
        cli.cluster_join.clone()
    };

    // A supplied token means "join their cluster"; none means "be one".
    let supplied = match cli.cluster_token.as_deref() {
        Some(value) => Some(resolve_token_arg(value)?),
        None => config.cluster.token.as_ref().map(|s| s.expose().to_owned()),
    };
    let token = match supplied {
        Some(text) => {
            let token = JoinToken::parse(&text)
                .ok_or_else(|| format!("`{text}` is not a caret join token"))?;
            store.adopt_join_token(&token).map_err(|e| e.to_string())?;
            token
        }
        None => store.join_token().map_err(|e| e.to_string())?,
    };

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| format!("failed to bind cluster port {listen}: {e}"))?;
    let bound = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| listen.clone());
    // Peers dial the advertised address, which must be routable — a
    // wildcard bind is not.
    let advertised = advertised_addr(&listen, &bound);

    let node = ClusterNode::start(
        store.raft_node_id(),
        advertised.clone(),
        data_dir,
        token.clone(),
    )
    .await
    .map_err(|e| format!("cluster start failed: {e}"))?;

    let serving = node.clone();
    tokio::spawn(async move {
        let _ = cluster_server::serve(listener, serving, shutdown_signal()).await;
    });

    if seeds.is_empty() {
        node.bootstrap()
            .await
            .map_err(|e| format!("cluster bootstrap failed: {e}"))?;
        tracing::info!(cluster = token.cluster_id(), addr = %advertised, "bootstrapped a cluster of one");
    } else {
        let me = JoinRequest {
            node_id: node.id,
            addr: advertised.clone(),
        };
        let response = cluster_server::request_join(&token, &seeds, me).await?;
        tracing::info!(
            cluster = response.cluster,
            members = response.members,
            "joined cluster"
        );
    }

    state.attach_cluster(node.clone());
    node.spawn_liveness_probe();
    state.refresh_vkeys();
    Ok(true)
}

/// `0.0.0.0:9444` tells a peer nothing about where to dial. Fall back to
/// the host's own address rather than advertising a wildcard.
fn advertised_addr(configured: &str, bound: &str) -> String {
    let wildcard = configured.starts_with("0.0.0.0:")
        || configured.starts_with("[::]:")
        || configured.starts_with(":::");
    if !wildcard {
        return if configured.ends_with(":0") {
            bound.to_owned()
        } else {
            configured.to_owned()
        };
    }
    let port = bound.rsplit(':').next().unwrap_or("9444");
    let host = std::env::var("CARET_ADVERTISE_HOST")
        .ok()
        .or_else(local_ipv4)
        .unwrap_or_else(|| "127.0.0.1".into());
    format!("{host}:{port}")
}

/// The address a peer would reach us on, discovered by asking the OS which
/// local interface it would use to leave the box. No packets are sent.
fn local_ipv4() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?; // TEST-NET-1: routable, never real
    socket.local_addr().ok().map(|a| a.ip().to_string())
}

fn resolve_token_arg(value: &str) -> Result<String, String> {
    match value.strip_prefix("env.") {
        Some(var) => {
            std::env::var(var).map_err(|_| format!("environment variable {var} is not set"))
        }
        None => Ok(value.to_owned()),
    }
}

fn run(cli: Cli) -> ExitCode {
    let data_dir = data_dir(&cli);
    let store = match Store::open(&data_dir) {
        Ok(store) => Arc::new(store),
        Err(err) => return fail(err),
    };
    let config = match load_initial_config(&cli, &store) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start async runtime");
    runtime.block_on(async move {
        let addr = format!("{}:{}", config.server.host, config.server.port);
        let providers: Vec<String> = config.providers.keys().cloned().collect();
        let state = if cli.mode == Mode::Managed {
            AppState::managed(config, store.clone(), data_dir.clone())
        } else {
            AppState::file_with_data_dir(config, store.clone(), data_dir.clone())
        };
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => listener,
            Err(err) => return fail(format!("failed to bind {addr}: {err}")),
        };
        tracing::info!(%addr, ?providers, mode = ?cli.mode, "caret-router listening");
        if cli.mode == Mode::File {
            spawn_reload_tasks(state.clone(), cli.config.clone(), cli.watch, store.clone());
        }

        let clustered = match start_cluster(&cli, &state, &store, &data_dir).await {
            Ok(started) => started,
            Err(message) => return fail(message),
        };
        state.spawn_share_tracker();

        let app = router_server::build_router(state.clone());
        let shutdown_state = state.clone();
        let result = router_server::serve(listener, state, app, shutdown_signal()).await;

        // A clustered node's state machine owns the snapshot file, so the
        // single-node store must not write a stale one over it on the way
        // out.
        if clustered {
            if let Some(node) = shutdown_state.cluster.get() {
                node.shutdown().await;
            }
        } else if let Err(err) = store.compact() {
            tracing::warn!(%err, "store compaction failed during shutdown");
        }
        match result {
            Ok(()) => {
                tracing::info!("shutdown complete");
                ExitCode::SUCCESS
            }
            Err(err) => fail(format!("server error: {err}")),
        }
    })
}

fn load_initial_config(cli: &Cli, store: &Store) -> Result<Config, ExitCode> {
    if cli.mode == Mode::Managed {
        let (snapshot, _) = store.read();
        if let Some(text) = snapshot.config_text {
            return validate_store_config(store, &text, Format::Toml).map_err(fail);
        }
        if let Some(path) = cli.config.as_deref() {
            let text = std::fs::read_to_string(path).map_err(fail)?;
            let config = validate_store_config(store, &text, format_for(path)).map_err(fail)?;
            store
                .commit(None, StoreCommand::PutConfig { text })
                .map_err(fail)?;
            return Ok(config);
        }
        if let Some(config) = Config::discover_from_env(&|name: &str| std::env::var(name).ok()) {
            return Ok(config);
        }
        return bootstrap(store).map_err(fail);
    }
    match cli.config.as_deref() {
        Some(path) => load_file_config(store, path).map_err(fail),
        None => Config::discover_from_env(&|name: &str| std::env::var(name).ok()).ok_or_else(|| {
            fail("file mode needs --config or at least one conventional provider environment variable")
        }),
    }
}

fn bootstrap(store: &Store) -> Result<Config, Box<dyn std::error::Error>> {
    let secret = router_core::vkey::generate_secret();
    store.commit(
        None,
        StoreCommand::PutSecret {
            name: "bootstrap_admin".into(),
            sealed: store.seal_secret(&secret),
        },
    )?;
    let text = "[console]\nadmin_keys = [\"store.bootstrap_admin\"]\n".to_owned();
    let config = validate_store_config(store, &text, Format::Toml)?;
    store.commit(None, StoreCommand::PutConfig { text })?;
    eprintln!("Bootstrap admin key (shown once): {secret}");
    eprintln!("Open http://127.0.0.1:8080/console");
    Ok(config)
}

fn load_file_config(store: &Store, path: &Path) -> Result<Config, router_core::config::LoadError> {
    let text = std::fs::read_to_string(path)?;
    validate_store_config(store, &text, format_for(path))
}

fn validate_store_config(
    store: &Store,
    text: &str,
    format: Format,
) -> Result<Config, router_core::config::LoadError> {
    let env = |name: &str| {
        name.strip_prefix("store.")
            .and_then(|secret| store.resolve_secret(secret))
            .or_else(|| std::env::var(name).ok())
    };
    Config::from_str_with_env(text, format, &env)
}

fn format_for(path: &Path) -> Format {
    if path.extension().and_then(|value| value.to_str()) == Some("json") {
        Format::Json
    } else {
        Format::Toml
    }
}

fn valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn spawn_reload_tasks(
    state: Arc<AppState>,
    config_path: Option<PathBuf>,
    watch: bool,
    store: Arc<Store>,
) {
    let Some(path) = config_path else { return };
    let reload = {
        let state = state.clone();
        let path = path.clone();
        move || match load_file_config(&store, &path) {
            Ok(new_config) => {
                state.apply_config(new_config);
                tracing::info!(path = %path.display(), "config reloaded");
            }
            Err(err) => {
                tracing::error!(path = %path.display(), %err, "reload failed; keeping previous config")
            }
        }
    };
    #[cfg(unix)]
    {
        let reload = reload.clone();
        tokio::spawn(async move {
            let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("failed to install SIGHUP handler");
            while hup.recv().await.is_some() {
                reload();
            }
        });
    }
    if watch {
        tokio::spawn(async move {
            let mut last = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let current = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                if current.is_some() && current != last {
                    last = current;
                    reload();
                }
            }
        });
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn init_tracing(dev: bool) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if dev {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    }
}

fn fail(error: impl std::fmt::Display) -> ExitCode {
    eprintln!("{error}");
    ExitCode::FAILURE
}
