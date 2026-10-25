//! Native boundary. Constructors never ask permission or post an OS banner.
use crate::*;
#[cfg(any(target_os = "linux", test))]
#[cfg_attr(all(test, not(target_os = "linux")), allow(dead_code))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

pub(crate) fn native(identity: AppIdentity) -> Result<(Arc<dyn Adapter>, Receiver<String>)> {
    #[cfg(target_os = "macos")]
    {
        actor::start(identity, macos::Backend::new)
    }
    #[cfg(target_os = "windows")]
    {
        actor::start(identity, windows::Backend::new)
    }
    #[cfg(target_os = "linux")]
    {
        Ok(linux::Native::create(identity))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = identity;
        anyhow::bail!("Native notifications are unavailable on this platform")
    }
}
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod actor;
