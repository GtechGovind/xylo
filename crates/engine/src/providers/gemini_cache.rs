//! Short-lived explicit Gemini prefix caches, scoped to one authorized client.
use super::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::Instant;
use tokio::sync::Mutex;

#[derive(Default)]
pub(super) struct PrefixCache(Mutex<BTreeMap<String, (String, Instant)>>);

impl NativeProvider {
    /// Creates or reuses an explicit cache only for substantial, stable prefixes.
    pub(super) async fn cached_body(&self, body: &Value) -> Option<Value> {
        if self.config.family != ApiFamily::Gemini {
            return None;
        }
        let contents = body["contents"].as_array()?;
        if contents.is_empty() {
            return None;
        }
        let prefix_len = (contents.len() - 1).min(2);
        let mut prefix = json!({"model":format!("models/{}", self.config.model.trim_start_matches("models/")),"contents":&contents[..prefix_len],"ttl":"300s"});
        for key in ["systemInstruction", "tools"] {
            if let Some(value) = body.get(key) {
                prefix[key] = value.clone();
            }
        }
        let bytes = serde_json::to_vec(&prefix).ok()?;
        if bytes.len() < 32768 {
            return None;
        }
        let key = hex::encode(Sha256::digest(bytes));
        let mut entries = self.prefix_cache.0.lock().await;
        entries.retain(|_, (_, expiry)| *expiry > Instant::now());
        let name = if let Some((name, _)) = entries.get(&key) {
            name.clone()
        } else {
            let mut url = self.endpoint.base_url.clone();
            url.set_path(&format!(
                "{}/cachedContents",
                url.path().trim_end_matches('/')
            ));
            let response = self.client.post(url).json(&prefix).send().await.ok()?;
            if !response.status().is_success() {
                return None;
            }
            let result = read_json(response).await.ok()?;
            let name = result["name"].as_str()?.to_owned();
            if !name.starts_with("cachedContents/")
                || name.len() > 256
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_'))
            {
                return None;
            }
            if entries.len() >= 8 {
                entries.clear();
            }
            entries.insert(
                key,
                (name.clone(), Instant::now() + Duration::from_secs(240)),
            );
            name
        };
        let mut cached = body.clone();
        let fields = cached.as_object_mut()?;
        fields.remove("systemInstruction");
        fields.remove("tools");
        fields.insert("contents".into(), json!(&contents[prefix_len..]));
        fields.insert("cachedContent".into(), json!(name));
        Some(cached)
    }

    pub(super) async fn invalidate_cache(&self) {
        self.prefix_cache.0.lock().await.clear();
    }
}

#[cfg(test)]
mod tests;
