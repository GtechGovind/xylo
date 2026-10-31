//! Opt-in runs against real providers.
//!
//! A matrix file lists the models to evaluate. Live runs spend money and
//! send the task fixtures to the provider, so they happen only when the
//! environment variable [`LIVE_ENV`] is `1`; credentials are read from the
//! environment variables each entry names and are never written to traces or
//! reports.
//!
//! ```json
//! [
//!   {
//!     "label": "gpt-4.1-mini",
//!     "credential_env": "OPENAI_API_KEY",
//!     "provider": {
//!       "name": "OpenAI", "family": "open_ai_chat",
//!       "endpoint": "https://api.openai.com/v1", "model": "gpt-4.1-mini",
//!       "credential_ref": null, "context_tokens": 128000,
//!       "output_tokens": 8192, "timeout_seconds": 120
//!     }
//!   }
//! ]
//! ```

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{fmt, path::Path};
use xylo_core::ProviderConfig;

/// The environment variable that must be `1` for live runs.
pub const LIVE_ENV: &str = "XYLO_EVALS_LIVE";

/// A real model to evaluate.
#[derive(Clone)]
pub struct LiveModel {
    /// Name used in reports and trace directories.
    pub label: String,
    pub provider: ProviderConfig,
    /// The provider secret, from the environment.
    pub credential: Option<String>,
}

impl fmt::Debug for LiveModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveModel")
            .field("label", &self.label)
            .field("model", &self.provider.model)
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    label: String,
    provider: ProviderConfig,
    #[serde(default)]
    credential_env: Option<String>,
}

/// Reads a matrix file, refusing unless live runs were opted into.
pub fn load(path: &Path) -> Result<Vec<LiveModel>> {
    ensure!(
        std::env::var(LIVE_ENV).as_deref() == Ok("1"),
        "Live runs call real providers and are opt-in: set {LIVE_ENV}=1"
    );
    let bytes = std::fs::read(path).with_context(|| format!("Read {}", path.display()))?;
    let entries: Vec<Entry> = serde_json::from_slice(&bytes)
        .with_context(|| format!("Parse model matrix {}", path.display()))?;
    ensure!(!entries.is_empty(), "The model matrix lists no models");
    entries
        .into_iter()
        .map(|entry| {
            entry
                .provider
                .validate()
                .with_context(|| format!("Model {}", entry.label))?;
            let credential = entry
                .credential_env
                .as_deref()
                .map(|name| {
                    std::env::var(name).with_context(|| {
                        format!(
                            "Model {} needs the environment variable {name}",
                            entry.label
                        )
                    })
                })
                .transpose()?;
            Ok(LiveModel {
                label: entry.label,
                provider: entry.provider,
                credential,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
