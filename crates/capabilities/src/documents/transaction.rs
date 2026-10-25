//! Atomic no-clobber publication with immutable request identity and recovery.
use super::{check, editing};
use crate::CapabilityConfig;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workbench_core::*;
#[derive(Serialize, Deserialize)]
struct Operation {
    digest: String,
    receipt: DocumentSaveReceipt,
}
pub struct Staging {
    pub path: PathBuf,
    file: Option<tempfile::NamedTempFile>,
}
impl Staging {
    pub fn new(artifacts: &Path) -> Result<Self> {
        let file = tempfile::Builder::new()
            .prefix(".document-")
            .suffix(".partial")
            .tempfile_in(artifacts)?;
        let path = file.path().to_path_buf();
        Ok(Self {
            path,
            file: Some(file),
        })
    }
    pub fn commit(&mut self, output: &Path, cancel: &CancellationToken) -> Result<()> {
        check(cancel)?;
        self.file
            .as_ref()
            .context("Document staging was already consumed")?
            .as_file()
            .sync_all()?;
        check(cancel)?;
        self.file
            .take()
            .unwrap()
            .persist_noclobber(output)
            .map_err(|error| error.error)
            .context("Publish document without overwriting an existing file")?;
        #[cfg(unix)]
        File::open(output.parent().context("Output parent missing")?)?.sync_all()?;
        Ok(())
    }
}
fn root(config: &CapabilityConfig) -> Result<PathBuf> {
    let root = config.artifacts.join(".document-operations");
    fs::create_dir_all(&root)?;
    ensure!(
        !fs::symlink_metadata(&root)?.file_type().is_symlink() && root.is_dir(),
        "Invalid document receipt directory"
    );
    Ok(root)
}
fn lock(path: &Path) -> Result<File> {
    if path.exists() {
        ensure!(
            !fs::symlink_metadata(path)?.file_type().is_symlink(),
            "Invalid document operation lease"
        );
    }
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}
fn try_lock(file: &File) -> Result<bool> {
    match fs2::FileExt::try_lock_exclusive(file) {
        Ok(()) => Ok(true),
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}
pub fn admission(config: &CapabilityConfig, cancel: &CancellationToken) -> Result<File> {
    let path = config.artifacts.join(".document-admission.lock");
    let lock = lock(&path)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        check(cancel)?;
        if try_lock(&lock)? {
            return Ok(lock);
        }
        ensure!(
            Instant::now() < deadline,
            "Another native document is still being processed; retry shortly"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn write(path: &Path, value: &Operation) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() < 32 * 1024, "Document receipt exceeds budget");
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("Missing receipt root")?)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn read(path: &Path) -> Result<Operation> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() < 32 * 1024,
        "Invalid document receipt"
    );
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn file_hash(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= 128 * 1024 * 1024,
        "Invalid saved document"
    );
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        ensure!(
            file.stream_position()? <= 128 * 1024 * 1024,
            "Saved document grew beyond budget"
        );
        hash.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
pub fn status(config: &CapabilityConfig, id: Uuid) -> Result<DocumentSaveReceipt> {
    ensure!(!id.is_nil(), "Choose a valid document operation ID");
    let root = root(config)?;
    let path = root.join(format!("{id}.json"));
    let lease = lock(&root.join(format!("{id}.lock")))?;
    let owned = try_lock(&lease)?;
    if !path.exists() {
        let receipt=DocumentSaveReceipt {
            operation_id:id,
            state:if owned {DocumentSaveState::Cancelled} else {DocumentSaveState::Prepared},
            result:None,
            message:if owned {"No copy was published under this operation ID. The ID is now cancelled so a delayed request cannot publish later."} else {"This copy request is starting. Check its status again before deciding its outcome."}.into(),
        };
        if owned {
            write(
                &path,
                &Operation {
                    digest: String::new(),
                    receipt: receipt.clone(),
                },
            )?;
        }
        return Ok(receipt);
    }
    let mut operation = read(&path)?;
    if let Some(result) = &operation.receipt.result {
        ensure!(
            result.path.parent() == Some(config.artifacts.as_path()),
            "Receipt output is outside managed artifacts"
        );
    }
    ensure!(
        operation.receipt.operation_id == id,
        "Invalid operation receipt identity"
    );
    if owned && operation.receipt.state == DocumentSaveState::Prepared {
        operation.receipt.state = if let Some(result) = &operation.receipt.result {
            if result.path.exists() {
                if file_hash(&result.path)? == result.revision {
                    DocumentSaveState::Committed
                } else {
                    DocumentSaveState::OutcomeUnknown
                }
            } else {
                DocumentSaveState::Cancelled
            }
        } else {
            DocumentSaveState::Cancelled
        };
        operation.receipt.message = if operation.receipt.state == DocumentSaveState::Committed {
            "Saved copy reconciled and verified."
        } else if operation.receipt.state == DocumentSaveState::Cancelled {
            "Operation stopped before publishing a copy."
        } else {
            "Output cannot be reconciled; retain this operation ID and inspect the artifact."
        }
        .into();
        write(&path, &operation)?;
    }
    if operation.receipt.state == DocumentSaveState::Committed {
        let result = operation
            .receipt
            .result
            .as_ref()
            .context("Missing committed copy")?;
        ensure!(
            file_hash(&result.path)? == result.revision,
            "Saved document changed after publication"
        );
    }
    Ok(operation.receipt)
}
fn copy_output(config: &CapabilityConfig, name: &str, extension: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && name.len() <= 180
            && name.trim() == name
            && !name.starts_with('.')
            && !name.ends_with('.')
            && name.chars().all(|c| !c.is_control()
                && !matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')),
        "Choose a document filename without folders, reserved characters or leading/trailing spaces"
    );
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_ascii_uppercase();
    ensure!(
        ![
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"
        ]
        .contains(&stem.as_str()),
        "Choose a filename that is not reserved by the operating system"
    );
    let suffix = format!(".{extension}");
    let filename = if name
        .to_ascii_lowercase()
        .ends_with(&suffix.to_ascii_lowercase())
    {
        name.to_owned()
    } else {
        ensure!(
            Path::new(name).extension().is_none(),
            "Use the same .{extension} file type as the original document"
        );
        format!("{name}{suffix}")
    };
    let output = config.artifacts.join(filename);
    ensure!(
        !output.exists(),
        "A file with that name already exists. Choose another name."
    );
    Ok(output)
}

