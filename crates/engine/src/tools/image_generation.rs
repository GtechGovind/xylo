//! Image generation uses one user-saved connection, captured for this turn.
//! Tool arguments cannot supply a provider, endpoint, credential or output path.
use super::*;
use serde::Deserialize;
use workbench_core::{ImageGenerationProvider, ImageGenerationRequest, ImageGenerationState};
use workbench_services::image_generation::ImageGenerationService;

pub(super) struct Configuration {
    provider: Option<ImageGenerationProvider>,
    error: Option<String>,
}
impl Configuration {
    pub(super) fn load(artifacts: &std::path::Path) -> Self {
        let result = (|| {
            ImageGenerationService::new(
                artifacts
                    .parent()
                    .context("Missing application data directory")?
                    .to_path_buf(),
                artifacts.to_path_buf(),
            )?
            .configured_provider()
        })();
        match result {
            Ok(provider) => Self {provider,error:None},
            Err(_) => Self {provider:None,error:Some("Saved image provider is invalid. Review the Images page before using image_generate.".into())},
        }
    }
    fn provider(&self) -> Result<&ImageGenerationProvider> {
        if let Some(error) = &self.error {
            bail!("{error}");
        }
        self.provider.as_ref().context("Choose and save an image provider and model on the Images page before using image_generate")
    }
    pub(super) fn definition(&self) -> ToolDefinition {
        let selection = self
            .provider
            .as_ref()
            .map(|provider| {
                format!(
                    " Selected connection: {}; model: {}.",
                    provider.name, provider.model
                )
            })
            .unwrap_or_default();
        ToolDefinition {
            name: "image_generate".into(),
            description: format!(
                "Generate one image with the image provider explicitly saved by the user, after write approval. No automatic provider fallback or retry. A new request may incur a charge.{selection}"
            ),
            writes: true,
            parameters: json!({"type":"object","properties":{"prompt":{"type":"string","minLength":1,"maxLength":16384},"size":{"type":"string","enum":["auto","256x256","512x512","1024x1024","1024x1536","1536x1024"],"default":"1024x1024"}},"required":["prompt"],"additionalProperties":false}),
        }
    }
    pub(super) fn validate(&self, arguments: &Value) -> Result<()> {
        self.provider()?;
        let args: Arguments = serde_json::from_value(arguments.clone())?;
        ensure!(
            !args.prompt.trim().is_empty() && args.prompt.len() <= 16 * 1024,
            "Image prompt must contain at most 16 KiB"
        );
        ensure!(
            [
                "auto",
                "256x256",
                "512x512",
                "1024x1024",
                "1024x1536",
                "1536x1024"
            ]
            .contains(&args.size.as_str()),
            "Unsupported image size"
        );
        Ok(())
    }
    pub(super) fn approval(&self, arguments: &Value) -> Result<Value> {
        let provider = self.provider()?;
        let mut arguments = arguments.clone();
        arguments["selected_image_provider"] =
            json!({"name":provider.name,"model":provider.model,"protocol":provider.protocol});
        Ok(arguments)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    prompt: String,
    #[serde(default = "default_size")]
    size: String,
}
fn default_size() -> String {
    "1024x1024".into()
}

#[derive(Debug)]
pub(super) struct DefinitiveFailure(pub String);
impl std::fmt::Display for DefinitiveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for DefinitiveFailure {}

pub(super) fn execute(broker: &ToolBroker, id: Uuid, arguments: &Value) -> Result<Value> {
    let provider = broker.image_generation.provider()?.clone();
    let args: Arguments = serde_json::from_value(arguments.clone())?;
    let data = broker
        .artifacts
        .parent()
        .context("Missing application data directory")?;
    let service = ImageGenerationService::new(data.to_path_buf(), broker.artifacts.clone())?;
    let secret=provider.credential_ref.as_deref().map(|reference|{
        CredentialVault::new(workbench_core::CREDENTIAL_SERVICE)?.get(reference)?.context("Saved image provider credential is unavailable")
    }).transpose().map_err(|_|DefinitiveFailure("Read the selected image credential from the OS credential store before trying again. No request was dispatched.".into()))?;
    let receipt = broker.runtime.block_on(async {
        let resources = workbench_runtime::resources::ResourceManager::new();
        let _lease = resources
            .acquire(
                workbench_runtime::resources::TaskDemand {
                    memory_bytes: 384 * 1024 * 1024,
                    cpu_share: 1.0,
                },
                broker.cancel.child_token(),
            )
            .await?;
        service
            .generate(
                ImageGenerationRequest {
                    id,
                    session_id: broker.request.session_id,
                    provider,
                    prompt: args.prompt,
                    size: args.size,
                },
                secret,
                broker.cancel.child_token(),
            )
            .await
    });
    let receipt = match receipt {
        Ok(receipt) => receipt,
        Err(error) => {
            if matches!(service.receipt(id), Ok(None)) {
                return Err(DefinitiveFailure(error.to_string()).into());
            }
            return Err(error);
        }
    };
    match receipt.state {
        ImageGenerationState::Succeeded => Ok(
            json!({"status":"success","summary":{"generation_id":receipt.id,"provider":receipt.provider_name,"model":receipt.model,"message":receipt.message,"images":receipt.images.iter().map(|image|&image.attachment).collect::<Vec<_>>()},"artifacts":receipt.images.iter().map(|image|&image.artifact_path).collect::<Vec<_>>(),"next_actions":["The generated image is saved locally; use its managed attachment reference or artifact."]}),
        ),
        ImageGenerationState::Cancelled | ImageGenerationState::Failed => Err(DefinitiveFailure(
            format!("{} Generation receipt: {}", receipt.message, receipt.id),
        )
        .into()),
        _ => bail!(
            "{} Generation receipt: {}. Do not automatically generate another image.",
            receipt.message,
            receipt.id
        ),
    }
}

#[cfg(test)]
mod tests;
