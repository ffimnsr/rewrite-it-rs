//! DBus service implementation and typed client proxy.
//!
//! Service name : `org.rewriteit.Rewriter1`
//! Object path  : `/org/rewriteit/Rewriter`
//!
//! # Methods (server → exposed to clients)
//!
//! | Method | Signature | Description |
//! |--------|-----------|-------------|
//! | `Rewrite` | `(s text, s style) → s` | Blocking full rewrite |
//! | `StartRewrite` | `(s text, s style) → s job_id` | Async streaming rewrite |
//! | `ListStyles` | `() → as` | Available style names |
//! | `IsReady` | `() → b` | Model loaded and ready |
//!
//! # Signals (emitted during `StartRewrite`)
//!
//! | Signal | Signature | Description |
//! |--------|-----------|-------------|
//! | `Chunk` | `(s job_id, s text)` | One generated token piece |
//! | `Done`  | `(s job_id)` | Generation finished successfully |
//! | `Error` | `(s job_id, s message)` | Generation failed |

use std::{
    str::FromStr,
    sync::{Arc, OnceLock},
};

use anyhow::Result;
use sd_notify::NotifyState;
use tracing::{error, info};
use zbus::{connection, interface, SignalContext};

use crate::{llm::EngineManager, prompt::Style};

pub(crate) const SERVICE_NAME: &str = "org.rewriteit.Rewriter1";
pub(crate) const OBJECT_PATH: &str = "/org/rewriteit/Rewriter";

// ── Client proxy ─────────────────────────────────────────────────────────────

/// Auto-generated async client proxy for the Rewriter DBus interface.
///
/// Created by consumers via `RewriterProxy::new(&connection).await?`.
#[zbus::proxy(
    interface = "org.rewriteit.Rewriter1",
    default_path = "/org/rewriteit/Rewriter",
    default_service = "org.rewriteit.Rewriter1",
    gen_blocking = false
)]
pub trait Rewriter {
    /// Blocking full rewrite; awaits until the model finishes generating.
    async fn rewrite(&self, text: &str, style: &str) -> zbus::Result<String>;

    /// Start a streaming rewrite; returns a job_id.  Subscribe to `Chunk` /
    /// `Done` / `Error` signals filtered by that job_id for the output.
    async fn start_rewrite(&self, text: &str, style: &str) -> zbus::Result<String>;

    /// Return the names of all supported rewriting styles.
    async fn list_styles(&self) -> zbus::Result<Vec<String>>;

    /// True when the model is loaded and ready to accept requests.
    async fn is_ready(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn model_name(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;
}

// ── Server ────────────────────────────────────────────────────────────────────

/// The live object registered on the session bus.
pub(crate) struct RewriterService {
    engine: Arc<EngineManager>,
    /// Filled exactly once in `serve()` right after the connection is built.
    /// Stored as an instance-level `OnceLock` (not a `static`) so there is no
    /// global mutable state and multiple service instances can coexist in tests.
    conn: Arc<OnceLock<zbus::Connection>>,
}

impl RewriterService {
    pub(crate) fn new(
        engine: Arc<EngineManager>,
        conn: Arc<OnceLock<zbus::Connection>>,
    ) -> Self {
        Self { engine, conn }
    }
}

#[interface(name = "org.rewriteit.Rewriter1")]
impl RewriterService {
    // ── Methods ──────────────────────────────────────────────────────────────

    /// Rewrite `text` using `style`; returns when the model is done.
    ///
    /// `text` must be non-empty and ≤ 32 000 bytes. `style` must be one of the
    /// values returned by `ListStyles`; unknown values default to "grammar".
    async fn rewrite(&self, text: String, style: String) -> zbus::fdo::Result<String> {
        validate_text(&text)?;
        self.engine.touch();

        let style = Style::from_str(&style).expect("style parsing is infallible");
        let engine = self
            .engine
            .ensure_ready()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        tokio::task::spawn_blocking(move || engine.rewrite(&text, style))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("task join error: {e}")))?
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Start an asynchronous rewrite; returns a unique `job_id` immediately.
    ///
    /// The service emits `Chunk(job_id, text)` for each token and then either
    /// `Done(job_id)` or `Error(job_id, message)` when finished.
    async fn start_rewrite(&self, text: String, style: String) -> zbus::fdo::Result<String> {
        validate_text(&text)?;
        self.engine.touch();

        let job_id = uuid::Uuid::new_v4().to_string();
        let style = Style::from_str(&style).expect("style parsing is infallible");
        let engine = Arc::clone(&self.engine);
        let conn_once = Arc::clone(&self.conn);
        let job = job_id.clone();

        tokio::spawn(async move {
            // conn_once is set in serve() before any client can reach StartRewrite.
            let conn: zbus::Connection = match conn_once.get() {
                Some(c) => c.clone(), // zbus::Connection is cheaply cloneable (Arc internally)
                None => {
                    error!("start_rewrite: connection not yet initialised");
                    return;
                }
            };

            // `conn` is owned by this async block; `ctxt` borrows it and is
            // valid for the full duration of the block.
            let ctxt = match SignalContext::new(&conn, OBJECT_PATH) {
                Ok(c) => c,
                Err(e) => {
                    error!("failed to build SignalContext: {e}");
                    return;
                }
            };

            let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);

            let inference = tokio::spawn(async move {
                let engine = engine.ensure_ready().await?;
                tokio::task::spawn_blocking(move || engine.rewrite_streaming(&text, style, tx))
                    .await
                    .map_err(|e| anyhow::anyhow!("task join error: {e}"))?
            });

            // Forward each token piece as a DBus signal.
            while let Some(chunk) = rx.recv().await {
                if let Err(e) = RewriterService::chunk(&ctxt, &job, &chunk).await {
                    error!("failed to emit Chunk signal: {e}");
                    break;
                }
            }

            // Emit completion or error signal.
            match inference.await {
                Ok(Ok(())) => {
                    let _ = RewriterService::done(&ctxt, &job).await;
                }
                Ok(Err(e)) => {
                    let _ = RewriterService::error(&ctxt, &job, &e.to_string()).await;
                }
                Err(e) => {
                    let _ = RewriterService::error(&ctxt, &job, &e.to_string()).await;
                }
            }
        });

        Ok(job_id)
    }