pub fn save(
    config: &CapabilityConfig,
    request: DocumentSaveRequest,
    cancel: &CancellationToken,
) -> Result<DocumentSaveReceipt> {
    ensure!(
        !request.operation_id.is_nil()
            && request.source.revision.len() == 64
            && request
                .source
                .revision
                .bytes()
                .all(|v| v.is_ascii_hexdigit()),
        "Choose a valid operation ID and source revision"
    );
    let digest = editing::digest(&serde_json::to_vec(&request)?);
    let root = root(config)?;
    let receipt_path = root.join(format!("{}.json", request.operation_id));
    let lease = lock(&root.join(format!("{}.lock", request.operation_id)))?;
    let owned = try_lock(&lease)?;
    if receipt_path.exists() {
        let mut previous = read(&receipt_path)?;
        if previous.digest.is_empty() && previous.receipt.state == DocumentSaveState::Cancelled {
            if owned {
                previous.digest = digest;
                write(&receipt_path, &previous)?;
            }
            return Ok(previous.receipt);
        }
        ensure!(
            previous.digest == digest,
            "Operation ID was reused with different source/edits/name"
        );
        drop(lease);
        return status(config, request.operation_id);
    }
    ensure!(owned, "Document operation is already starting");
    let extension = match request.source.kind {
        DocumentKind::Docx => "docx",
        DocumentKind::Spreadsheet => "xlsx",
        DocumentKind::Text => request
            .source
            .path
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or("txt"),
    };
    let output = copy_output(config, &request.name, extension)?;
    let mut operation = Operation {
        digest,
        receipt: DocumentSaveReceipt {
            operation_id: request.operation_id,
            state: DocumentSaveState::Prepared,
            result: None,
            message: "Preparing a revision-checked document copy.".into(),
        },
    };
    write(&receipt_path, &operation)?;
    let result = (|| -> Result<()> {
        let _admission = admission(config, cancel)?;
        let mut staging = Staging::new(&config.artifacts)?;
        editing::build(config, &request, &staging.path, cancel)?;
        let hash = file_hash(&staging.path)?;
        operation.receipt.result = Some(DocumentSaveResult {
            path: output.clone(),
            source: request.source.clone(),
            revision: hash,
            applied_edits: request.edits.len() as u32,
        });
        write(&receipt_path, &operation)?;
        editing::verify(config, &request.source, cancel)?;
        staging.commit(&output, cancel)?;
        operation.receipt.state = DocumentSaveState::Committed;
        operation.receipt.message="Saved a verified copy. The original and unrelated Office parts are preserved; formulas were not evaluated.".into();
        Ok(())
    })();
    if let Err(error) = result {
        if output.exists() {
            operation.receipt.state = if operation
                .receipt
                .result
                .as_ref()
                .is_some_and(|r| file_hash(&output).is_ok_and(|hash| hash == r.revision))
            {
                DocumentSaveState::Committed
            } else {
                DocumentSaveState::OutcomeUnknown
            };
        } else {
            operation.receipt.state = if cancel.is_cancelled() {
                DocumentSaveState::Cancelled
            } else {
                DocumentSaveState::Failed
            };
        }
        operation.receipt.message = error.to_string();
    }
    write(&receipt_path, &operation)?;
    Ok(operation.receipt)
}
