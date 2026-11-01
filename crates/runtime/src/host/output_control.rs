//! Bounded reads from app-owned command logs. The caller never supplies a path.
use super::*;
use std::io::{Read, Seek, SeekFrom};
const PAGE_BYTES: usize = 32 * 1024;
/// Complete clipboard reads fail explicitly above this raw log size.
const COPY_BYTES: usize = 32 * 1024 * 1024;
#[derive(Clone, Debug)]
pub struct CommandOutputPage {
    pub offset: u64,
    pub next: Option<u64>,
    pub text: String,
}
impl HostClient {
    /// Reads a bounded final page; the response carries its real byte offset.
    pub fn command_output_end(&self, id: Uuid, session: Uuid, turn: Uuid, job: Uuid) -> Result<()> {
        self.enqueue(HostCommand::CommandOutputEnd(id, session, turn, job))
    }
    /// Copies the complete retained log, or reports an explicit size/read error.
    pub fn command_output_copy(
        &self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        job: Uuid,
    ) -> Result<()> {
        self.enqueue(HostCommand::CommandOutputCopy(id, session, turn, job))
    }
    pub fn command_output(
        &self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        job: Uuid,
        offset: u64,
    ) -> Result<()> {
        self.enqueue(HostCommand::CommandOutput(id, session, turn, job, offset))
    }
}
impl HostState {
    pub(super) async fn command_output_end(
        &self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        job: Uuid,
    ) -> Result<()> {
        let artifacts = self.paths.artifacts.clone();
        let result = tokio::task::spawn_blocking(move || read_end(&artifacts, session, turn, job))
            .await
            .context("Read final command output page")?
            .map_err(|error| format!("{error:#}"));
        self.events
            .send(HostEvent::CommandOutput {
                request_id: id,
                session,
                turn,
                job,
                result,
            })
            .await?;
        Ok(())
    }
    pub(super) async fn command_output_copy(
        &self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        job: Uuid,
    ) -> Result<()> {
        let artifacts = self.paths.artifacts.clone();
        let result = tokio::task::spawn_blocking(move || read_copy(&artifacts, session, turn, job))
            .await
            .context("Read complete command output")?
            .map_err(|error| format!("{error:#}"));
        self.events
            .send(HostEvent::CommandOutputCopy {
                request_id: id,
                session,
                turn,
                job,
                result,
            })
            .await?;
        Ok(())
    }
    pub(super) async fn command_output(
        &self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        job: Uuid,
        offset: u64,
    ) -> Result<()> {
        let artifacts = self.paths.artifacts.clone();
        let result =
            tokio::task::spawn_blocking(move || read_page(&artifacts, session, turn, job, offset))
                .await
                .context("Read command output")?
                .map_err(|error| format!("{error:#}"));
        self.events
            .send(HostEvent::CommandOutput {
                request_id: id,
                session,
                turn,
                job,
                result,
            })
            .await?;
        Ok(())
    }
}
/// Resolve only UUID-owned retained files; no API accepts a caller path.
fn owned_log(
    artifacts: &std::path::Path,
    session: Uuid,
    turn: Uuid,
    job: Uuid,
) -> Result<std::fs::File> {
    let (directory, owner_turn) = job_identity(artifacts, session, job)?;
    ensure!(owner_turn == turn, "Command output belongs to another task");
    let path = directory.join(format!("{job}.log")).canonicalize()?;
    ensure!(
        path.parent() == Some(directory.as_path()),
        "Invalid command log"
    );
    Ok(std::fs::File::open(path)?)
}
pub(super) fn job_identity(
    artifacts: &std::path::Path,
    session: Uuid,
    job: Uuid,
) -> Result<(std::path::PathBuf, Uuid)> {
    let root = artifacts.join("shell-jobs").canonicalize()?;
    let directory = root.join(session.to_string()).canonicalize()?;
    ensure!(
        directory.parent() == Some(root.as_path()),
        "Command output is outside this chat"
    );
    let metadata = directory.join(format!("{job}.json"));
    ensure!(
        metadata.canonicalize()?.parent() == Some(directory.as_path()),
        "Invalid command metadata"
    );
    let mut identity = Vec::new();
    std::fs::File::open(metadata)?
        .take(4096)
        .read_to_end(&mut identity)?;
    let (owner, owner_turn): (Uuid, Uuid) = serde_json::from_slice(&identity)?;
    ensure!(owner == session, "Command output belongs to another task");
    Ok((directory, owner_turn))
}
fn read_copy(artifacts: &std::path::Path, session: Uuid, turn: Uuid, job: Uuid) -> Result<String> {
    let file = owned_log(artifacts, session, turn, job)?;
    ensure!(
        file.metadata()?.len() <= COPY_BYTES as u64,
        "Command output exceeds the 32 MiB copy limit; no output was copied"
    );
    // Read one extra byte to catch a concurrently growing log without ever
    // returning truncated text under the 'Copy output' label.
    let mut bytes = Vec::new();
    file.take(COPY_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= COPY_BYTES,
        "Command output exceeds the 32 MiB copy limit; no output was copied"
    );
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
fn read_end(
    artifacts: &std::path::Path,
    session: Uuid,
    turn: Uuid,
    job: Uuid,
) -> Result<CommandOutputPage> {
    let mut file = owned_log(artifacts, session, turn, job)?;
    let length = file.metadata()?.len();
    let mut offset = length.saturating_sub(PAGE_BYTES as u64);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::with_capacity(PAGE_BYTES);
    (&mut file).take(length - offset).read_to_end(&mut bytes)?;
    // A tail may begin inside a character. Drop only its leading continuation
    // bytes so a valid UTF-8 log never gains replacement characters at the cut.
    if offset > 0 {
        let skip = bytes
            .iter()
            .take(3)
            .take_while(|byte| **byte & 0xc0 == 0x80)
            .count();
        bytes.drain(..skip);
        offset += skip as u64;
    }
    Ok(CommandOutputPage {
        offset,
        next: None,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}
pub(super) fn read_page(
    artifacts: &std::path::Path,
    session: Uuid,
    turn: Uuid,
    job: Uuid,
    offset: u64,
) -> Result<CommandOutputPage> {
    let mut file = owned_log(artifacts, session, turn, job)?;
    let length = file.metadata()?.len();
    ensure!(offset <= length, "Output page is no longer available");
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::with_capacity(PAGE_BYTES);
    file.take(PAGE_BYTES as u64).read_to_end(&mut bytes)?;
    // Keep a valid UTF-8 character together across pages. Invalid process bytes
    // remain readable via replacement characters, without changing raw logs.
    if offset + (bytes.len() as u64) < length
        && let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
        && error.valid_up_to() > 0
    {
        bytes.truncate(error.valid_up_to());
    }
    let end = offset + bytes.len() as u64;
    Ok(CommandOutputPage {
        offset,
        next: (end < length).then_some(end),
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_are_bounded_and_owned_by_the_original_task() {
        let temp = tempfile::tempdir().unwrap();
        let (session, turn, job) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let directory = temp.path().join("shell-jobs").join(session.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("{job}.json")),
            serde_json::to_vec(&(session, turn)).unwrap(),
        )
        .unwrap();
        let log = format!("{}final error", "long line output\n".repeat(4000));
        std::fs::write(directory.join(format!("{job}.log")), &log).unwrap();
        let first = read_page(temp.path(), session, turn, job, 0).unwrap();
        assert!(first.text.len() <= PAGE_BYTES);
        assert!(first.next.is_some());
        assert!(read_page(temp.path(), session, Uuid::new_v4(), job, 0).is_err());
        assert!(read_page(temp.path(), Uuid::new_v4(), turn, job, 0).is_err());
        let mut output = first.text;
        let mut next = first.next;
        while let Some(offset) = next {
            let page = read_page(temp.path(), session, turn, job, offset).unwrap();
            output.push_str(&page.text);
            next = page.next;
        }
        assert_eq!(output, log);
    }
    #[test]
    fn utf8_pages_reassemble_and_invalid_offsets_fail() {
        let temp = tempfile::tempdir().unwrap();
        let (session, turn, job) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let directory = temp.path().join("shell-jobs").join(session.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        let metadata = directory.join(format!("{job}.json"));
        std::fs::write(&metadata, serde_json::to_vec(&(session, turn)).unwrap()).unwrap();
        let log = format!("{}🚀done", "a".repeat(PAGE_BYTES - 1));
        std::fs::write(directory.join(format!("{job}.log")), &log).unwrap();
        let first = read_page(temp.path(), session, turn, job, 0).unwrap();
        assert_eq!(first.text.len(), PAGE_BYTES - 1);
        let last = read_page(temp.path(), session, turn, job, first.next.unwrap()).unwrap();
        assert_eq!(format!("{}{}", first.text, last.text), log);
        assert!(last.next.is_none());
        assert!(read_page(temp.path(), session, turn, job, log.len() as u64 + 1).is_err());
        std::fs::write(metadata, "not an ownership record").unwrap();
        assert!(read_page(temp.path(), session, turn, job, 0).is_err());
    }
    #[test]
    fn final_page_and_complete_copy_preserve_owned_utf8_logs() {
        let temp = tempfile::tempdir().unwrap();
        let (session, turn, job) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let directory = temp.path().join("shell-jobs").join(session.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("{job}.json")),
            serde_json::to_vec(&(session, turn)).unwrap(),
        )
        .unwrap();
        let log = format!("{}🚀{}", "x".repeat(PAGE_BYTES), "y".repeat(PAGE_BYTES - 2));
        let path = directory.join(format!("{job}.log"));
        std::fs::write(&path, &log).unwrap();
        let end = read_end(temp.path(), session, turn, job).unwrap();
        assert_eq!(end.offset, PAGE_BYTES as u64 + 4);
        assert_eq!(end.text, "y".repeat(PAGE_BYTES - 2));
        assert!(end.next.is_none());
        assert_eq!(read_copy(temp.path(), session, turn, job).unwrap(), log);
        {
            let wrong_turn = Uuid::new_v4();
            assert!(read_end(temp.path(), session, wrong_turn, job).is_err());
            assert!(read_copy(temp.path(), session, wrong_turn, job).is_err());
        }
        assert!(read_end(temp.path(), Uuid::new_v4(), turn, job).is_err());
        assert!(read_copy(temp.path(), Uuid::new_v4(), turn, job).is_err());
        std::fs::write(&path, "short 🚀 log").unwrap();
        let end = read_end(temp.path(), session, turn, job).unwrap();
        assert_eq!(end.offset, 0);
        assert_eq!(end.text, "short 🚀 log");
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_copy(temp.path(), session, turn, job).unwrap(), "");
        assert_eq!(read_end(temp.path(), session, turn, job).unwrap().offset, 0);
    }
    #[test]
    fn complete_copy_refuses_oversize_logs_instead_of_returning_a_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let (session, turn, job) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let directory = temp.path().join("shell-jobs").join(session.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("{job}.json")),
            serde_json::to_vec(&(session, turn)).unwrap(),
        )
        .unwrap();
        let path = directory.join(format!("{job}.log"));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(COPY_BYTES as u64 + 1).unwrap();
        let error = read_copy(temp.path(), session, turn, job)
            .unwrap_err()
            .to_string();
        assert!(error.contains("32 MiB"));
        assert!(error.contains("no output was copied"));
        // Go to end stays bounded even when complete clipboard reads are denied.
        assert!(
            read_end(temp.path(), session, turn, job)
                .unwrap()
                .text
                .len()
                <= PAGE_BYTES
        );
    }
}
