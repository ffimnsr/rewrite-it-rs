use std::{io::Read, str::FromStr, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use rewrite_it::{config::Config, llm::Engine, model, prompt::Style};
use tracing::info;

#[derive(Parser)]
#[command(
    name = "llm-test",
    version,
    about = "Direct LLM smoke-test CLI for rewrite-it",
    long_about = None,
)]
struct Cli {
    /// Text to rewrite directly through the local model. Reads stdin when omitted.
    text: Option<String>,

    /// Rewriting style.
    #[arg(short, long, default_value = "grammar")]
    style: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cli = Cli::parse();
    let text = read_input(cli.text)?;

    if text.is_empty() {
        anyhow::bail!("no text provided (pass it as an argument or via stdin)");
    }

    let style = Style::from_str(&cli.style).expect("style parsing is infallible");
    let config = Arc::new(Config::load()?);
    info!(path = ?Config::default_path(), "config loaded");

    let model_path = model::ensure_model(&config).await?;
    let mut effective_config = (*config).clone();
    effective_config.model_path = model_path;
    let effective_config = Arc::new(effective_config);

    info!("loading LLM for direct smoke test");
    let engine = tokio::task::spawn_blocking({
        let cfg = Arc::clone(&effective_config);
        move || Engine::load(cfg)
    })
    .await
    .context("model-loading thread panicked")??;

    let result = tokio::task::spawn_blocking(move || engine.rewrite(&text, style))
        .await
        .context("inference thread panicked")??;

    println!("{result}");
    Ok(())
}

fn read_input(text: Option<String>) -> Result<String> {
    match text {
        Some(text) => Ok(text),
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading text from stdin")?;
            Ok(buf.trim().to_string())
        }
    }
}
