//! Apple UserNotifications. Delegate structure adapted from GPUI's Apache-2.0
//! implementation; unlike GPUI posting, authorization is always explicit here.
//! Copyright Zed Industries / GPUI contributors; see docs/notifications.md.
use super::{actor, *};
use block2::RcBlock;
use objc2::{
    AnyThread, DefinedClass, define_class, msg_send,
    rc::Retained,
    runtime::{Bool, ProtocolObject},
};
use objc2_foundation::{NSArray, NSBundle, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification,
    UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNNotificationSettings,
    UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use std::{ptr::NonNull, sync::mpsc::sync_channel};

pub(super) struct Backend {
    center: Option<Center>,
    activations: async_channel::Sender<String>,
}
struct Center {
    center: Retained<UNUserNotificationCenter>,
    _delegate: Retained<Delegate>,
}
impl Backend {
    pub fn new(_: AppIdentity, activations: async_channel::Sender<String>) -> Self {
        Self {
            center: None,
            activations,
        }
    }
    fn center(&mut self) -> Option<&UNUserNotificationCenter> {
        if self.center.is_none() {
            // Apple's API raises an Objective-C exception in an unbundled CLI.
            // Checking identity is read-only and never asks for authorization.
            NSBundle::mainBundle().bundleIdentifier()?;
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let delegate = Delegate::new(self.activations.clone());
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            self.center = Some(Center {
                center,
                _delegate: delegate,
            });
        }
        self.center.as_ref().map(|value| value.center.as_ref())
    }
}
impl actor::Backend for Backend {
    fn permission(&mut self, cancel: &CancellationToken) -> Result<Permission> {
        ensure!(!cancel.is_cancelled(), "Notification action cancelled");
        let Some(center) = self.center() else {
            return Ok(Permission::Unsupported);
        };
        let (reply, incoming) = sync_channel(1);
        let callback = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
            // SAFETY: Apple provides a valid settings object during this block.
            let status = unsafe { settings.as_ref() }.authorizationStatus();
            let permission = match status {
                UNAuthorizationStatus::Authorized => Permission::Allowed,
                UNAuthorizationStatus::Provisional | UNAuthorizationStatus::Ephemeral => {
                    Permission::Quiet
                }
                UNAuthorizationStatus::Denied => Permission::Denied,
                UNAuthorizationStatus::NotDetermined => Permission::NotDetermined,
                _ => Permission::Unavailable,
            };
            let _ = reply.try_send(permission);
        });
        center.getNotificationSettingsWithCompletionHandler(&callback);
        actor::wait(incoming, cancel, Duration::from_secs(5))
    }
    fn request_permission(&mut self, cancel: &CancellationToken) -> Result<Permission> {
        ensure!(
            !cancel.is_cancelled(),
            "Notification permission request cancelled"
        );
        let permission = self.permission(cancel)?;
        if permission != Permission::NotDetermined {
            return Ok(permission);
        }
        let Some(center) = self.center() else {
            return Ok(Permission::Unsupported);
        };
        let (reply, incoming) = sync_channel(1);
        let callback = RcBlock::new(move |allowed: Bool, error: *mut NSError| {
            let _ = reply.try_send((allowed.as_bool(), !error.is_null()));
        });
        // No sound, badge, critical or time-sensitive authorization is requested.
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert,
            &callback,
        );
        let (allowed, failed) = actor::wait(incoming, cancel, Duration::from_secs(120))?;
        ensure!(!failed, "Notification permission request could not finish");
        Ok(if allowed {
            Permission::Allowed
        } else {
            Permission::Denied
        })
    }
    fn post(&mut self, notice: NativeNotice, cancel: &CancellationToken) -> Result<()> {
        ensure!(!cancel.is_cancelled(), "Notification delivery cancelled");
        let Some(center) = self.center() else {
            anyhow::bail!("Install the application bundle to use native notifications");
        };
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(notice.title));
        content.setBody(&NSString::from_str(notice.body));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&notice.tag),
            &content,
            None,
        );
        let (reply, incoming) = sync_channel(1);
        let cancelled = cancel.clone();
        let tag = notice.tag;
        let callback = RcBlock::new(move |error: *mut NSError| {
            if cancelled.is_cancelled() {
                // Completion may arrive after cancellation stopped the waiting
                // actor. Retract on Apple's callback thread in that case too.
                let center = UNUserNotificationCenter::currentNotificationCenter();
                dismiss(&center, &tag);
            }
            let _ = reply.try_send(error.is_null());
        });
        center.addNotificationRequest_withCompletionHandler(&request, Some(&callback));
        ensure!(
            actor::wait(incoming, cancel, Duration::from_secs(5))?,
            "The system could not accept the notification"
        );
        Ok(())
    }
    fn dismiss(&mut self, tag: &str) -> Result<()> {
        if let Some(center) = self.center() {
            dismiss(center, tag);
        }
        Ok(())
    }
}
fn dismiss(center: &UNUserNotificationCenter, tag: &str) {
    let ids = NSArray::from_retained_slice(&[NSString::from_str(tag)]);
    center.removePendingNotificationRequestsWithIdentifiers(&ids);
    center.removeDeliveredNotificationsWithIdentifiers(&ids);
}
struct Ivars {
    activations: async_channel::Sender<String>,
}
define_class!(
    // SAFETY: NSObject has no additional subclass invariants; ivars contain a
    // thread-safe bounded sender and callbacks never access GPUI or SQL.
    #[unsafe(super(NSObject))]
    #[ivars=Ivars]
    struct Delegate;
    unsafe impl NSObjectProtocol for Delegate {}
    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn activated(
            &self,
            _: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &block2::DynBlock<dyn Fn()>,
        ) {
            if &*response.actionIdentifier() == unsafe { UNNotificationDefaultActionIdentifier } {
                let _ = self
                    .ivars()
                    .activations
                    .try_send(response.notification().request().identifier().to_string());
            }
            completion.call(());
        }
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn presenting(
            &self,
            _: &UNUserNotificationCenter,
            _: &UNNotification,
            completion: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            // Foreground banners are controlled by background_only policy;
            // opting in never requests sounds or overrides Focus modes.
            completion
                .call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
        }
    }
);
impl Delegate {
    fn new(activations: async_channel::Sender<String>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { activations });
        // SAFETY: NSObject's designated initializer initializes the allocated object.
        unsafe { msg_send![super(this), init] }
    }
}
