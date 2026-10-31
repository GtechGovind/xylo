//! Activation verifies the installed bytes again before exposing owned paths.
use super::*;

impl AssetManager {
    pub(super) async fn verified_exports(
        &self,
        id: &str,
        cancel: &CancellationToken,
    ) -> Result<(PathBuf, BTreeMap<String, PathBuf>)> {
        let (root, receipt) = self.receipt(self.asset(id)?)?;
        for file in receipt.files {
            let path = root.join(&file.path);
            let canonical = std::fs::canonicalize(&path)?;
            ensure!(
                canonical.starts_with(&root),
                "Installed asset escapes owned root"
            );
            download::verify(&canonical, file.size_bytes, &file.sha256, cancel).await?;
        }
        let exports = receipt
            .exports
            .into_iter()
            .map(|(key, path)| {
                let path = std::fs::canonicalize(root.join(path))?;
                ensure!(
                    path.starts_with(&root) && path.is_file(),
                    "Invalid installed export"
                );
                Ok((key, path))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok((root, exports))
    }

    pub async fn activation(&self, id: &str, cancel: &CancellationToken) -> Result<Activation> {
        let plan = self.plan(id)?;
        let mut installed = BTreeMap::new();
        for item in plan {
            let verified = match self.verified_exports(&item, cancel).await {
                Ok(verified) => verified,
                Err(error) => {
                    if !cancel.is_cancelled()
                        && let Ok(path) = self.location(self.asset(&item)?)
                    {
                        let receipt = path.join("receipt.json");
                        if receipt.is_file() {
                            let _ = std::fs::rename(&receipt, path.join("receipt.invalid.json"));
                        }
                    }
                    return Err(error.context(
                        "Installed asset verification failed; choose Install again to repair",
                    ));
                }
            };
            installed.insert(item.clone(), verified);
        }
        let export = |asset: &str, key: &str| -> Result<PathBuf> {
            installed
                .get(asset)
                .and_then(|(_, exports)| exports.get(key))
                .cloned()
                .with_context(|| format!("Installed {asset} has no verified {key}"))
        };
        let speech = |engine: &str, model: Option<&str>| -> Result<Value> {
            let mut assets = BTreeMap::new();
            if let Some(model) = model {
                assets.insert("model", export(model, "model")?);
            }
            if engine == "sherpa-runtime" {
                if let Some(model) = model {
                    assets.insert("tokens", export(model, "tokens")?);
                    assets.insert("lexicon", export(model, "lexicon")?);
                }
                Ok(
                    json!({"executable":export(engine,"executable")?,"assets":assets,"args":[],"output_extension":"wav","synthesis_args":["--vits-model={model}","--vits-lexicon={lexicon}","--vits-tokens={tokens}","--num-threads={threads}","--output-filename={output}","{text}"]}),
                )
            } else {
                Ok(
                    json!({"executable":export(engine,"executable")?,"assets":assets,"args":["-m","{model}","-f","{input}","-t","{threads}","-of","{output_stem}","-otxt"],"output_extension":"txt"}),
                )
            }
        };
        let mut result = Activation {
            asset_id: id.to_owned(),
            config_patch: None,
            embedding_config: None,
            language_server: None,
        };
        result.config_patch = match id {
            "llama-runtime" => Some(json!({"llama":{"executable":export(id,"executable")?}})),
            "qwen3-0.6b-q8" | "qwen3-1.7b-q8" => Some(
                json!({"llama":{"executable":export("llama-runtime","executable")?,"model":export(id,"model")?}}),
            ),
            "pdfium-runtime" => Some(json!({"pdfium_library":export(id,"library")?})),
            "whisper-runtime" => Some(json!({"whisper":speech(id,None)?})),
            "whisper-tiny-en" => Some(json!({"whisper":speech("whisper-runtime",Some(id))?})),
            "sherpa-runtime" => Some(json!({"sherpa":speech(id,None)?})),
            "vits-ljs-int8" => Some(json!({"sherpa":speech("sherpa-runtime",Some(id))?})),
            "bge-small-en-v1.5" => {
                result.embedding_config = Some(
                    json!({"directory":installed.get(id).context("Embedding installation missing")?.0,"model_id":format!("BAAI/bge-small-en-v1.5@{}",self.asset(id)?.revision),"dimensions":384,"cpu_threads":0,"pooling":"cls","runtime_library":export("onnx-runtime","library")?}),
                );
                None
            }
            "rust-analyzer" => {
                result.language_server = Some(
                    json!({"languages":["rust"],"executable":export(id,"executable")?,"args":[],"initialization_options":{"cargo":{"buildScripts":{"enable":false}},"procMacro":{"enable":false},"checkOnSave":false}}),
                );
                None
            }
            "onnx-runtime" => anyhow::bail!(
                "Choose the embedding model to activate its compatible ONNX runtime together"
            ),
            _ => anyhow::bail!("This reviewed asset has no activation adapter"),
        };
        Ok(result)
    }

    /// Embedding configuration is shared with the lightweight host. It never
    /// loads a model in this process. The host reports whether reload is needed.
    pub fn save_embedding_configuration(&self, configuration: &Value) -> Result<()> {
        write_json(
            &self
                .root
                .parent()
                .context("Asset data root missing")?
                .join("embedding.json"),
            configuration,
        )
    }
    pub fn save_language_server_configuration(&self, server: &Value) -> Result<()> {
        let path = self
            .root
            .parent()
            .context("Asset data root missing")?
            .join("language-servers.json");
        let mut configuration = if path.exists() {
            read_json::<Value>(&path, 1024 * 1024)?
        } else {
            json!({"servers":[]})
        };
        let servers = configuration
            .get_mut("servers")
            .and_then(Value::as_array_mut)
            .context("Language server configuration requires servers array")?;
        ensure!(servers.len() < 128, "Too many language server overrides");
        // Replace only Rust; a multi-language override retains its other languages.
        for existing in servers.iter_mut() {
            if let Some(languages) = existing.get_mut("languages").and_then(Value::as_array_mut) {
                languages.retain(|language| {
                    language
                        .as_str()
                        .is_none_or(|language| !language.eq_ignore_ascii_case("rust"))
                });
            }
        }
        servers.retain(|existing| {
            existing
                .get("languages")
                .and_then(Value::as_array)
                .is_none_or(|languages| !languages.is_empty())
        });
        servers.push(server.clone());
        write_json(&path, &configuration)
    }
}

#[cfg(test)]
mod tests;
