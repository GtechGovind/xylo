//! Relinking changes a durable proposal's source identity atomically. It never
//! rewrites a source, rebuilds Office parts or authorizes an external action.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
mod source_file;
use xylo_core::DocumentKind;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentDraftRelinkRequest {
    pub operation_id: Uuid,
    pub key: DocumentDraftKey,
    pub expected_revision: i64,
    pub source: DocumentSource,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentDraftRelinkReceipt {
    pub request: DocumentDraftRelinkRequest,
    pub key: DocumentDraftKey,
    pub revision: i64,
    pub previous_revision: i64,
    pub updated_at: i64,
}
fn receipt(conn: &Connection, id: Uuid) -> Result<Option<DocumentDraftRelinkReceipt>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT receipt FROM document_draft_relinks WHERE operation_id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|json| serde_json::from_str(&json).map_err(Into::into))
        .transpose()
}
fn validate_request(request: &DocumentDraftRelinkRequest) -> Result<()> {
    validate(&request.key, &None)?;
    path_text(&request.source.path)?;
    ensure!(
        !request.operation_id.is_nil()
            && request.expected_revision > 0
            && request.expected_revision < i64::MAX,
        "Invalid saved document location request"
    );
    ensure!(
        request.source.path != request.key.source,
        "Choose the moved original in a different location"
    );
    ensure!(
        request.source.revision.len() == 64
            && request
                .source
                .revision
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            && request.source.bytes <= 64 * 1024 * 1024,
        "Invalid original document revision"
    );
    Ok(())
}
/// Blocking, bounded verification before the DB transaction. Normal document
/// open/save repeats revision checks; a file changing after this read is never
/// silently accepted as the original during editing or copy publication.
pub fn verify_document_relink_source(
    request: &DocumentDraftRelinkRequest,
    artifacts: &Path,
    management_root: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    validate_request(request)?;
    ensure!(
        !cancel.is_cancelled(),
        "Original location request cancelled"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let path = request
        .source
        .path
        .canonicalize()
        .context("The selected original is not available")?;
    ensure!(
        !path.starts_with(management_root) && !request.key.source.starts_with(management_root),
        "Application management files are outside the document scope"
    );
    ensure!(
        path == request.source.path,
        "Choose the original's current canonical location"
    );
    let workspace = request.key.workspace.canonicalize()?;
    let artifacts = artifacts.canonicalize()?;
    ensure!(
        path.starts_with(&workspace) || path.starts_with(&artifacts),
        "Choose an original inside this project or the application's saved files"
    );
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let kind = match extension.as_str() {
        "docx" => DocumentKind::Docx,
        "xlsx" => DocumentKind::Spreadsheet,
        "txt" | "md" | "csv" | "json" | "html" | "xml" | "log" | "yaml" | "yml" | "rs" | "toml" => {
            DocumentKind::Text
        }
        _ => anyhow::bail!("Choose the same document format as the original"),
    };
    ensure!(
        kind == request.source.kind,
        "Choose the same document format as the original"
    );
    let mut file = source_file::open(&path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() == request.source.bytes,
        "The selected original differs from the saved version"
    );
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut bytes = 0u64;
    loop {
        ensure!(
            !cancel.is_cancelled(),
            "Original location request cancelled"
        );
        ensure!(
            Instant::now() <= deadline,
            "Original verification timed out; your saved edits remain unchanged"
        );
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        ensure!(bytes <= 64 * 1024 * 1024, "Original exceeds its size limit");
        hash.update(&buffer[..n]);
    }
    ensure!(
        bytes == request.source.bytes && hex::encode(hash.finalize()) == request.source.revision,
        "The selected original differs from the saved version"
    );
    Ok(())
}
impl Store {
    pub fn document_draft_relink_status(
        &self,
        key: DocumentDraftKey,
        operation_id: Uuid,
    ) -> Result<Option<DocumentDraftRelinkReceipt>> {
        validate(&key, &None)?;
        ensure!(
            !operation_id.is_nil(),
            "Invalid saved document location request"
        );
        self.call(move |conn| {
            let result = receipt(conn, operation_id)?;
            ensure!(
                result.as_ref().is_none_or(|r| r.request.key == key),
                "This location request belongs to another document"
            );
            Ok(result)
        })
    }
    pub fn relink_document_draft(
        &self,
        request: DocumentDraftRelinkRequest,
    ) -> Result<DocumentDraftRelinkReceipt> {
        self.relink_document_draft_cancellable(request, &CancellationToken::new())
    }
    pub fn relink_document_draft_cancellable(
        &self,
        request: DocumentDraftRelinkRequest,
        cancel: &CancellationToken,
    ) -> Result<DocumentDraftRelinkReceipt> {
        validate_request(&request)?;
        let cancel = cancel.clone();
        self.call(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // A durable receipt always wins, even after a cancellation or a
            // subsequent destination edit. Replay never touches either row.
            if let Some(saved) = receipt(&tx, request.operation_id)? {
                ensure!(saved.request == request, "This location request was reused for different content");
                tx.commit()?;
                return Ok(saved);
            }
            ensure!(!cancel.is_cancelled(), "Original location request cancelled");
            let (old, mut state) = original_proposal(&tx, &request)?;
            let key = DocumentDraftKey {
                workspace: request.key.workspace.clone(),
                source: request.source.path.clone(),
            };
            let destination = read(&tx, &key)?;
            ensure!(destination.as_ref().is_none_or(|row| row.0.state.is_none()),
                "This original already has saved edits. Both sets of edits are preserved; open that document separately");
            state.source = request.source.clone();
            state.pending_relink = None;
            let json = validate(&key, &Some(state))?;
            check_capacity(&tx, &old, destination.as_ref(), json.len())?;
            let previous_revision = old.0.revision.checked_add(1).context("Document revision limit reached")?;
            let expected = destination.as_ref().map_or(0, |row| row.0.revision);
            let revision = expected.checked_add(1).context("Document revision limit reached")?;
            let updated_at = now_ms().max(old.0.updated_at.saturating_add(1));
            let saved = DocumentDraftRelinkReceipt {request: request.clone(), key, revision, previous_revision, updated_at};
            ensure!(!cancel.is_cancelled(), "Original location request cancelled");
            write_move(&tx, &saved, expected, &json)?;
            tx.commit()?;
            Ok(saved)
        })
    }
}
fn original_proposal(
    conn: &Connection,
    request: &DocumentDraftRelinkRequest,
) -> Result<(Stored, DocumentDraftState)> {
    let old =
        read(conn, &request.key)?.context("The saved document edits are no longer available")?;
    ensure!(
        old.0.revision == request.expected_revision,
        "Saved document edits changed; review the current saved version before locating the original"
    );
    let state = old
        .0
        .state
        .clone()
        .context("The saved document edits were cleared")?;
    ensure!(
        state.pending_copy.is_none(),
        "Check the earlier saved copy before locating the original"
    );
    ensure!(
        state.pending_relink.as_deref() == Some(request),
        "The location request is not the exact saved request"
    );
    ensure!(
        state.source.kind == request.source.kind
            && state.source.revision == request.source.revision
            && state.source.bytes == request.source.bytes,
        "The selected original differs from the saved version"
    );
    Ok((old, state))
}
fn check_capacity(
    conn: &Connection,
    old: &Stored,
    destination: Option<&Stored>,
    new_bytes: usize,
) -> Result<()> {
    let (bytes, rows): (i64, i64) = conn.query_row(
        "SELECT bytes,rows FROM document_draft_usage WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        destination.is_some() || rows < MAX_ROWS,
        "Saved documents have reached their limit; the original edits remain saved"
    );
    let total = bytes
        .saturating_sub(old.3 as i64)
        .saturating_sub(destination.map_or(0, |row| row.3 as i64))
        .saturating_add(4)
        .saturating_add(new_bytes as i64);
    ensure!(
        total <= MAX_STORAGE_BYTES,
        "Saved documents have reached their size limit; the original edits remain saved"
    );
    let (count, receipt_bytes): (i64, i64) = conn.query_row(
        "SELECT count(*),coalesce(sum(length(CAST(request AS BLOB))+length(CAST(receipt AS BLOB))),0) FROM document_draft_relinks",
        [], |row| Ok((row.get(0)?, row.get(1)?)))?;
    ensure!(
        count < 4096 && receipt_bytes < 16 * 1024 * 1024 - 128 * 1024,
        "Saved location records have reached their limit; the original edits remain saved"
    );
    Ok(())
}
fn write_move(
    conn: &Connection,
    saved: &DocumentDraftRelinkReceipt,
    destination_revision: i64,
    json: &str,
) -> Result<()> {
    let request = &saved.request;
    conn.execute(
        "UPDATE document_drafts SET json='null',revision=?1,updated_at=?2,mutation_id=?3,expected=?4
         WHERE workspace=?5 AND source=?6",
        params![saved.previous_revision,saved.updated_at,request.operation_id.to_string(),request.expected_revision,
            path_text(&request.key.workspace)?,path_text(&request.key.source)?])?;
    conn.execute(
        "INSERT INTO document_drafts(workspace,source,json,revision,updated_at,mutation_id,expected)
         VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(workspace,source) DO UPDATE SET
         json=excluded.json,revision=excluded.revision,updated_at=excluded.updated_at,
         mutation_id=excluded.mutation_id,expected=excluded.expected",
        params![path_text(&saved.key.workspace)?,path_text(&saved.key.source)?,json,saved.revision,
            saved.updated_at,request.operation_id.to_string(),destination_revision])?;
    conn.execute(
        "INSERT INTO document_draft_relinks(operation_id,request,receipt) VALUES(?1,?2,?3)",
        params![
            request.operation_id.to_string(),
            serde_json::to_string(request)?,
            serde_json::to_string(saved)?
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
