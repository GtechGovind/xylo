//! Crash-safe install intent. Highest accepted version advances after successful
//! install, or when the next running binary reconciles a pending install.
use super::*;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct State {
    pub highest_version: Option<Version>,
    pub highest_sequence: u64,
    pub highest_manifest: Option<String>,
    pub pending: Option<Pending>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pending {
    pub version: Version,
    pub sequence: u64,
    pub manifest: String,
}
impl State {
    pub fn accept_pending(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.highest_version = Some(pending.version);
            self.highest_sequence = pending.sequence;
            self.highest_manifest = Some(pending.manifest);
        }
    }
}
impl Updater {
    pub(super) fn load_state(&self) -> Result<State> {
        let path = self.data.join("installed.json");
        match File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(16385).read_to_end(&mut bytes)?;
                ensure!(bytes.len() <= 16384, "Invalid update journal");
                Ok(serde_json::from_slice(&bytes)
                    .context("Update journal is invalid; it must not be silently reset")?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(error.into()),
        }
    }
    pub(super) fn save_state(&self, state: &State) -> Result<()> {
        let mut temporary = NamedTempFile::new_in(&self.data)?;
        temporary.write_all(&serde_json::to_vec(state)?)?;
        temporary.as_file_mut().sync_all()?;
        temporary.persist(self.data.join("installed.json"))?;
        #[cfg(unix)]
        File::open(&self.data)?.sync_all()?;
        Ok(())
    }
    pub(super) fn reconcile(&self) -> Result<()> {
        let mut state = self.load_state()?;
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| self.current >= pending.version)
        {
            state.accept_pending();
            self.save_state(&state)?;
        }
        Ok(())
    }
}
