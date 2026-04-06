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

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use tracing::{error, info};
use zbus::{connection, interface, SignalContext};

use crate::{llm::Engine, prompt::Style};

pub(crate) const SERVICE_NAME: &str = "org.rewriteit.Rewriter1";
pub(crate) const OBJECT_PATH: &str = "/org/rewriteit/Rewriter";

/// Module-level slot for the session-bus connection.
///
/// Set once in `serve()` before the service is advertised.  Because the binary
/// is a single-process daemon this is the natural home for the connection.
/// Stored at `'static` so that spawned tasks can obtain `&'static Connection`
/// and build a `SignalContext<'static>` for signal emission.
static DBUS_CONN: OnceLock<zbus::Connection> = OnceLock::new();

// ── Client proxy ─────────────────────────────────────────────────────────────

/// Auto-generated async client proxy for the Rewriter DBus interface.
///
/// Created by consumers via `RewriterProxy::new(&connection).await?`.
#[zbus::proxy(
    interface   = "org.rewriteit.Rewriter1",
    default_path    = "/org/rewriteit/Rewriter",
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
}

// ── Server ────────────────────────────────────────────────────────────────────

/// The live object registered on the session bus.
pub(crate) struct RewriterService {
    engine: Arc<Engine>,
}

impl RewriterService {
    pub(crate) fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }
}

#[interface(name = "org.rewriteit.Rewriter1")]
impl RewriterService {
    // ── Methods ──────────────────────────────────────────────────────────────

    /// Rewrite `text` using `style`; returns when the model is done.
    ///
    /// `text` must be non-empty and ≤ 32 000 bytes. `style` must be one of the
    /// values returned by `ListStyles`; unknown values default to "grammar".
    async fn rewrite(
        &self,
        text: String,
        style: String,
    ) -> zbus::fdo::Result<String> {
        validate_text(&text)?;

        let style   = Style::from_str(&style);
        let engine  = Arc::clone(&self.engine);

        tokio::task::spawn_blocking(move || engine.rewrite(&text, style))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("task join error: {e}")))?
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Start an asynchronous rewrite; returns a unique `job_id` immediately.
    ///
    /// The service emits `Chunk(job_id, text)` for each token and then either
    /// `Done(job_id)` or `Error(job_id, message)` when finished.
    async fn start_rewrite(
        &self,
        text: String,
        style: String,
    ) -> zbus::fdo::Result<String> {
        validate_text(&text)?;

        let job_id = uuid::Uuid::new_v4().to_string();
        let style  = Style::from_str(&style);
        let engine = Arc::clone(&self.engine);
        let job    = job_id.clone();

        tokio::spawn(async move {
            // DBUS_CONN is set before any client can call StartRewrite.
            let conn: &'static zbus::Connection = match DBUS_CONN.get() {
                Some(c) => c,
                None => {
                    error!("start_rewrite: DBUS_CONN not yet set");
                    return;
                }
            };

            // SignalContext<'static> because conn is &'static Connection and
            // OBJECT_PATH is &'static str.
            let ctxt = match SignalContext::new(conn, OBJECT_PATH) {
                Ok(c)  => c,
                Err(e) => { error!("failed to build SignalContext: {e}"); return; }
            };

            let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);

            // Run inference in a blocking thread.
            let inference = tokio::task::spawn_blocking(move || {
                engine.rewrite_streaming(&text, style, tx)
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

    /// Return `true`; the model is always loaded before the service starts.
    async fn is_ready(&self) -> zbus::fdo::Result<bool> {
        Ok(true)
    }

    /// Return metadata about the running service.
    #[zbus(property)]
    fn model_name(&self) -> String {
        self.engine.model_name()
    }

    // ── Signals ───────────────────────────────────────────────────────────────

    /// A chunk of generated text for the given streaming job.
    #[zbus(signal)]
    async fn chunk(
        ctxt: &SignalContext<'_>,
        job_id: &str,
        text: &str,
    ) -> zbus::Result<()>;

    /// The streaming job completed successfully.
    #[zbus(signal)]
    async fn done(ctxt: &SignalContext<'_>, job_id: &str) -> zbus::Result<()>;

    /// The streaming job failed with `message`.
    #[zbus(signal)]
    async fn error(
        ctxt: &SignalContext<'_>,
        job_id: &str,
        message: &str,
    ) -> zbus::Result<()>;
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
pub async fn serve(engine: Arc<Engine>) -> Result<()> {
    let service = RewriterService::new(Arc::clone(&engine));

    let conn = connection::Builder::session()?
        .name(SERVICE_NAME)?
        .serve_at(OBJECT_PATH, service)?
        .build()
        .await?;

    // Store the connection in the static so spawned tasks can obtain
    // `&'static Connection` for building `SignalContext<'static>`.
    DBUS_CONN
        .set(conn)
        .expect("DBUS_CONN already set — this is a bug");

    info!("DBus service ready: {SERVICE_NAME} @ {OBJECT_PATH}");
    info!("Press Ctrl-C to stop.");

    tokio::signal::ctrl_c().await?;
    info!("shutting down");

    Ok(())
}
