# Desktop notifications

Notifications are an opt-in product preference. A fresh profile is disabled,
silent, and restricted to the background. Opening Workbench or its notification
settings never requests system permission or sends a test notification.

The **Notifications** page provides separate completion, approval, failure,
scheduled-task, and background-only switches. Enabling saves that preference;
**Allow notifications** is a separate deliberate authorization action on macOS.
Windows and Linux use their desktop notification settings instead of a simulated
permission grant. **Send test notification** is always a separate action. System
Focus and notification policies can suppress a banner even after submission.

## Native boundary

| Platform | Native implementation | Activation and limits |
| --- | --- | --- |
| macOS | Apple UserNotifications, a retained delegate and one bounded native actor | Default-action click routes to the existing application's task. An application bundle identity is required; the unbundled CLI reports unsupported before invoking `currentNotificationCenter`. No sound, badge, critical or time-sensitive authorization. |
| Windows | WinRT `ToastNotificationManager`, application identity registration and one bounded native actor | Live-process `Activated` event routes to a task. Silent toast XML. Retraction removes a tracked banner and its tag/group from history. No COM activation server or cold-start routing is implemented. |
| Linux | XDG `org.freedesktop.Notifications` over one asynchronous D-Bus connection | Routing is advertised only if the daemon reports `actions`. A default action produces a real `ActionInvoked` signal. No permission dialog exists in this API. A missing daemon is unavailable. |

Cold-start activation is reported false on every platform. Workbench does not
claim it can launch and restore a task from a notification after its process
exits. Live activation uses the real platform callback, validates a stored
opaque tag, consumes it once, and emits the session and turn IDs to the host.
The host looks up the durable conversation, activates the existing window, and
uses the application's draft-preserving conversation navigation.

Native framework objects stay off the rendering thread. The macOS/Windows actor
has a 16-command bound and a 64-activation bound; Linux uses one event listener
that stops when its adapter is dropped. There is no idle status polling. Native
queries and delivery have bounded waits. The main agent stream uses the host's
bounded best-effort notice queue and does not wait for the notification daemon.

## Delivery and privacy policy

`workbench-notifications::NoticeEvent` accepts only session, turn and event UUIDs,
an event enum, an original timestamp, and the host's foreground observation. It
has no prompt, file path, title, body, model output, or tool-argument field.
The service selects short, fixed text such as “Task complete” and “Open Workbench
to view the result.” OS notifications therefore never contain conversation
content. They can still disclose that Workbench has a task or approval.

Only trusted live worker events produce notices. History replay does not.
Permission, preference and test commands are native UI host actions, separate
from the agent's tool registry; tools cannot enable or authorize notifications.
`observe()` never requests authorization. A persisted opt-in permits a read-only
settings/delegate refresh at startup, with no authorization request or banner.

The first terminal event for a turn wins. Cancellation retracts prior approvals
and prevents a later completion from replacing that outcome. Replayed starts do
not reopen a completed turn; a genuinely new run needs a new turn ID. Stable
approval event IDs prevent repeated approval envelopes from producing repeated
notices. Events more than two minutes old or over five seconds in the future are
ignored. Original timestamps must be retained during any recovery.

Preferences live in a separate lazy `notifications.sqlite3` database, with WAL,
a two-second busy bound, and Unix mode 0600 for the database. A profile that has
never opted in creates no notification database or journal on ordinary events.
The journal stores identifiers and category/validity metadata, never freeform
content. It retains at most 1,024 notice rows and 4,096 turn rows, also pruned at
30 days. These are bounded dedupe semantics, not an unlimited exactly-once claim.
Native submission failures are not automatically retried after restart.

Settings writes finish in an owned task if the UI waiter disappears. Tightening
policy takes effect before persistence; enabling takes effect only after a
successful durable preference write. Preference saving does not wait for an
in-flight permission dialog. Late delivery rechecks policy and its policy
revision. Cancelling or dropping an in-flight delivery waiter triggers cleanup
and best-effort retraction, including late native acceptance. A cancelled Linux
Notify request collects its bounded reply so its returned ID can be closed.
OS errors, a nonresponsive daemon, process termination, and lost native receipts
can prevent guaranteed physical retraction; no “displayed” receipt is invented.

## Host/UI contract

