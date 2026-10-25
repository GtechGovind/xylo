use anyhow::{Context, Result, ensure};
use std::sync::{Arc, Mutex};

type PlatformStore = Arc<keyring_core::CredentialStore>;
/// OS-backed credentials. Construction is lazy; only the first credential operation
/// opens the platform provider. There is no plaintext fallback or global default
/// store mutation. OS dialogs may block: call off the UI thread.
#[derive(Clone)]
pub struct CredentialVault {
    service: Arc<str>,
    store: Arc<Mutex<Option<PlatformStore>>>,
    serial: Arc<Mutex<()>>,
}
impl CredentialVault {
    pub fn new(service: impl Into<String>) -> Result<Self> {
        let service = service.into();
        ensure!(
            !service.trim().is_empty(),
            "Credential service name is required"
        );
        Ok(Self {
            service: service.into(),
            store: Arc::new(Mutex::new(None)),
            serial: Arc::new(Mutex::new(())),
        })
    }
    /// Injects an isolated keyring provider for offline protocol qualification.
    /// Normal production dependency edges do not enable this constructor.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn from_test_store(service: &str, store: Arc<keyring_core::CredentialStore>) -> Self {
        Self {
            service: service.into(),
            store: Arc::new(Mutex::new(Some(store))),
            serial: Arc::new(Mutex::new(())),
        }
    }
    fn entry(&self, reference: &str) -> Result<keyring_core::Entry> {
        ensure!(
            !reference.trim().is_empty() && reference.len() <= 256,
            "Invalid credential reference"
        );
        let mut guard = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("Credential provider lock poisoned"))?;
        if guard.is_none() {
            #[cfg(target_os = "macos")]
            let store: Arc<keyring_core::CredentialStore> =
                apple_native_keyring_store::keychain::Store::new()?;
            #[cfg(target_os = "windows")]
            let store: Arc<keyring_core::CredentialStore> =
                windows_native_keyring_store::Store::new()?;
            #[cfg(target_os = "linux")]
            let store: Arc<keyring_core::CredentialStore> =
                zbus_secret_service_keyring_store::Store::new()?;
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            anyhow::bail!("No OS credential provider is available on this platform");
            *guard = Some(store);
        }
        guard
            .as_ref()
            .context("OS credential provider unavailable")?
            .build(&self.service, reference, None)
            .context("Open OS credential entry")
    }
    pub fn get(&self, reference: &str) -> Result<Option<String>> {
        let _guard = self
            .serial
            .lock()
            .map_err(|_| anyhow::anyhow!("Credential store lock poisoned"))?;
        match self.entry(reference)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("Read OS credential"),
        }
    }
    pub fn set(&self, reference: &str, secret: &str) -> Result<()> {
        ensure!(!secret.is_empty(), "Secret cannot be empty");
        let _guard = self
            .serial
            .lock()
            .map_err(|_| anyhow::anyhow!("Credential store lock poisoned"))?;
        self.entry(reference)?
            .set_password(secret)
            .context("Save OS credential")
    }
    pub fn delete(&self, reference: &str) -> Result<()> {
        let _guard = self
            .serial
            .lock()
            .map_err(|_| anyhow::anyhow!("Credential store lock poisoned"))?;
        match self.entry(reference)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).context("Delete OS credential"),
        }
    }
}
