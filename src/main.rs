//! rewrite-it — LLM-powered text rewriting DBus service for Linux desktops.
//!
//! # Modes
//!
//! ```text
//! rewrite-it [daemon]          Start the DBus service (default)
//! rewrite-it setup             Download the model; does not start the daemon
//! rewrite-it rewrite [TEXT]    Rewrite TEXT (or stdin) via the running daemon
//! rewrite-it styles            Print the available style names
//! rewrite-it status            Print daemon model readiness/status
//! rewrite-it config            Print current config and its file location
//! ```

use std::{io::Read, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rewrite_it::{config, dbus, llm, model, prompt};
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

    /// Show daemon model readiness and lifecycle status.
    Status,

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
        Command::Daemon => run_daemon().await,
        Command::Setup => run_setup().await,
        Command::Rewrite { text, style } => run_client_rewrite(text, style).await,
        Command::Styles => {
            for s in prompt::Style::all_names() {
                println!("{s}");
            }
            Ok(())
        }
        Command::Status => run_status().await,
        Command::Config => run_show_config(),
    }
}

// ── Sub-command handlers ──────────────────────────────────────────────────────

async fn run_daemon() -> Result<()> {
    info!("rewrite-it daemon starting");

    let config = Arc::new(config::Config::load()?);
    info!(path = ?config::Config::default_path(), "config loaded");

    let engine = Arc::new(llm::EngineManager::new(Arc::clone(&config)));

    // ── Background model preload ───────────────────────────────────────────
    // Start downloading/loading the model immediately so the first client
    // request is served without delay, rather than waiting for the first call.
    {
        let eng = Arc::clone(&engine);
        tokio::spawn(async move {
            if let Err(e) = eng.ensure_ready().await {
                tracing::error!("background model preload failed: {e}");
            } else {
                info!("model preloaded and ready");
            }
        });
    }

    // ── Systemd watchdog ──────────────────────────────────────────────────
    // Ping the watchdog at half the configured interval while the tokio
    // runtime is alive.  Stops pinging when an inference hang is detected
    // (inference_hung returns true), causing systemd to restart the service.
    {
        let mut wd_usec: u64 = 0;
        if sd_notify::watchdog_enabled(false, &mut wd_usec) {
            let ping_interval = Duration::from_micros(wd_usec / 2);
            let eng = Arc::clone(&engine);
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(ping_interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    if eng.inference_hung() {
                        tracing::warn!(
                            timeout_secs = eng.config().inference_timeout_secs,
                            "inference appears hung — suppressing watchdog ping to trigger restart"
                        );
                    } else {
                        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
                    }
                }
            });
        }
    }

    // ── Idle-exit ─────────────────────────────────────────────────────────
    // If configured, exit the daemon after a period with no requests so it
    // can be restarted on-demand via DBus activation.
    if let Some(idle_secs) = config.idle_timeout_secs {
        let check_interval = Duration::from_secs(60.min(idle_secs / 4).max(10));
        let eng = Arc::clone(&engine);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(check_interval).await;
                let idle = eng.seconds_idle();
                if idle > 0 && idle >= idle_secs {
                    info!(idle_secs, "idle timeout reached, exiting daemon");
                    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);
                    std::process::exit(0);
                }
            }
        });
    }

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

    let conn = zbus::Connection::session().await?;
    let proxy = dbus::RewriterProxy::new(&conn).await?;
    let result = proxy.rewrite(&text, &style).await?;
    println!("{result}");
    Ok(())
}

async fn run_status() -> Result<()> {
    let conn = zbus::Connection::session().await?;
    let proxy = dbus::RewriterProxy::new(&conn).await?;
    println!("Ready: {}", proxy.is_ready().await?);
    println!("Status: {}", proxy.status().await?);
    println!("Model: {}", proxy.model_name().await?);
    Ok(())
}

fn run_show_config() -> Result<()> {
    let cfg = config::Config::load()?;
    println!("Config file: {:?}", config::Config::default_path());
    println!();
    println!("{}", toml::to_string_pretty(&cfg)?);
    Ok(())
}