The runtime reexports `workbench_notifications` as `workbench_runtime::notifications`.
`Service::new(data, identity)` creates no authorization dialog, banner or SQLite
database. `initialize()` loads preferences. `status()` is a bounded memory-only
snapshot with a monotonically increasing observation sequence. Native and disk
work happens through asynchronous methods or the native actor.

The host's native capabilities are:

| Name | Arguments | Result |
| --- | --- | --- |
| `notifications_status` | `{}` | `{snapshot}` |
| `notifications_preferences` | `{preferences: Preferences}` | `{snapshot}` |
| `notifications_refresh` | `{}` | `{snapshot}`; read-only permission refresh |
| `notifications_request_permission` | `{}` | `{snapshot}`; explicit Allow button only |
| `notifications_test` | `{}` | `{snapshot,outcome}`; explicit Test button only |

The service itself exposes typed `Preferences`, `Snapshot`, `Permission`,
`NoticeEvent`, `Outcome`, `Activation`, and explicit `UserAction` markers. These
markers document the trusted UI boundary; they are not a substitute for keeping
the host commands outside the agent tool registry. `Snapshot.capabilities`
separately reports permission-request, live/cold activation, and retraction
support. `Outcome::Submitted` means native acceptance, not visible delivery.

The native UI correlates request IDs and rejects older snapshot sequences.
It retains the previous controlled preference if enqueueing fails, consumes
late cancelled replies, disables unsupported permission actions, and never
turns a status result into permission or test notification commands.

## Verification

No actual permission dialog or desktop notification has been triggered during
automated checks. Service tests use a private fake adapter and real temporary
SQLite. Native interaction tests use a headless Workbench with no HostClient.

```sh
cargo test -p workbench-notifications
cargo clippy -p workbench-notifications --all-targets -- -D warnings
CARGO_TARGET_DIR="$PWD/target" cargo test \
  --manifest-path crates/desktop/validation/Cargo.toml --locked \
  --test notification-interactions
```

The focused service cases cover defaults/laziness, explicit authorization,
permission denial/unsupported states, fixed content, category/foreground policy,
original timestamps, concurrent duplicates, first-terminal semantics, restart
dedupe, cancellation and late acceptance, disable/re-enable races, activation
validation/consumption, persistence failure, bounded journal rows, and ID reuse after a daemon restart. Native
cases cover control accessibility, pending/disabled state, stale/cancelled
responses and truthful submission feedback. Build/test results do not establish
physical OS delivery acceptance.

Release qualification still needs explicit user-driven checks of the installed,
signed application on macOS, Windows and supported Linux desktops: authorization
denial and later grant, OS Focus behavior, each enabled category, background-only
behavior, cancellation/retraction, live click routing to older conversations,
and draft preservation. Linux daemons without `actions` must show the unavailable
routing capability accurately. The portable Linux adapter is also type-checked by macOS unit tests using
dev-only D-Bus dependencies, without connecting to a bus. Windows native
compilation and Windows/Linux OS acceptance require their respective runners; this macOS check alone does not
qualify those implementations.

## Primary references and attribution

- [Apple UNUserNotificationCenter](https://developer.apple.com/documentation/usernotifications/unusernotificationcenter) and [requestAuthorization](https://developer.apple.com/documentation/usernotifications/unusernotificationcenter/requestauthorization(options:completionhandler:)) define separate settings queries, authorization, notification submission and delegate delivery.
- [Windows ToastNotifier.Setting](https://learn.microsoft.com/en-us/uwp/api/windows.ui.notifications.toastnotifier.setting?view=winrt-26100) reports application/user/system blocking. [ToastNotificationHistory.Remove](https://learn.microsoft.com/en-us/uwp/api/windows.ui.notifications.toastnotificationhistory.remove?view=winrt-26100) defines tag/group/application removal from action center.
- [XDG Desktop Notifications specification 1.3](https://specifications.freedesktop.org/notification/latest-single/) defines `GetCapabilities`, `Notify`, `CloseNotification`, `ActionInvoked`, default action and suppress-sound hints.
- [GPUI system-notification source](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/platform/mac/system_notifications.rs), also inspected in pinned `gpui-pre-macos 0.3.9`, informed the Objective-C delegate structure. The new adapter changes authorization, lifetime, receipt, cancellation and data policy. Copyright Zed Industries, Inc. and GPUI contributors; Apache License 2.0. Preserve that attribution with the source and include the dependency's license text and notices in distribution.
