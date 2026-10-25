//! Hash-verified terminal result archives preserve operation-ID deduplication.
//! Pending and uncertain writes are never moved or discarded.
use super::*;
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
};
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_RECENT: usize = 128;
#[derive(Clone, Serialize, Deserialize)]
struct Index {
    digest: String,
    raw_sha256: String,
    compressed_sha256: String,
    receipt: EnvironmentReceipt,
}
#[derive(Default, Serialize, Deserialize)]
struct Usage {
    bytes: u64,
    receipts: u64,
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn directory(manager: &Manager, id: Uuid) -> PathBuf {
    manager
        .root
        .join("history")
        .join(&id.simple().to_string()[..2])
}
fn read_index(manager: &Manager, id: Uuid) -> Result<Option<Index>> {
    let path = directory(manager, id).join(format!("{id}.json"));
    if !path.exists() {
        return Ok(None);
    }
    let index: Index = storage::read_json(&path, 8 * 1024)?;
    ensure!(
        index.receipt.operation_id == id,
        "Invalid archived environment operation identity"
    );
    Ok(Some(index))
}
pub(super) fn cancelled(manager: &Manager, id: Uuid) -> Result<Option<EnvironmentReceipt>> {
    let path = directory(manager, id).join(format!("{id}.cancelled.json"));
    if !path.exists() {
        return Ok(None);
    }
    let receipt: EnvironmentReceipt = storage::read_json(&path, 8 * 1024)?;
    ensure!(
        receipt.operation_id == id
            && receipt.state == EnvironmentOperationState::Cancelled
            && receipt.environment.is_none()
            && receipt.output.is_none(),
        "Invalid cancelled request record"
    );
    Ok(Some(receipt))
}
pub(super) fn cancel_undispatched(
    manager: &Manager,
    receipt: &EnvironmentReceipt,
) -> Result<EnvironmentReceipt> {
    if let Some(previous) = cancelled(manager, receipt.operation_id)? {
        ensure!(
            previous.environment_id == receipt.environment_id && previous.action == receipt.action,
            "This request identity belongs to another environment or action"
        );
        return Ok(previous);
    }
    ensure!(
        !receipt.environment_id.is_nil(),
        "Cancellation requires the original environment identity"
    );
    storage::private_directory(&manager.root.join("history"))?;
    let path = manager.root.join("history/usage.json");
    let mut usage: Usage = if path.exists() {
        storage::read_json(&path, 1024)?
    } else {
        Usage::default()
    };
    let bytes = serde_json::to_vec(receipt)?.len() as u64;
    ensure!(
        usage.bytes.saturating_add(bytes) <= MAX_ARCHIVE_BYTES,
        "Environment history storage is full; existing results are preserved"
    );
    usage.bytes = usage.bytes.saturating_add(bytes);
    usage.receipts = usage.receipts.saturating_add(1);
    storage::write_json(&path, &usage, 1024)?;
    let directory = directory(manager, receipt.operation_id);
    storage::private_directory(&directory)?;
    storage::write_json(
        &directory.join(format!("{}.cancelled.json", receipt.operation_id)),
        receipt,
        8 * 1024,
    )?;
    Ok(receipt.clone())
}
pub(super) fn operation(manager: &Manager, id: Uuid) -> Result<Option<Operation>> {
    let Some(index) = read_index(manager, id)? else {
        return Ok(None);
    };
    let path = directory(manager, id).join(format!("{id}.json.gz"));
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= storage::MAX_RECEIPT_BYTES + 1024,
        "Invalid archived environment receipt"
    );
    let compressed = fs::read(path)?;
    ensure!(
        hash(&compressed) == index.compressed_sha256,
        "Archived environment receipt hash mismatch"
    );
    let mut raw = Vec::new();
    GzDecoder::new(compressed.as_slice())
        .take(storage::MAX_RECEIPT_BYTES + 1)
        .read_to_end(&mut raw)?;
    ensure!(
        raw.len() as u64 <= storage::MAX_RECEIPT_BYTES && hash(&raw) == index.raw_sha256,
        "Archived environment receipt contents changed"
    );
    let operation: Operation = serde_json::from_slice(&raw)?;
    ensure!(
        operation.digest == index.digest
            && operation.receipt.operation_id == id
            && operation.profile.id == index.receipt.environment_id,
        "Archived receipt belongs to another operation"
    );
    Ok(Some(operation))
}
pub(super) fn recent(manager: &Manager, id: Uuid) -> Result<Vec<EnvironmentReceipt>> {
    let path = manager.root.join("history/recent.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let entries: Vec<Index> = storage::read_json(&path, 1024 * 1024)?;
    ensure!(
        entries.len() <= MAX_RECENT,
        "Archived receipt summary index exceeds its bound"
    );
    Ok(entries
        .into_iter()
        .filter(|entry| entry.receipt.environment_id == id)
        .map(|entry| entry.receipt)
        .collect())
}
/// Caller holds the metadata lease. No native process is launched.
pub(super) fn maintain(
    manager: &Manager,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Value> {
    storage::private_directory(&manager.root.join("history"))?;
    let usage_path = manager.root.join("history/usage.json");
    let mut usage: Usage = if usage_path.exists() {
        storage::read_json(&usage_path, 1024)?
    } else {
        Usage::default()
    };
    let recent_path = manager.root.join("history/recent.json");
    let mut recent: Vec<Index> = if recent_path.exists() {
        storage::read_json(&recent_path, 1024 * 1024)?
    } else {
        Vec::new()
    };
    ensure!(
        recent.len() <= MAX_RECENT,
        "Archived receipt summary index exceeds its bound"
    );
    let mut archived = 0;
    let mut candidates = manager.all_operations()?;
    candidates.sort_by_key(|operation| (operation.created_at, operation.receipt.operation_id));
    for operation in candidates {
        if archived >= limit {
            break;
        }
        ensure!(
            !cancel.is_cancelled(),
            "Environment receipt maintenance cancelled; existing data remains retained"
        );
        if !matches!(
            operation.receipt.state,
            EnvironmentOperationState::Succeeded
                | EnvironmentOperationState::Failed
                | EnvironmentOperationState::Cancelled
        ) {
            continue;
        }
        let id = operation.receipt.operation_id;
        let raw = serde_json::to_vec(&operation)?;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&raw)?;
        let compressed = encoder.finish()?;
        let mut summary = operation.receipt.clone();
        if summary.created_at_ms.is_none() && operation.created_at > 0 {
            summary.created_at_ms = Some(operation.created_at);
        }
        summary.environment = None;
        summary.output = None;
        summary.message = summary.message.chars().take(256).collect();
        let index = Index {
            digest: operation.digest,
            raw_sha256: hash(&raw),
            compressed_sha256: hash(&compressed),
            receipt: summary,
        };
        let directory = directory(manager, id);
        storage::private_directory(&directory)?;
        if let Some(existing) = read_index(manager, id)? {
            ensure!(
                existing.digest == index.digest && existing.raw_sha256 == index.raw_sha256,
                "Archived environment operation was reused for different content"
            );
            self::operation(manager, id)?.context("Archived receipt is missing")?;
        } else {
            // Reserve before publication: a crash may overcount the reservation,
            // but never allows unaccounted archive growth or drops the original.
            let bytes = compressed.len() as u64 + serde_json::to_vec(&index)?.len() as u64;
            ensure!(
                usage.bytes.saturating_add(bytes) <= MAX_ARCHIVE_BYTES,
                "Native environment archive reached 1 GiB; export/review retained history before further operations"
            );
            usage.bytes = usage.bytes.saturating_add(bytes);
            usage.receipts = usage.receipts.saturating_add(1);
            storage::write_json(&usage_path, &usage, 1024)?;
            let mut file = tempfile::NamedTempFile::new_in(&directory)?;
            file.write_all(&compressed)?;
            file.as_file().sync_all()?;
            file.persist(directory.join(format!("{id}.json.gz")))
                .map_err(|error| error.error)?;
            storage::write_json(&directory.join(format!("{id}.json")), &index, 8 * 1024)?;
        }
        recent.retain(|entry| entry.receipt.operation_id != id);
        recent.push(index);
        if recent.len() > MAX_RECENT {
            recent.remove(0);
        }
        storage::write_json(&recent_path, &recent, 1024 * 1024)?;
        fs::remove_file(manager.root.join("operations").join(format!("{id}.json")))?;
        #[cfg(unix)]
        fs::File::open(manager.root.join("operations"))?.sync_all()?;
        archived += 1;
    }
    Ok(
        json!({"archived":archived,"archive_receipts":usage.receipts,"archive_reserved_bytes":usage.bytes,
        "archive_limit_bytes":MAX_ARCHIVE_BYTES,"active_receipts":manager.all_operations()?.len(),
        "message":"Completed results remain available by original operation ID; uncertain operations were retained."}),
    )
}
