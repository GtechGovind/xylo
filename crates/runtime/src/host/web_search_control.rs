//! Search settings save credentials before publishing non-secret references.
use super::*;
use xylo_core::{WebSearchBackend, WebSearchSettings};

fn validate(settings: &WebSearchSettings) -> Result<()> {
    if settings.backend == WebSearchBackend::Searxng {
        let url = url::Url::parse(&settings.endpoint).context("Enter a valid SearXNG HTTPS URL")?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "SearXNG needs an HTTPS URL without credentials, a query or a fragment"
        );
    }
    Ok(())
}

impl HostState {
    pub(super) async fn save_web_search(
        &self,
        mut config: WebSearchSettings,
        secret: Option<String>,
    ) -> Result<()> {
        validate(&config)?;
        if config.backend != WebSearchBackend::Searxng {
            config.endpoint.clear();
        }
        let _save = self.provider_save_gate.lock().await;
        let mut settings = self.settings.lock().await.clone();
        let old = settings.web_search.clone();
        let vault = self.vault()?;
        // A caller cannot substitute a provider or connector credential reference.
        config.credential_ref = if config.backend == old.backend {
            old.credential_ref.clone()
        } else {
            None
        };
        let key = secret.filter(|key| !key.trim().is_empty());
        let mut created = None;
        if config.backend == WebSearchBackend::Disabled {
            config = WebSearchSettings::default();
        } else if let Some(key) = key {
            ensure!(key.len() <= 16384, "Search API key is too long");
            let reference = format!("web-search:{}", Uuid::new_v4());
            let (write_vault, write_reference) = (vault.clone(), reference.clone());
            tokio::task::spawn_blocking(move || write_vault.set(&write_reference, &key)).await??;
            config.credential_ref = Some(reference.clone());
            created = Some(reference);
        }
        if matches!(
            config.backend,
            WebSearchBackend::Brave | WebSearchBackend::Tavily
        ) {
            let reference = config
                .credential_ref
                .as_deref()
                .context("Enter an API key for this search service")?;
            ensure!(
                vault.get(reference)?.is_some(),
                "The saved search key is unavailable. Enter it again."
            );
        }
        settings.web_search = config.clone();
        let saved = self
            .commit_settings(settings, settings_control::ProjectFields::Current)
            .await;
        match saved {
            Ok(saved) => {
                if old.credential_ref != config.credential_ref
                    && let Some(reference) = old.credential_ref
                {
                    let vault = vault.clone();
                    tokio::task::spawn_blocking(move || vault.delete(&reference)).await??;
                }
                self.events.send(HostEvent::Settings(saved)).await?;
            }
            Err(error) => {
                if let Some(reference) = created {
                    let _ = tokio::task::spawn_blocking(move || vault.delete(&reference)).await;
                }
                return Err(error);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
