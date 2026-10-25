//! Explicit saved SSH/container identities. Requests carry a stable operation
//! identity and revision; neither cancellation nor a saved flag means success.
use super::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
#[path = "environments_view.rs"]
mod view;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Ssh,
    Docker,
    Podman,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Ssh => "SSH",
            Self::Docker => "Docker",
            Self::Podman => "Podman",
        }
    }
    fn runtime(self) -> &'static str {
        match self {
            Self::Ssh => "ssh",
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Values {
    kind: Kind,
    managed: bool,
    name: String,
    host: String,
    user: String,
    port: String,
    target: String,
    image: String,
    startup: String,
    client: Option<PathBuf>,
    known_hosts: Option<PathBuf>,
    identity: Option<PathBuf>,
}
impl Default for Values {
    fn default() -> Self {
        Self {
            kind: Kind::Ssh,
            managed: false,
            name: String::new(),
            host: String::new(),
            user: String::new(),
            port: "22".into(),
            target: String::new(),
            image: String::new(),
            startup: String::new(),
            client: None,
            known_hosts: None,
            identity: None,
        }
    }
}
#[derive(Clone, Deserialize)]
struct Client {
    runtime: String,
    executable: PathBuf,
    available: bool,
}
pub(super) struct Form {
    pub name: Entity<InputState>,
    pub host: Entity<InputState>,
    pub user: Entity<InputState>,
    pub port: Entity<InputState>,
    pub target: Entity<InputState>,
    pub image: Entity<InputState>,
    pub startup: Entity<InputState>,
    kind: Kind,
    managed: bool,
    client: Option<PathBuf>,
    known_hosts: Option<PathBuf>,
    identity: Option<PathBuf>,
    id: Uuid,
    original: Option<EnvironmentRecord>,
    baseline: Values,
    last: Values,
    pub generation: u64,
    dirty: bool,
    conflict: bool,
    restoring: bool,
}
impl Form {
    fn new(window: &mut Window, cx: &mut Context<Workbench>) -> Self {
        let port = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value("22", window, cx);
            state
        });
        let mut input = |text: &str, cx: &mut Context<Workbench>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(text.to_owned()))
        };
        Self {
            name: input("Environment name", cx),
            host: input("Host name", cx),
            user: input("User name", cx),
            port,
            target: input("Container name or ID", cx),
            image: input("Image name", cx),
            startup: input("Optional startup command", cx),
            kind: Kind::Ssh,
            managed: false,
            client: None,
            known_hosts: None,
            identity: None,
            id: Uuid::new_v4(),
            original: None,
            baseline: Values::default(),
            last: Values::default(),
            generation: 0,
            dirty: false,
            conflict: false,
            restoring: false,
        }
    }
    fn values(&self, cx: &App) -> Values {
        Values {
            kind: self.kind,
            managed: self.managed,
            name: self.name.read(cx).value().to_string(),
            host: self.host.read(cx).value().to_string(),
            user: self.user.read(cx).value().to_string(),
            port: self.port.read(cx).value().to_string(),
            target: self.target.read(cx).value().to_string(),
            image: self.image.read(cx).value().to_string(),
            startup: self.startup.read(cx).value().to_string(),
            client: self.client.clone(),
            known_hosts: self.known_hosts.clone(),
            identity: self.identity.clone(),
        }
    }
}
#[derive(Clone)]
enum RequestKind {
    List,
    Clients,
    History(Uuid),
    Status(Uuid),
    Receipt {
        operation_id: Uuid,
        environment_id: Uuid,
        action: EnvironmentAction,
        submitted: Option<Box<Values>>,
    },
    Operation(Uuid),
    Resolve(Uuid),
}
struct Request {
    epoch: u64,
    kind: RequestKind,
}
#[derive(Clone, Copy)]
enum Asset {
    Client,
    KnownHosts,
    Identity,
}
pub(super) struct EnvironmentUi {
    records: Vec<EnvironmentRecord>,
    loaded: bool,
    selected: Option<Uuid>,
    statuses: BTreeMap<Uuid, EnvironmentStatus>,
    clients: Vec<Client>,
    clients_loaded: bool,
    pub form: Form,
    subscriptions: Vec<Subscription>,
    pub command: Entity<InputState>,
    output: Entity<TextareaState>,
    output_details: Option<EnvironmentOutput>,
    output_environment: String,
    history: Arc<Vec<EnvironmentReceipt>>,
    history_for: Option<Uuid>,
    history_loaded: bool,
    resolution_note: Entity<TextareaState>,
    resolving: Option<Uuid>,
    operations: BTreeMap<Uuid, EnvironmentReceipt>,
    checked_operations: BTreeSet<Uuid>,
    submissions: BTreeMap<Uuid, Values>,
    pending: BTreeMap<Uuid, Request>,
    retired: BTreeSet<Uuid>,
    epoch: u64,
    picking: bool,
    confirming: bool,
    pub notice: String,
}
impl EnvironmentUi {
    pub fn new(window: &mut Window, cx: &mut Context<Workbench>) -> Self {
        Self {
            records: Vec::new(),
            loaded: false,
            selected: None,
            statuses: BTreeMap::new(),
            clients: Vec::new(),
            clients_loaded: false,
            form: Form::new(window, cx),
            subscriptions: Vec::new(),
            command: cx.new(|cx| InputState::new(window, cx).placeholder("Command to run")),
            output: cx.new(|cx| TextareaState::new(window, cx).rows(7)),
            output_details: None,
            output_environment: String::new(),
            history: Arc::new(Vec::new()),
            history_for: None,
            history_loaded: false,
            resolution_note: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(2)
                    .placeholder("Describe the result you checked")
            }),
            resolving: None,
            operations: BTreeMap::new(),
            checked_operations: BTreeSet::new(),
            submissions: BTreeMap::new(),
            pending: BTreeMap::new(),
            retired: BTreeSet::new(),
            epoch: 0,
            picking: false,
            confirming: false,
            notice: String::new(),
        }
    }
    pub fn busy(&self) -> bool {
        !self.pending.is_empty() || self.confirming || self.picking
    }
    pub fn active(&self) -> bool {
        self.confirming
            || self.pending.values().any(|request| {
                !matches!(
                    request.kind,
                    RequestKind::List | RequestKind::Clients | RequestKind::History(_)
                )
            })
    }
    pub fn dirty(&self) -> bool {
        self.form.dirty
    }
    fn connected(&self, record: &EnvironmentRecord) -> bool {
        record.connected
            && self.statuses.get(&record.profile.id).is_some_and(|status| {
                status.environment.revision == record.revision
                    && status.state == EnvironmentState::Connected
            })
    }
    fn selected_record(&self) -> Option<&EnvironmentRecord> {
        self.selected
            .and_then(|id| self.records.iter().find(|record| record.profile.id == id))
    }
    fn unresolved(&self, id: Uuid) -> bool {
        self.operations
            .values()
            .any(|operation| operation.environment_id == id && !terminal(operation.state))
    }
}
impl Workbench {
    pub(super) fn subscribe_environments(&mut self, cx: &mut Context<Self>) {
        let inputs = [
            self.environments.form.name.clone(),
            self.environments.form.host.clone(),
            self.environments.form.user.clone(),
            self.environments.form.port.clone(),
            self.environments.form.target.clone(),
            self.environments.form.image.clone(),
            self.environments.form.startup.clone(),
        ];
        self.environments.subscriptions = inputs
            .iter()
            .map(|input| {
                cx.subscribe(input, |view, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        view.environment_form_changed(cx);
                    }
                })
            })
            .collect();
    }
    fn environment_form_changed(&mut self, cx: &mut Context<Self>) {
        if self.environments.form.restoring {
            return;
        }
        let current = self.environments.form.values(cx);
        if current == self.environments.form.last {
            return;
        }
        self.environments.form.last = current.clone();
        self.environments.form.dirty = current != self.environments.form.baseline;
        self.environments.form.generation = self.environments.form.generation.saturating_add(1);
        self.cancel_setup_close_intents(cx);
        cx.notify();
    }
    fn environment_request(
        &mut self,
        name: &str,
        args: Value,
        kind: RequestKind,
        cx: &mut Context<Self>,
    ) {
        if self.quitting || !self.environments.pending.is_empty() {
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, name.into(), args));
        if result.is_ok() {
            if let RequestKind::Receipt {
                operation_id,
                submitted: Some(values),
                ..
            } = &kind
            {
                self.environments
                    .submissions
                    .insert(*operation_id, (**values).clone());
            }
            self.environments.pending.insert(
                id,
                Request {
                    epoch: self.environments.epoch,
                    kind,
                },
            );
        } else {
            self.environments.notice =
                "This request could not start. Your setup stays here; try again.".into();
        }
        self.result(result, cx);
    }
    pub(super) fn refresh_environments(&mut self, cx: &mut Context<Self>) {
        self.environment_request("helper_environment_list", json!({}), RequestKind::List, cx);
    }
    pub(super) fn environment_owns_request(&self, id: Uuid) -> bool {
        self.environments.pending.contains_key(&id) || self.environments.retired.contains(&id)
    }
    pub(super) fn environment_scope_changed(&mut self, cx: &mut Context<Self>) {
        let ids = self
            .environments
            .pending
            .iter()
            .filter(|(_, request)| {
                matches!(
                    request.kind,
                    RequestKind::List | RequestKind::Clients | RequestKind::History(_)
                )
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(host) = &self.host {
                let _ = host.cancel_capability(id);
            }
            self.environments.pending.remove(&id);
            self.environments.retired.insert(id);
        }
        self.environments.epoch = self.environments.epoch.saturating_add(1);
        self.environments.statuses.clear();
        self.environments.history = Arc::new(Vec::new());
        self.environments.history_for = None;
        self.environments.history_loaded = false;
        self.environments.picking = false;
        while self.environments.retired.len() > 128 {
            self.environments.retired.pop_first();
        }
        cx.notify();
    }
    fn environment_profile(&self, cx: &App) -> Result<EnvironmentProfile, String> {
        let form = &self.environments.form;
        let values = form.values(cx);
        if values.name.trim().is_empty() {
            return Err("Give this environment a name.".into());
        }
        let executable = values
            .client
            .clone()
            .or_else(|| {
                self.environments
                    .clients
                    .iter()
                    .find(|client| client.runtime == values.kind.runtime() && client.available)
                    .map(|client| client.executable.clone())
            })
            .ok_or_else(|| {
                format!(
                    "Install {} or choose its client application.",
                    values.kind.label()
                )
            })?;
        let target = match values.kind {
            Kind::Ssh => {
                if values.host.trim().is_empty() || values.user.trim().is_empty() {
                    return Err("Enter the host and user name.".into());
                }
                let port = values
                    .port
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port > 0)
                    .ok_or("Enter a valid SSH port.")?;
                let known_hosts = values
                    .known_hosts
                    .ok_or("Choose your verified hosts file before saving SSH setup.")?;
                EnvironmentTarget::Ssh {
                    executable,
                    host: values.host.trim().into(),
                    user: values.user.trim().into(),
                    port,
                    known_hosts,
                    identity_file: values.identity,
                }
            }
            Kind::Docker | Kind::Podman => {
                let container = if values.managed {
                    if values.image.trim().is_empty() {
                        return Err("Enter the image to use for this container.".into());
                    }
                    let command = shell_words::split(values.startup.trim())
                        .map_err(|_| "Check the quotation marks in the startup command.")?;
                    ContainerTarget::Managed {
                        image: values.image.trim().into(),
                        command,
                    }
                } else {
                    if values.target.trim().is_empty() {
                        return Err("Enter the name of the existing container.".into());
                    }
                    ContainerTarget::Existing {
                        target: values.target.trim().into(),
                    }
                };
                EnvironmentTarget::Container {
                    executable,
                    runtime: if values.kind == Kind::Docker {
                        ContainerRuntime::Docker
                    } else {
                        ContainerRuntime::Podman
                    },
                    container,
                }
            }
        };
        Ok(EnvironmentProfile {
            id: form.id,
            name: values.name.trim().into(),
            target,
        })
    }
    fn environment_save(&mut self, cx: &mut Context<Self>) {
        if self.environments.busy()
            || self.environments.form.conflict
            || self.environments.unresolved(self.environments.form.id)
        {
            return;
        }
        let profile = match self.environment_profile(cx) {
            Ok(profile) => profile,
            Err(message) => {
                self.environments.notice = message;
                cx.notify();
                return;
            }
        };
        let operation_id = Uuid::new_v4();
        let expected_revision = self
            .environments
            .form
            .original
            .as_ref()
            .map_or(0, |record| record.revision);
        let environment_id = profile.id;
        let args = json!({"operation_id":operation_id,"expected_revision":expected_revision,"profile":profile});
        let submitted = self.environments.form.values(cx);
        self.environments.notice = "Saving environment setup…".into();
        self.environment_request(
            "helper_environment_save",
            args,
            RequestKind::Receipt {
                operation_id,
                environment_id,
                action: EnvironmentAction::Save,
                submitted: Some(Box::new(submitted)),
            },
            cx,
        );
    }
    fn environment_select(
        &mut self,
        id: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.environments.busy() {
            return;
        }
        if self.environments.form.dirty {
            let epoch = self.environments.epoch;
            let generation = self.environments.form.generation;
            self.environments.confirming = true;
            let answer = window.prompt(
                PromptLevel::Warning,
                "Discard unsaved environment changes?",
                Some("The saved profile and its connection will stay unchanged."),
                &[
                    PromptButton::cancel("Keep editing"),
                    PromptButton::new("Discard changes"),
                ],
                cx,
            );
            cx.spawn_in(window, async move |view, cx| {
                let answer = answer.await;
                let _ = view.update_in(cx, |view, window, cx| {
                    view.environments.confirming = false;
                    if matches!(answer, Ok(1))
                        && epoch == view.environments.epoch
                        && generation == view.environments.form.generation
                    {
                        view.environment_apply_selection(id, window, cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        } else {
            self.environment_apply_selection(id, window, cx);
            cx.notify();
        }
    }
    fn environment_apply_selection(
        &mut self,
        id: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let record = id
            .and_then(|id| {
                self.environments
                    .records
                    .iter()
                    .find(|record| record.profile.id == id)
            })
            .cloned();
        self.environments.selected = record.as_ref().map(|record| record.profile.id);
        self.environments.history = Arc::new(Vec::new());
        self.environments.history_for = None;
        self.environments.history_loaded = false;
        let values = record
            .as_ref()
            .map(|record| values_from_profile(&record.profile))
            .unwrap_or_default();
        self.environment_apply_values(values, window, cx);
        self.environments.form.id = record
            .as_ref()
            .map_or_else(Uuid::new_v4, |record| record.profile.id);
        self.environments.form.original = record;
        self.environments.form.conflict = false;
    }
    fn environment_apply_values(
        &mut self,
        values: Values,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = &mut self.environments.form;
        form.restoring = true;
        for (input, value) in [
            (&form.name, &values.name),
            (&form.host, &values.host),
            (&form.user, &values.user),
            (&form.port, &values.port),
            (&form.target, &values.target),
            (&form.image, &values.image),
            (&form.startup, &values.startup),
        ] {
            Self::set_input(input, value.clone(), window, cx);
        }
        form.kind = values.kind;
        form.managed = values.managed;
        form.client = values.client.clone();
        form.known_hosts = values.known_hosts.clone();
        form.identity = values.identity.clone();
        form.baseline = values.clone();
        form.last = values;
        form.dirty = false;
        form.generation = form.generation.saturating_add(1);
        form.restoring = false;
    }
    pub(super) fn discard_environment_setup(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self
            .environments
            .form
            .original
            .as_ref()
            .map(|record| record.profile.id);
        self.environment_apply_selection(id, window, cx);
    }
    fn environment_pick(&mut self, asset: Asset, window: &mut Window, cx: &mut Context<Self>) {
        if self.environments.busy() {
            return;
        }
        let epoch = self.environments.epoch;
        let generation = self.environments.form.generation;
        self.environments.picking = true;
        cx.spawn_in(window, async move |view, cx| {
            let title = match asset {
                Asset::Client => "Choose client application",
                Asset::KnownHosts => "Choose verified SSH hosts file",
                Asset::Identity => "Choose SSH identity file",
            };
            let file = rfd::AsyncFileDialog::new()
                .set_title(title)
                .pick_file()
                .await;
            let _ = view.update_in(cx, |view, _, cx| {
                if view.environments.epoch != epoch {
                    return;
                }
                view.environments.picking = false;
                if generation == view.environments.form.generation
                    && let Some(file) = file
                {
                    let path = Some(file.path().to_path_buf());
                    match asset {
                        Asset::Client => view.environments.form.client = path,
                        Asset::KnownHosts => view.environments.form.known_hosts = path,
                        Asset::Identity => view.environments.form.identity = path,
                    };
                    view.environment_form_changed(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn environment_status(&mut self, cx: &mut Context<Self>) {
        let Some(record) = self.environments.selected_record() else {
            return;
        };
        let id = record.profile.id;
        self.environment_request(
            "helper_environment_status",
            json!({"environment_id":id}),
            RequestKind::Status(id),
            cx,
        );
    }
    fn environment_action(&mut self, action: EnvironmentAction, cx: &mut Context<Self>) {
        if self.environments.busy()
            || self.environments.form.dirty
            || self.environments.form.conflict
        {
            return;
        }
        let Some(record) = self.environments.selected_record() else {
            return;
        };
        let environment_id = record.profile.id;
        if self.environments.unresolved(environment_id) {
            return;
        }
        let operation_id = Uuid::new_v4();
        let tool = match action {
            EnvironmentAction::Connect => "helper_environment_connect",
            EnvironmentAction::Disconnect => "helper_environment_disconnect",
            EnvironmentAction::Create => "helper_environment_create",
            EnvironmentAction::Start => "helper_environment_start",
            EnvironmentAction::Stop => "helper_environment_stop",
            EnvironmentAction::RemoveContainer => "helper_environment_remove_container",
            EnvironmentAction::Remove => "helper_environment_remove",
            _ => return,
        };
        let args = json!({"operation_id":operation_id,"environment_id":environment_id,"expected_revision":record.revision});
        self.environments.notice = format!("{}…", action_progress(action));
        self.environment_request(
            tool,
            args,
            RequestKind::Receipt {
                operation_id,
                environment_id,
                action,
                submitted: None,
            },
            cx,
        );
    }
    fn environment_confirm_remove(
        &mut self,
        container: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.environments.busy() || self.environments.form.dirty {
            return;
        }
        let Some(record) = self.environments.selected_record() else {
            return;
        };
        let id = record.profile.id;
        let revision = record.revision;
        let epoch = self.environments.epoch;
        self.environments.confirming = true;
        let title = if container {
            "Remove the Xylo container?"
        } else {
            "Remove this environment profile?"
        };
        let detail = if container {
            "The managed container will be removed. Its image and named volumes will stay unchanged."
        } else {
            "The saved profile will be removed. This does not remove remote files or existing containers."
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            title,
            Some(detail),
            &[
                PromptButton::cancel("Cancel"),
                PromptButton::new(if container {
                    "Remove container"
                } else {
                    "Remove profile"
                }),
            ],
            cx,
        );
        cx.spawn_in(window, async move |view, cx| {
            let answer = answer.await;
            let _ = view.update_in(cx, |view, _, cx| {
                view.environments.confirming = false;
                if matches!(answer, Ok(1))
                    && epoch == view.environments.epoch
                    && view.environments.selected_record().is_some_and(|record| {
                        record.profile.id == id && record.revision == revision
                    })
                {
                    view.environment_action(
                        if container {
                            EnvironmentAction::RemoveContainer
                        } else {
                            EnvironmentAction::Remove
                        },
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn environment_execute(&mut self, cx: &mut Context<Self>) {
        if self.environments.busy()
            || self.environments.form.dirty
            || self.environments.form.conflict
        {
            return;
        }
        let Some(record) = self.environments.selected_record() else {
            return;
        };
        if !self.environments.connected(record) || self.environments.unresolved(record.profile.id) {
            return;
        }
        let command = match shell_words::split(self.environments.command.read(cx).value().trim()) {
            Ok(command) if !command.is_empty() => command,
            _ => {
                self.environments.notice = "Enter a command and check its quotation marks.".into();
                cx.notify();
                return;
            }
        };
        let environment_id = record.profile.id;
        let operation_id = Uuid::new_v4();
        let args = json!({"operation_id":operation_id,"environment_id":environment_id,"expected_revision":record.revision,"command":command,"timeout_seconds":60});
        self.environments.notice = "Running command…".into();
        self.environment_request(
            "environment_execute",
            args,
            RequestKind::Receipt {
                operation_id,
                environment_id,
                action: EnvironmentAction::Execute,
                submitted: None,
            },
            cx,
        );
    }
    fn environment_saved_results(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.environments.selected else {
            return;
        };
        if self.environments.busy() {
            return;
        }
        self.environments.history = Arc::new(Vec::new());
        self.environments.history_for = Some(id);
        self.environments.history_loaded = false;
        self.environment_request(
            "helper_environment_operations",
            json!({"environment_id":id}),
            RequestKind::History(id),
            cx,
        );
    }
    fn environment_open_saved_result(&mut self, operation_id: Uuid, cx: &mut Context<Self>) {
        let Some(operation) = self.environments.history.iter().find(|operation| {
            operation.operation_id == operation_id
                && Some(operation.environment_id) == self.environments.selected
        }) else {
            return;
        };
        self.environment_request("helper_environment_operation", json!({
            "operation_id":operation_id,"environment_id":operation.environment_id,"action":operation.action}),
            RequestKind::Operation(operation_id), cx);
    }
    fn environment_check_operation(&mut self, operation_id: Uuid, cx: &mut Context<Self>) {
        let Some(operation) = self.environments.operations.get(&operation_id) else {
            return;
        };
        self.environment_request(
            "helper_environment_operation",
            json!({"operation_id":operation_id,"environment_id":operation.environment_id,"action":operation.action}),
            RequestKind::Operation(operation_id), cx,
        );
    }
    fn environment_resolve(
        &mut self,
        succeeded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(operation_id) = self.environments.resolving else {
            return;
        };
        if !self.environments.checked_operations.contains(&operation_id)
            || !self
                .environments
                .operations
                .get(&operation_id)
                .is_some_and(|operation| {
                    operation.state == EnvironmentOperationState::OutcomeUnknown
                })
        {
            self.environments.notice =
                "Check this action's result before recording an observation.".into();
            cx.notify();
            return;
        }
        let note = self
            .environments
            .resolution_note
            .read(cx)
            .value()
            .trim()
            .to_string();
        if note.is_empty() {
            self.environments.notice =
                "Describe the result you checked before recording it.".into();
            cx.notify();
            return;
        }
        if self.environments.busy() {
            return;
        }
        self.environments.confirming = true;
        let epoch = self.environments.epoch;
        let answer = window.prompt(
            PromptLevel::Warning,
            "Record this observed result?",
            Some("This records your observation and does not repeat the action. Check the remote or container result first."),
            &[PromptButton::cancel("Cancel"), PromptButton::new("Record result")], cx,
        );
        cx.spawn_in(window, async move |view, cx| {
            let answer = answer.await;
            let _ = view.update_in(cx, |view, _, cx| {
                view.environments.confirming = false;
                if matches!(answer, Ok(1)) && epoch == view.environments.epoch {
                    view.environment_request(
                        "helper_environment_resolve",
                        json!({"operation_id":operation_id,"succeeded":succeeded,"note":note}),
                        RequestKind::Resolve(operation_id),
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn environment_cancel(&mut self, cx: &mut Context<Self>) {
        for id in self.environments.pending.keys() {
            if let Some(host) = &self.host {
                let _ = host.cancel_capability(*id);
            }
        }
        self.environments.notice =
            "Cancelling this request. Check the previous action's result before retrying.".into();
        cx.notify();
    }
    pub(super) fn environment_result(
        &mut self,
        id: Uuid,
        result: Result<Value, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.environments.retired.remove(&id) {
            return;
        }
        let Some(request) = self.environments.pending.remove(&id) else {
            return;
        };
        if request.epoch != self.environments.epoch {
            return;
        }
        // A transport or decoding failure is not proof that a mutation never
        // reached the helper. Retain its exact identity until a receipt checks it.
        let uncertainty = match &request.kind {
            RequestKind::Receipt {
                operation_id,
                environment_id,
                action,
                ..
            } => Some((*operation_id, *environment_id, *action)),
            _ => None,
        };
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if let RequestKind::Receipt {
                    operation_id,
                    environment_id,
                    action,
                    ..
                } = request.kind
                {
                    self.environments.operations.insert(
                        operation_id,
                        EnvironmentReceipt {
                            operation_id,
                            environment_id,
                            action,
                            state: EnvironmentOperationState::OutcomeUnknown,
                            environment: None,
                            output: None,
                            created_at_ms: None,
                            command_label: None,
                            message:
                                "The action result is not confirmed. Check it before retrying."
                                    .into(),
                        },
                    );
                }
                self.environments.notice = environment_error(&error);
                cx.notify();
                return;
            }
        };
        let summary = value
            .get("summary")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or(value);
        let parsed: Result<(), String> = (|| {
            match request.kind {
                RequestKind::List => {
                    let records: Vec<EnvironmentRecord> = serde_json::from_value(
                        summary
                            .get("environments")
                            .cloned()
                            .ok_or("Environment list is unavailable.")?,
                    )
                    .map_err(|_| "Environment list could not be read.")?;
                    self.environments.records = records;
                    self.environments.loaded = true;
                    if let Some(clients) = summary.get("clients") {
                        self.environments.clients = serde_json::from_value(clients.clone())
                            .map_err(|_| "Available clients could not be read.")?;
                        self.environments.clients_loaded = true;
                    }
                    if let Some(operations) = summary.get("operations") {
                        let operations: Vec<EnvironmentReceipt> =
                            serde_json::from_value(operations.clone())
                                .map_err(|_| "Previous action results could not be read.")?;
                        for receipt in operations {
                            if !terminal(receipt.state) {
                                self.environments
                                    .checked_operations
                                    .insert(receipt.operation_id);
                                self.environments
                                    .operations
                                    .insert(receipt.operation_id, receipt);
                            }
                        }
                    }
                    self.environment_refresh_form_conflict();
                    if !self.environments.clients_loaded {
                        self.environment_request(
                            "helper_environment_clients",
                            json!({}),
                            RequestKind::Clients,
                            cx,
                        );
                    }
                }
                RequestKind::Clients => {
                    self.environments.clients = serde_json::from_value(
                        summary
                            .get("clients")
                            .cloned()
                            .ok_or("Available clients could not be read.")?,
                    )
                    .map_err(|_| "Available clients could not be read.")?;
                    self.environments.clients_loaded = true;
                }
                RequestKind::History(expected) => {
                    if self.environments.selected != Some(expected)
                        || self.environments.history_for != Some(expected)
                    {
                        return Ok(());
                    }
                    let operations: Vec<EnvironmentReceipt> = serde_json::from_value(
                        summary
                            .get("operations")
                            .cloned()
                            .ok_or("Saved command results are unavailable.")?,
                    )
                    .map_err(|_| "Saved command results could not be read.")?;
                    if operations.len() > 1152
                        || operations
                            .iter()
                            .any(|operation| operation.environment_id != expected)
                    {
                        return Err("Saved command results could not be confirmed.".into());
                    }
                    let mut unique = BTreeMap::new();
                    for operation in operations {
                        unique.entry(operation.operation_id).or_insert(operation);
                    }
                    let mut history = unique
                        .into_values()
                        .filter(|operation| operation.action == EnvironmentAction::Execute)
                        .collect::<Vec<_>>();
                    history.sort_unstable_by(|a, b| {
                        b.created_at_ms
                            .cmp(&a.created_at_ms)
                            .then(a.operation_id.cmp(&b.operation_id))
                    });
                    self.environments.history = Arc::new(history);
                    self.environments.history_loaded = true;
                }
                RequestKind::Status(expected) => {
                    let status: EnvironmentStatus = serde_json::from_value(summary)
                        .map_err(|_| "Environment status could not be read.")?;
                    if status.environment.profile.id != expected {
                        return Err("Environment status could not be confirmed.".into());
                    }
                    self.environment_accept_record(status.environment.clone(), None, cx);
                    self.environments.notice = status.message.clone();
                    self.environments.statuses.insert(expected, status);
                }
                RequestKind::Receipt {
                    operation_id,
                    submitted,
                    ..
                } => {
                    let receipt: EnvironmentReceipt = serde_json::from_value(summary).map_err(
                        |_| "The action result could not be read. Refresh before trying again.",
                    )?;
                    if receipt.operation_id != operation_id {
                        return Err("The action result could not be confirmed.".into());
                    }
                    let verified = receipt.state == EnvironmentOperationState::Succeeded
                        && matches!(
                            receipt.action,
                            EnvironmentAction::Connect | EnvironmentAction::Execute
                        );
                    if verified
                        && let Some(record) = receipt.environment.clone()
                        && record.connected
                    {
                        self.environments.statuses.insert(
                            record.profile.id,
                            EnvironmentStatus {
                                environment: record,
                                state: EnvironmentState::Connected,
                                message: receipt.message.clone(),
                            },
                        );
                    }
                    self.environment_accept_receipt(
                        receipt,
                        submitted.map(|values| *values),
                        window,
                        cx,
                    );
                }
                RequestKind::Operation(expected) | RequestKind::Resolve(expected) => {
                    let receipt: EnvironmentReceipt = serde_json::from_value(summary)
                        .map_err(|_| "The previous action result could not be read.")?;
                    if receipt.operation_id != expected {
                        return Err("The previous action could not be confirmed.".into());
                    }
                    let submitted = self
                        .environments
                        .submissions
                        .get(&receipt.operation_id)
                        .cloned();
                    self.environment_accept_receipt(receipt, submitted, window, cx);
                }
            }
            Ok(())
        })();
        if let Err(message) = parsed {
            if let Some((operation_id, environment_id, action)) = uncertainty {
                self.environments
                    .operations
                    .entry(operation_id)
                    .or_insert(EnvironmentReceipt {
                        operation_id,
                        environment_id,
                        action,
                        state: EnvironmentOperationState::OutcomeUnknown,
                        environment: None,
                        output: None,
                        created_at_ms: None,
                        command_label: None,
                        message: "The action result is not confirmed. Check it before retrying."
                            .into(),
                    });
            }
            self.environments.notice = message;
        }
        cx.notify();
    }
    fn environment_accept_record(
        &mut self,
        record: EnvironmentRecord,
        submitted: Option<Values>,
        cx: &mut Context<Self>,
    ) {
        let id = record.profile.id;
        if self
            .environments
            .records
            .iter()
            .any(|current| current.profile.id == id && current.revision > record.revision)
        {
            return;
        }
        self.environments
            .records
            .retain(|existing| existing.profile.id != id);
        self.environments.records.push(record.clone());
        if self.environments.form.id == id {
            if let Some(values) = submitted {
                self.environments.form.original = Some(record);
                self.environments.form.baseline = values;
                self.environments.form.dirty =
                    self.environments.form.values(cx) != self.environments.form.baseline;
                self.environments.selected = Some(id);
                self.environments.form.conflict = false;
            } else if self
                .environments
                .form
                .original
                .as_ref()
                .is_some_and(|old| old.profile == record.profile)
            {
                self.environments.form.original = Some(record);
            } else if self.environments.form.original.is_some() {
                self.environments.form.conflict = true;
            }
        }
    }
    fn environment_accept_receipt(
        &mut self,
        receipt: EnvironmentReceipt,
        submitted: Option<Values>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.environments.notice = receipt.message.clone();
        let id = receipt.environment_id;
        self.environments
            .checked_operations
            .insert(receipt.operation_id);
        if let Some(output) = &receipt.output {
            self.environments.output_details = Some(output.clone());
            self.environments.output_environment = receipt
                .environment
                .as_ref()
                .map(|record| record.profile.name.clone())
                .or_else(|| {
                    self.environments
                        .records
                        .iter()
                        .find(|record| record.profile.id == receipt.environment_id)
                        .map(|record| record.profile.name.clone())
                })
                .unwrap_or_else(|| "Environment".into());
            for artifact in &output.artifacts {
                self.record_artifact(artifact.path.clone());
            }
            let text = format!(
                "{}{}{}",
                output.stdout,
                if output.stderr.is_empty() { "" } else { "\n" },
                output.stderr
            );
            self.environments
                .output
                .update(cx, |input, cx| input.set_value(text, window, cx));
        }
        if receipt.state == EnvironmentOperationState::Succeeded {
            if let Some(record) = receipt.environment.clone() {
                self.environment_accept_record(record, submitted, cx);
            }
            if receipt.action == EnvironmentAction::Remove {
                self.environments
                    .records
                    .retain(|record| record.profile.id != id);
                if self.environments.selected == Some(id) {
                    self.environment_apply_selection(None, window, cx);
                }
            }
        } else if let Some(record) = &receipt.environment
            && self.environments.form.original.as_ref().is_some_and(|old| {
                old.profile.id == record.profile.id && old.revision != record.revision
            })
        {
            self.environments.form.conflict = true;
        }
        if terminal(receipt.state) {
            self.environments.submissions.remove(&receipt.operation_id);
            self.environments
                .checked_operations
                .remove(&receipt.operation_id);
            self.environments.operations.remove(&receipt.operation_id);
            if self.environments.resolving == Some(receipt.operation_id) {
                self.environments.resolving = None;
            }
        } else {
            self.environments
                .operations
                .insert(receipt.operation_id, receipt);
        }
    }
    fn environment_refresh_form_conflict(&mut self) {
        if let Some(original) = &self.environments.form.original {
            match self
                .environments
                .records
                .iter()
                .find(|record| record.profile.id == original.profile.id)
            {
                Some(current) if current.profile == original.profile => {
                    self.environments.form.original = Some(current.clone())
                }
                _ => self.environments.form.conflict = true,
            }
        }
    }
}
fn terminal(state: EnvironmentOperationState) -> bool {
    matches!(
        state,
        EnvironmentOperationState::Succeeded
            | EnvironmentOperationState::Failed
            | EnvironmentOperationState::Cancelled
    )
}
fn values_from_profile(profile: &EnvironmentProfile) -> Values {
    let mut values = Values {
        name: profile.name.clone(),
        ..Default::default()
    };
    match &profile.target {
        EnvironmentTarget::Ssh {
            executable,
            host,
            user,
            port,
            known_hosts,
            identity_file,
        } => {
            values.client = Some(executable.clone());
            values.host = host.clone();
            values.user = user.clone();
            values.port = port.to_string();
            values.known_hosts = Some(known_hosts.clone());
            values.identity = identity_file.clone();
        }
        EnvironmentTarget::Container {
            executable,
            runtime,
            container,
        } => {
            values.client = Some(executable.clone());
            values.kind = if *runtime == ContainerRuntime::Docker {
                Kind::Docker
            } else {
                Kind::Podman
            };
            match container {
                ContainerTarget::Existing { target } => values.target = target.clone(),
                ContainerTarget::Managed { image, command } => {
                    values.managed = true;
                    values.image = image.clone();
                    values.startup = shell_words::join(command);
                }
            }
        }
    }
    values
}
fn action_progress(action: EnvironmentAction) -> &'static str {
    match action {
        EnvironmentAction::Save => "Saving profile",
        EnvironmentAction::Remove => "Removing profile",
        EnvironmentAction::Connect => "Connecting",
        EnvironmentAction::Disconnect => "Disconnecting",
        EnvironmentAction::Create => "Creating container",
        EnvironmentAction::Start => "Starting container",
        EnvironmentAction::Stop => "Stopping container",
        EnvironmentAction::RemoveContainer => "Removing container",
        EnvironmentAction::Execute => "Running command",
    }
}
fn environment_error(error: &str) -> String {
    if error.contains("cancel") {
        "The request was cancelled. Check the previous action to confirm its result before retrying.".into()
    } else if error.contains("revision") || error.contains("changed") {
        "This profile changed elsewhere. Refresh and review the saved setup before trying again."
            .into()
    } else if error.contains("unsupported") {
        "This environment connection is not supported on this platform.".into()
    } else {
        "The environment request could not finish. Check its saved setup and previous action result before retrying.".into()
    }
}

#[cfg(test)]
#[allow(dead_code)]
impl Workbench {
    pub(super) fn validation_environment_setup(
        &mut self,
        profile: EnvironmentProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.environment_apply_values(values_from_profile(&profile), window, cx);
        self.environments.form.original = None;
        self.environments.form.id = profile.id;
        self.environments.form.baseline = Values::default();
        self.environments.form.dirty = true;
        self.environments.form.generation = self.environments.form.generation.saturating_add(1);
        cx.notify();
    }
    pub(super) fn validation_environment_records(&self) -> Vec<EnvironmentRecord> {
        self.environments.records.clone()
    }
    pub(super) fn validation_environment_idle(&self) -> bool {
        self.environments.pending.is_empty() && !self.environments.confirming
    }
    pub(super) fn validation_environment_loaded(&self) -> bool {
        self.environments.loaded
    }
    pub(super) fn validation_environment_output(&self, cx: &App) -> String {
        self.environments.output.read(cx).value().to_string()
    }
    pub(super) fn validation_environment_output_details(&self) -> Option<EnvironmentOutput> {
        self.environments.output_details.clone()
    }
    pub(super) fn validation_environment_history(&self) -> Vec<EnvironmentReceipt> {
        self.environments.history.as_ref().clone()
    }
    pub(super) fn validation_environment_notice(&self) -> &str {
        &self.environments.notice
    }
    pub(super) fn validation_environment_dirty(&self) -> bool {
        self.environments.form.dirty
    }
    pub(super) fn validation_environment_name(&self, cx: &App) -> String {
        self.environments.form.name.read(cx).value().to_string()
    }
    pub(super) fn validation_environment_focus_name(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.environments
            .form
            .name
            .update(cx, |input, cx| input.focus(window, cx));
    }
    pub(super) fn validation_environment_operations(&self) -> Vec<EnvironmentReceipt> {
        self.environments.operations.values().cloned().collect()
    }
    pub(super) fn validation_environment_note(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.environments
            .resolution_note
            .update(cx, |input, cx| input.set_value(text, window, cx));
    }
}