    /// Return the list of supported style names.
    async fn list_styles(&self) -> zbus::fdo::Result<Vec<String>> {
        Ok(Style::all_names().iter().map(|s| s.to_string()).collect())
    }

    /// Return `true` when the model has already been downloaded and loaded.
    async fn is_ready(&self) -> zbus::fdo::Result<bool> {
        Ok(self.engine.is_ready())
    }

    /// Return metadata about the running service.
    #[zbus(property)]
    fn model_name(&self) -> String {
        self.engine.model_name()
    }

    /// Return the current model lifecycle status.
    #[zbus(property)]
    fn status(&self) -> String {
        self.engine.status()
    }

    // ── Signals ───────────────────────────────────────────────────────────────

    /// A chunk of generated text for the given streaming job.
    #[zbus(signal)]
    async fn chunk(ctxt: &SignalContext<'_>, job_id: &str, text: &str) -> zbus::Result<()>;

    /// The streaming job completed successfully.
    #[zbus(signal)]
    async fn done(ctxt: &SignalContext<'_>, job_id: &str) -> zbus::Result<()>;

    /// The streaming job failed with `message`.
    #[zbus(signal)]
    async fn error(ctxt: &SignalContext<'_>, job_id: &str, message: &str) -> zbus::Result<()>;
}

// ── Input validation ─────────────────────────────────────────────────────────

fn validate_text(text: &str) -> zbus::fdo::Result<()> {
    if text.trim().is_empty() {
        return Err(zbus::fdo::Error::InvalidArgs(
            "text must not be empty".into(),
        ));
    }
    // 32 000 bytes ≈ ~24 000 English words — well within Phi-4-mini's context.
    if text.len() > 32_000 {
        return Err(zbus::fdo::Error::InvalidArgs(
            "text exceeds maximum length (32 000 bytes)".into(),
        ));
    }
    Ok(())
}

// ── Service entry-point ───────────────────────────────────────────────────────

/// Build and run the DBus session-bus service.  Returns when Ctrl-C is received.
pub async fn serve(engine: Arc<EngineManager>) -> Result<()> {
    let conn_once: Arc<OnceLock<zbus::Connection>> = Arc::new(OnceLock::new());
    let service = RewriterService::new(Arc::clone(&engine), Arc::clone(&conn_once));

    let conn = connection::Builder::session()?
        .name(SERVICE_NAME)?
        .serve_at(OBJECT_PATH, service)?
        .build()
        .await?;

    // Publish the connection so spawned signal tasks can reach it.
    conn_once
        .set(conn)
        .expect("conn_once already set — this is a bug");

    info!("DBus service ready: {SERVICE_NAME} @ {OBJECT_PATH}");
    // Notify systemd that the service is up (no-op when not running under systemd).
    let _ = sd_notify::notify(false, &[NotifyState::Ready]);
    info!("Press Ctrl-C to stop.");

    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    let _ = sd_notify::notify(false, &[NotifyState::Stopping]);

    Ok(())
}
