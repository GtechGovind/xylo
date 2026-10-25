//! Native WinRT toasts for the live desktop process. No COM activation server
//! or cold-start routing is claimed; an installed app identity is required.
use super::{actor, *};
use std::collections::VecDeque;
use windows::{
    Data::Xml::Dom::XmlDocument,
    Foundation::TypedEventHandler,
    UI::Notifications::{
        NotificationSetting, ToastNotification, ToastNotificationManager, ToastNotifier,
    },
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
    core::{IInspectable, h},
};

pub(super) struct Backend {
    identity: AppIdentity,
    activations: async_channel::Sender<String>,
    notifier: Option<ToastNotifier>,
    toasts: VecDeque<(String, ToastNotification)>,
    com: bool,
}
impl Backend {
    pub fn new(identity: AppIdentity, activations: async_channel::Sender<String>) -> Self {
        Self {
            identity,
            activations,
            notifier: None,
            toasts: VecDeque::new(),
            com: false,
        }
    }
    fn notifier(&mut self) -> Result<ToastNotifier> {
        if let Some(notifier) = &self.notifier {
            return Ok(notifier.clone());
        }
        // SAFETY: this adapter owns a fresh native thread and balances a
        // successful WinRT initialization on that same thread in Drop.
        if !self.com {
            unsafe { RoInitialize(RO_INIT_MULTITHREADED) }?;
            self.com = true;
        }
        let key = windows_registry::CURRENT_USER.create(format!(
            r"Software\Classes\AppUserModelId\{}",
            self.identity.id
        ))?;
        key.set_string("DisplayName", &self.identity.name)?;
        let notifier =
            ToastNotificationManager::CreateToastNotifierWithId(&self.identity.id.clone().into())?;
        self.notifier = Some(notifier.clone());
        Ok(notifier)
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        if self.com {
            unsafe { RoUninitialize() };
        }
    }
}
impl actor::Backend for Backend {
    fn permission(&mut self, cancel: &CancellationToken) -> Result<Permission> {
        ensure!(!cancel.is_cancelled(), "Notification action cancelled");
        Ok(
            if self.notifier()?.Setting()? == NotificationSetting::Enabled {
                Permission::Allowed
            } else {
                Permission::Denied
            },
        )
    }
    fn request_permission(&mut self, cancel: &CancellationToken) -> Result<Permission> {
        self.permission(cancel)
    }
    fn post(&mut self, notice: NativeNotice, cancel: &CancellationToken) -> Result<()> {
        ensure!(!cancel.is_cancelled(), "Notification delivery cancelled");
        let notifier = self.notifier()?;
        ensure!(
            notifier.Setting()? == NotificationSetting::Enabled,
            "Notifications are blocked in system settings"
        );
        let xml = XmlDocument::new()?;
        let toast_element = xml.CreateElement(h!("toast"))?;
        xml.AppendChild(&toast_element)?;
        let visual = xml.CreateElement(h!("visual"))?;
        toast_element.AppendChild(&visual)?;
        let binding = xml.CreateElement(h!("binding"))?;
        binding.SetAttribute(h!("template"), h!("ToastGeneric"))?;
        visual.AppendChild(&binding)?;
        for value in [notice.title, notice.body] {
            let text = xml.CreateElement(h!("text"))?;
            text.SetInnerText(&value.into())?;
            binding.AppendChild(&text)?;
        }
        let audio = xml.CreateElement(h!("audio"))?;
        audio.SetAttribute(h!("silent"), h!("true"))?;
        toast_element.AppendChild(&audio)?;
        let toast = ToastNotification::CreateToastNotification(&xml)?;
        toast.SetTag(&notice.tag.clone().into())?;
        toast.SetGroup(h!("agent-workbench"))?;
        let sender = self.activations.clone();
        let tag = notice.tag.clone();
        toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
            move |_, _| {
                let _ = sender.try_send(tag.clone());
                Ok(())
            },
        ))?;
        if let Some(position) = self.toasts.iter().position(|(tag, _)| tag == &notice.tag) {
            if let Some((_, old)) = self.toasts.remove(position) {
                let _ = notifier.Hide(&old);
            }
        }
        if self.toasts.len() >= 64 {
            if let Some((_, old)) = self.toasts.pop_front() {
                let _ = notifier.Hide(&old);
            }
        }
        notifier.Show(&toast)?;
        self.toasts.push_back((notice.tag, toast));
        Ok(())
    }
    fn dismiss(&mut self, tag: &str) -> Result<()> {
        if let Some(index) = self.toasts.iter().position(|(known, _)| known == tag) {
            if let Some((_, toast)) = self.toasts.remove(index) {
                self.notifier()?.Hide(&toast)?;
            }
        }
        ToastNotificationManager::History()?.RemoveGroupedTagWithId(
            &tag.into(),
            h!("agent-workbench"),
            &self.identity.id.clone().into(),
        )?;
        Ok(())
    }
}
