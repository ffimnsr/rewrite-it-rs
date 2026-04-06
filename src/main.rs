//! rewrite-it — LLM-powered text rewriting DBus service for Linux desktops.
//!
//! # Modes
//!
//! ```text
//! rewrite-it [daemon]          Start the DBus service (default)
//! rewrite-it setup             Download the model; does not start the daemon
//! rewrite-it rewrite [TEXT]    Rewrite TEXT (or stdin) via the running daemon
//! rewrite-it styles            Print the available style names
//! rewrite-it config            Print current config and its file location
//! ```

mod config;
mod dbus;
mod llm;
mod model;
mod prompt;

use std::{
    io::Read,
    sync::Arc,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name    = "rewrite-it",
    version,
    about   = "LLM-powered text rewriting as a self-contained DBus service",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the DBus service daemon (default when no sub-command is given).
    Daemon,

    /// Download the default model without starting the daemon.
    ///
    /// Useful for provisioning machines or CI environments.
    Setup,

    /// Rewrite TEXT using the running daemon.  Reads from stdin if TEXT is omitted.
    ///
    /// Requires the daemon to be running (or a DBus-activatable service file in place).
    Rewrite {
        /// Text to rewrite (reads from stdin when absent).
        text: Option<String>,
        /// Rewriting style.
        #[arg(short, long, default_value = "grammar")]
        style: String,
    },

    /// Print the available rewriting style names.
    Styles,

    /// Show the active configuration and its file path.
    Config,
}

// ── Entry-point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Daemon) {
        Command::Daemon         => run_daemon().await,
        Command::Setup          => run_setup().await,
        Command::Rewrite { text, style } => run_client_rewrite(text, style).await,
        Command::Styles         => {
            for s in prompt::Style::all_names() {
                println!("{s}");
            }
            Ok(())
        }
        Command::Config         => run_show_config(),
    }
}

// ── Sub-command handlers ──────────────────────────────────────────────────────

async fn run_daemon() -> Result<()> {
    info!("rewrite-it daemon starting");

    let config = Arc::new(config::Config::load()?);
    info!(path = ?config::Config::default_path(), "config loaded");

    // Ensure the model exists (downloads on first run).
    let model_path = model::ensure_model(&config).await?;

    // If the downloaded path differs from the configured one (unlikely after
    // first run), update config in memory so the engine uses the right path.
    let mut effective_config = (*config).clone();
    effective_config.model_path = model_path;
    let effective_config = Arc::new(effective_config);

    // Load model (CPU-bound: run in a blocking thread).
    info!("loading LLM — this may take a moment on first run…");
    let engine = tokio::task::spawn_blocking({
        let cfg = Arc::clone(&effective_config);
        move || llm::Engine::load(cfg)
    })
    .await
    .context("model-loading thread panicked")??;

    let engine = Arc::new(engine);
    info!("LLM ready — starting DBus service");

    dbus::serve(engine).await
}

async fn run_setup() -> Result<()> {
    info!("rewrite-it setup");
    let config = config::Config::load()?;
    info!(path = ?config::Config::default_path(), "config");
    let path = model::ensure_model(&config).await?;
    eprintln!("Model ready at {path:?}");
    eprintln!("Run `rewrite-it` to start the daemon.");
    Ok(())
}

async fn run_client_rewrite(text: Option<String>, style: String) -> Result<()> {
    let text = match text {
        Some(t) => t,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading text from stdin")?;
            buf.trim().to_string()
        }
    };

    if text.is_empty() {
        anyhow::bail!("no text provided (pass it as an argument or via stdin)");
    }

    let conn  = zbus::Connection::session().await?;
    let proxy = dbus::RewriterProxy::new(&conn).await?;
    let result = proxy.rewrite(&text, &style).await?;
    println!("{result}");
    Ok(())
}

fn run_show_config() -> Result<()> {
    let cfg  = config::Config::load()?;
    println!("Config file: {:?}", config::Config::default_path());
    println!();
    println!("{}", toml::to_string_pretty(&cfg)?);
    Ok(())
}
