use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use router_core::config::Config;
use router_server::AppState;

#[derive(Parser)]
#[command(name = "caret-router", version, about = "A fast LLM gateway")]
struct Cli {
    /// Path to the config file (TOML or JSON). Without it, providers are
    /// discovered from conventional environment variables.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Reload automatically when the config file changes.
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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.dev);

    match cli.command {
        Some(Command::Check { path }) => check(&path),
        None => run(cli),
    }
}

fn check(path: &std::path::Path) -> ExitCode {
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
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> ExitCode {
    let config = match load_initial_config(cli.config.as_deref()) {
        Ok(c) => c,
        Err(code) => return code,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start async runtime");

    runtime.block_on(async move {
        let addr = format!("{}:{}", config.server.host, config.server.port);
        let providers: Vec<String> = config.providers.keys().cloned().collect();
        let state = AppState::new(config);

        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(err) => {
                eprintln!("failed to bind {addr}: {err}");
                return ExitCode::FAILURE;
            }
        };
        tracing::info!(%addr, ?providers, "caret-router listening");

        spawn_reload_tasks(state.clone(), cli.config.clone(), cli.watch);

        let app = router_server::build_router(state.clone());
        match router_server::serve(listener, state, app, shutdown_signal()).await {
            Ok(()) => {
                tracing::info!("shutdown complete");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("server error: {err}");
                ExitCode::FAILURE
            }
        }
    })
}

fn load_initial_config(path: Option<&std::path::Path>) -> Result<Config, ExitCode> {
    match path {
        Some(p) => Config::load(p).map_err(|err| {
            eprintln!("{err}");
            ExitCode::FAILURE
        }),
        None => match Config::discover_from_env(&|var: &str| std::env::var(var).ok()) {
            Some(c) => {
                let names: Vec<&str> = c.providers.keys().map(String::as_str).collect();
                tracing::info!(?names, "providers configured from environment");
                Ok(c)
            }
            None => {
                eprintln!(
                    "no config file given and no provider environment variables found \
                     (e.g. OPENAI_API_KEY, ANTHROPIC_API_KEY); nothing to serve"
                );
                Err(ExitCode::FAILURE)
            }
        },
    }
}

/// SIGHUP reloads; `--watch` polls the file's mtime as an alternative for
/// environments where sending signals is awkward.
fn spawn_reload_tasks(state: std::sync::Arc<AppState>, config_path: Option<PathBuf>, watch: bool) {
    let Some(path) = config_path else { return };

    let reload = {
        let state = state.clone();
        let path = path.clone();
        move || match Config::load(&path) {
            Ok(new_config) => {
                state.apply_config(new_config);
                tracing::info!(path = %path.display(), "config reloaded");
            }
            Err(err) => {
                tracing::error!(path = %path.display(), %err, "config reload failed; keeping previous config");
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
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
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
