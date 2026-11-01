//! Explicit development-container setup using the existing native container target.
//! Discovery is read-only; setup starts only from the reviewed native settings action.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Offer {
    workspace: PathBuf,
    source: PathBuf,
    fingerprint: String,
    name: String,
    image: Option<String>,
    dockerfile: Option<PathBuf>,
    context: PathBuf,
    workspace_folder: String,
    post_create: Vec<String>,
}
/// Reads the supported devcontainer subset. Unsupported execution knobs are rejected visibly.
pub(super) fn discover(workspace: &Path) -> Result<Option<Offer>> {
    let workspace = workspace.canonicalize()?;
    let source = workspace.join(".devcontainer/devcontainer.json");
    if !source.exists() {
        return Ok(None);
    }
    let source = source.canonicalize()?;
    ensure!(
        source.starts_with(&workspace),
        "Development container configuration leaves the workspace"
    );
    let bytes = bounded_file(&source)?;
    let text = String::from_utf8(bytes.clone())?;
    let value: Value = serde_json::from_str(&json_comments(&text))?;
    for key in [
        "features",
        "mounts",
        "runArgs",
        "dockerComposeFile",
        "initializeCommand",
        "onCreateCommand",
        "updateContentCommand",
        "postStartCommand",
        "postAttachCommand",
        "containerEnv",
        "remoteEnv",
        "privileged",
        "capAdd",
        "securityOpt",
        "workspaceMount",
        "containerUser",
        "remoteUser",
        "updateRemoteUserUID",
        "overrideCommand",
        "shutdownAction",
        "init",
    ] {
        ensure!(
            value.get(key).is_none(),
            "Development container option {key} needs manual setup"
        );
    }
    if let Some(build) = value.get("build") {
        let build = build
            .as_object()
            .context("Development container build must be an object")?;
        for key in build.keys() {
            ensure!(
                matches!(key.as_str(), "dockerfile" | "context"),
                "Development container build option {key} needs manual setup"
            );
        }
    }
    let image = value["image"].as_str().map(str::to_owned);
    let dockerfile = value
        .pointer("/build/dockerfile")
        .or_else(|| value.get("dockerFile"))
        .and_then(Value::as_str)
        .map(|path| {
            source
                .parent()
                .unwrap_or(&workspace)
                .join(path)
                .canonicalize()
        })
        .transpose()?;
    ensure!(
        image.is_some() != dockerfile.is_some(),
        "Choose one development container image or Dockerfile"
    );
    if let Some(image) = &image {
        ensure!(
            !image.is_empty() && !image.starts_with('-') && !image.contains(char::is_whitespace),
            "Invalid development container image"
        );
    }
    let context = source
        .parent()
        .unwrap_or(&workspace)
        .join(
            value
                .pointer("/build/context")
                .or_else(|| value.get("context"))
                .and_then(Value::as_str)
                .unwrap_or("."),
        )
        .canonicalize()?;
    ensure!(
        context.starts_with(&workspace),
        "Development container build context leaves the workspace"
    );
    let mut digest = Sha256::new();
    digest.update(&bytes);
    if let Some(dockerfile) = &dockerfile {
        ensure!(
            dockerfile.starts_with(&workspace),
            "Development container Dockerfile leaves the workspace"
        );
        digest.update(bounded_file(dockerfile)?);
    }
    let folder = value["workspaceFolder"]
        .as_str()
        .unwrap_or("/workspace")
        .replace(
            "${localWorkspaceFolderBasename}",
            &workspace.file_name().unwrap_or_default().to_string_lossy(),
        );
    ensure!(
        folder.starts_with('/') && !folder.contains(['\0', '\n', '\r', ',', '$']),
        "Use an absolute container workspaceFolder without unsupported substitutions"
    );
    let post_create = match &value["postCreateCommand"] {
        Value::Null => Vec::new(),
        Value::String(command) => vec!["/bin/sh".into(), "-lc".into(), command.clone()],
        Value::Array(arguments) => arguments
            .iter()
            .map(|argument| {
                argument
                    .as_str()
                    .map(str::to_owned)
                    .context("postCreateCommand arguments must be strings")
            })
            .collect::<Result<Vec<_>>>()?,
        _ => bail!("postCreateCommand must be a command string or argument list"),
    };
    if !post_create.is_empty() {
        native::validate_command(&post_create)?;
    }
    Ok(Some(Offer {
        workspace,
        source,
        fingerprint: hex::encode(digest.finalize()),
        name: value["name"]
            .as_str()
            .unwrap_or("Development container")
            .chars()
            .take(160)
            .collect(),
        image,
        dockerfile,
        context,
        workspace_folder: folder,
        post_create,
    }))
}
fn bounded_file(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        std::fs::metadata(path)?.len() <= 256 * 1024,
        "Development container file exceeds 256 KiB"
    );
    Ok(std::fs::read(path)?)
}
/// Removes JSONC comments without interpreting strings, then removes trailing commas.
fn json_comments(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    let mut string = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if string {
            out.push(ch);
            if escaped {
                escaped = false
            } else if ch == '\\' {
                escaped = true
            } else if ch == '"' {
                string = false
            }
        } else if ch == '"' {
            string = true;
            out.push(ch);
        } else if ch == '/' && chars.peek() == Some(&'/') {
            for ch in chars.by_ref() {
                if ch == '\n' {
                    out.push(ch);
                    break;
                }
            }
        } else if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(ch) = chars.next() {
                if ch == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    let mut result = String::new();
    let mut chars = out.chars().peekable();
    string = false;
    escaped = false;
    while let Some(ch) = chars.next() {
        if !string
            && ch == ','
            && chars
                .clone()
                .find(|ch| !ch.is_whitespace())
                .is_some_and(|ch| matches!(ch, '}' | ']'))
        {
            continue;
        }
        result.push(ch);
        if string {
            if escaped {
                escaped = false
            } else if ch == '\\' {
                escaped = true
            } else if ch == '"' {
                string = false
            }
        } else if ch == '"' {
            string = true
        }
    }
    result
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Setup {
    operation_id: Uuid,
    offer: Offer,
    executable: PathBuf,
    runtime: ContainerRuntime,
}
pub(super) async fn create(
    manager: &Manager,
    workspace: &Path,
    args: &Value,
    cancel: CancellationToken,
) -> Result<Value> {
    let request: Setup = serde_json::from_value(args.clone())?;
    ensure!(
        request.offer.workspace == workspace.canonicalize()?,
        "Development container offer belongs to a different project"
    );
    let _lease = manager
        .operation_lease(request.operation_id, &cancel)
        .await?;
    let receipt_path = manager
        .root
        .join(format!("devcontainer-{}.json", request.operation_id));
    ensure!(
        !receipt_path.exists(),
        "This container setup was already attempted; inspect its receipt before another attempt"
    );
    let current = discover(&request.offer.workspace)?
        .context("Development container configuration was removed")?;
    ensure!(
        serde_json::to_value(&current)? == serde_json::to_value(&request.offer)?,
        "Development container configuration changed; review the updated offer"
    );
    let executable = process::validate_executable(&request.executable)?;
    let run = |args: Vec<String>, timeout| {
        let mut environment = BTreeMap::new();
        process::inherit(&mut environment, &["DOCKER_HOST", "CONTAINER_HOST"]);
        process::run(
            process::ProcessSpec {
                executable: executable.clone(),
                args,
                cwd: current.workspace.clone(),
                env: environment,
                timeout: Duration::from_secs(timeout),
                output_limit: 32768,
            },
            cancel.clone(),
        )
    };
    let name = format!("xylo-dev-{}", request.operation_id.simple());
    std::fs::write(
        &receipt_path,
        serde_json::to_vec(&json!({"state":"running","container":name,"source":current.source}))?,
    )?;
    let setup = async {
        let image = if let Some(image) = &current.image {
            image.clone()
        } else {
            let image = format!("xylo-dev:{}", &current.fingerprint[..24]);
            let result = run(
                vec![
                    "build".into(),
                    "--tag".into(),
                    image.clone(),
                    "--file".into(),
                    current
                        .dockerfile
                        .as_ref()
                        .context("Dockerfile missing")?
                        .display()
                        .to_string(),
                    current.context.display().to_string(),
                ],
                600,
            )
            .await?;
            ensure!(
                result.success,
                "Development container build failed: {}",
                result.stderr
            );
            image
        };
        ensure!(
            !current.workspace.to_string_lossy().contains(','),
            "Workspace path cannot contain a comma for container mounting"
        );
        let result = run(
            vec![
                "run".into(),
                "--detach".into(),
                "--name".into(),
                name.clone(),
                "--label".into(),
                format!("com.qurkos.xylo.devcontainer={}", request.operation_id),
                "--mount".into(),
                format!(
                    "type=bind,source={},target={}",
                    current.workspace.display(),
                    current.workspace_folder
                ),
                "--workdir".into(),
                current.workspace_folder.clone(),
                "--".into(),
                image,
                "/bin/sh".into(),
                "-c".into(),
                "while sleep 3600; do :; done".into(),
            ],
            600,
        )
        .await?;
        ensure!(
            result.success,
            "Development container creation failed: {}",
            result.stderr
        );
        let id = result.stdout.trim().to_owned();
        ensure!(
            id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Container runtime did not return an immutable ID"
        );
        if !current.post_create.is_empty() {
            let mut args = vec![
                "exec".into(),
                "--workdir".into(),
                current.workspace_folder.clone(),
                id.clone(),
            ];
            args.extend(current.post_create.clone());
            let result = run(args, 600).await?;
            ensure!(
                result.success,
                "Container exists but postCreateCommand failed: {}",
                result.stderr
            );
        }
        let saved = operations::save(
            manager,
            EnvironmentSaveRequest {
                operation_id: Uuid::new_v4(),
                expected_revision: 0,
                profile: EnvironmentProfile {
                    id: Uuid::new_v4(),
                    name: current.name.clone(),
                    target: EnvironmentTarget::Container {
                        executable: executable.clone(),
                        runtime: request.runtime,
                        container: ContainerTarget::Existing { target: id.clone() },
                    },
                },
            },
            cancel.clone(),
        )
        .await?;
        ensure!(
            saved.state == EnvironmentOperationState::Succeeded,
            "Container exists but saving its environment was not confirmed"
        );
        Ok::<_, anyhow::Error>(
            json!({"state":"succeeded","container":id,"environment":saved.environment}),
        )
    }
    .await;
    let receipt = match &setup {
        Ok(value) => value.clone(),
        Err(error) => {
            json!({"state":"outcome_unknown","container":name,"message":error.to_string(),"source":current.source})
        }
    };
    std::fs::write(&receipt_path, serde_json::to_vec(&receipt)?)?;
    setup.with_context(|| {
        format!(
            "Inspect container {name} before retrying; setup receipt: {}",
            receipt_path.display()
        )
    })?;
    list(manager, &cancel).await
}
#[cfg(test)]
mod tests;
