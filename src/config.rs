use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_HF_REPO: &str = "unsloth/Phi-4-mini-instruct-GGUF";
const DEFAULT_HF_FILENAME: &str = "Phi-4-mini-instruct-Q4_K_M.gguf";

/// Settings read from `~/.config/rewrite-it/config.toml` (created with defaults on first run).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Absolute path to the GGUF model file. Auto-downloaded when absent.
    pub model_path: PathBuf,
    /// HuggingFace repository for the automatic download fallback.
    pub hf_repo: String,
    /// Filename within the HF repository to download.
    pub hf_filename: String,
    /// KV-cache context window in tokens.
    pub context_size: u32,
    /// Maximum *new* tokens the model may generate per request.
    pub max_tokens: u32,
    /// Sampling temperature (0 < t ≤ 2). Lower = more deterministic.
    pub temperature: f32,
    /// Number of transformer layers to offload to the GPU (0 = CPU-only).
    pub n_gpu_layers: u32,
    /// CPU thread count for inference (None → llama.cpp auto-detect).
    pub n_threads: Option<i32>,
    /// Random seed for reproducible sampling.
    pub seed: u32,
    /// Seconds of inactivity before the daemon exits automatically (None = never).
    pub idle_timeout_secs: Option<u64>,
    /// Maximum seconds a single inference request may run before it is
    /// considered hung and returns an error (watchdog also uses this).
    pub inference_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        let model_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from(".local/share"))
            .join("rewrite-it")
            .join("models");

        Self {
            model_path: model_dir.join(DEFAULT_HF_FILENAME),
            hf_repo: DEFAULT_HF_REPO.to_string(),
            hf_filename: DEFAULT_HF_FILENAME.to_string(),
            context_size: 2048,
            max_tokens: 512,
            temperature: 0.3,
            n_gpu_layers: 0,
            n_threads: None,
            seed: 42,
            idle_timeout_secs: Some(300),
            inference_timeout_secs: 120,
        }
    }
}

impl Config {
    /// Load from the default config path, creating defaults when the file is absent.
    pub fn load() -> Result<Self> {
        let path = Self::default_path();
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config {path:?}"))?;
            toml::from_str(&raw).with_context(|| format!("parsing config {path:?}"))
        } else {
            Ok(Self::default())
        }
    }

    /// Persist the current settings to disk (creates parent directories as needed).
    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        let path = Self::default_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating config directory {dir:?}"))?;
        }
        let contents = toml::to_string_pretty(self).context("serialising config")?;
        std::fs::write(&path, contents).with_context(|| format!("writing config {path:?}"))
    }

    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from(".config"))
            .join("rewrite-it")
            .join("config.toml")
    }
}
