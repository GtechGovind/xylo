use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;
use xylo_core::{
    AppSettings, ChatMessage, ConnectorConfig, Conversation, ConversationCursor, ConversationPage,
    HistoryPage, MessageStatus, TitleSource, TurnRequest, Usage, now_ms,
};

type Job = Box<dyn FnOnce(&mut Connection) + Send>;
/// Operations admitted to the database thread at once. Callers beyond this wait
/// for a slot (up to [`QUEUE_WAIT`]) instead of failing a burst outright.
pub(crate) const QUEUE_CAPACITY: usize = 128;
const QUEUE_WAIT: Duration = Duration::from_secs(30);
const MAX_PAGE: usize = 1000;

/// Reads a non-negative SQLite integer as `usize`. rusqlite deliberately has no
/// `usize` conversion because SQLite integers are signed 64-bit values.
pub(crate) fn row_usize(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<usize> {
    let value: i64 = row.get(index)?;
    usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

/// Converts a bounded in-memory count to an SQLite integer parameter.
pub(crate) fn sql_count(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Shared by user-turn registration and atomic draft admission. The caller owns
/// the transaction; this function never commits or performs filesystem I/O.
pub(crate) fn register_user_turn_in_transaction(
    conn: &Connection,
    turn: &TurnRequest,
) -> Result<()> {
    ensure!(
        turn.attachments.len() <= xylo_core::MAX_ATTACHMENTS_PER_TURN,
        "Attach at most four images"
    );
    ensure!(
        turn.attachments
            .iter()
            .all(|image| image.session_id == turn.session_id),
        "Image belongs to another conversation"
    );
    let message_id = Uuid::new_v5(&turn.turn_id, b"user-message");
    let previous: Option<String> = conn
        .query_row(
            "SELECT json FROM messages WHERE id=?1",
            [message_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(previous) = previous {
        let previous: ChatMessage = serde_json::from_str(&previous)?;
        ensure!(
            previous.session_id == turn.session_id
                && previous.role == "user"
                && previous.content == turn.prompt
                && previous.attachments == turn.attachments,
            "Turn ID was already registered with different user input"
        );
    }
    let timestamp = now_ms();
    let previous: Option<String> = conn
        .query_row(
            "SELECT json FROM conversations WHERE id=?1",
            [turn.session_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let mut conversation = match previous {
        Some(json) => serde_json::from_str::<Conversation>(&json)?,
        None => Conversation {
            id: turn.session_id,
            title: if turn.prompt.trim().is_empty() {
                "Image conversation".into()
            } else {
                turn.prompt.chars().take(64).collect()
            },
            workspace: turn.workspace.clone(),
            updated_at: timestamp,
            title_source: Default::default(),
            pinned: false,
            archived: false,
        },
    };
    conversation.updated_at = timestamp;
    conn.execute("INSERT INTO conversations(id,json,updated_at) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET json=excluded.json,updated_at=excluded.updated_at",params![turn.session_id.to_string(),serde_json::to_string(&conversation)?,timestamp])?;
    let message = ChatMessage {
        activity_count: 0,
        activity_summary: None,
        turn_id: Some(turn.turn_id),
        reasoning: None,
        id: message_id,
        session_id: turn.session_id,
        role: "user".into(),
        content: turn.prompt.clone(),
        created_at: timestamp,
        attachments: turn.attachments.clone(),
        status: Default::default(),
    };
    conn.execute(
        "INSERT OR IGNORE INTO messages(id,session_id,json,created_at) VALUES(?1,?2,?3,?4)",
        params![
            message_id.to_string(),
            turn.session_id.to_string(),
            serde_json::to_string(&message)?,
            timestamp
        ],
    )?;
    Ok(())
}

/// Starts a transaction that takes the write lock up front. Every transaction
/// that reads and then writes must use this: a deferred transaction that later
/// upgrades its lock fails immediately with "database is locked" when another
/// process holds the write lock, because SQLite cannot honour the busy timeout
/// on a lock upgrade without risking deadlock.
pub(crate) fn write_transaction(
    conn: &mut Connection,
) -> rusqlite::Result<rusqlite::Transaction<'_>> {
    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
}

/// Counting semaphore bounding the operations queued for the database thread.
struct QueueSlots {
    free: Mutex<usize>,
    released: Condvar,
}

impl QueueSlots {
    fn acquire(self: &Arc<Self>, deadline: Instant) -> Result<QueueSlot> {
        let mut free = self
            .free
            .lock()
            .map_err(|_| anyhow!("Database queue lock poisoned"))?;
        while *free == 0 {
            let now = Instant::now();
            ensure!(now < deadline, "The database is busy; retry later");
            free = self
                .released
                .wait_timeout(free, deadline - now)
                .map_err(|_| anyhow!("Database queue lock poisoned"))?
                .0;
        }
        *free -= 1;
        Ok(QueueSlot(self.clone()))
    }
}

/// Returns its slot when the queued operation finishes or is dropped unrun.
struct QueueSlot(Arc<QueueSlots>);

impl Drop for QueueSlot {
    fn drop(&mut self) {
        if let Ok(mut free) = self.0.free.lock() {
            *free += 1;
            self.0.released.notify_one();
        }
    }
}

/// Cloneable handle to a dedicated SQLite thread. At most [`QUEUE_CAPACITY`]
/// operations are queued; further callers wait for a slot until a deadline.
#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
    slots: Arc<QueueSlots>,
    reset: Option<Arc<crate::schema::DatabaseReset>>,
    pub(crate) restored_context_sessions: Arc<std::collections::HashSet<Uuid>>,
    pub(crate) forgotten_action_sessions: Arc<Mutex<std::collections::HashSet<Uuid>>>,
    pub(crate) active_actions: Arc<Mutex<Vec<ActiveAction>>>,
    pub(crate) completed_import_turns: Arc<Mutex<std::collections::HashSet<(Uuid, Uuid)>>>,
    pub(crate) incomplete_action_turns: Arc<Mutex<std::collections::HashSet<(Uuid, Uuid)>>>,
    pub(crate) live_action_updates: tokio::sync::broadcast::Sender<crate::LiveActionUpdate>,
    pub(crate) extension_executions: Arc<Mutex<Vec<Value>>>,
    pub(crate) extension_bindings: Arc<Mutex<std::collections::HashMap<Uuid, Value>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub messages: Value,
    pub updated_at: i64,
    /// The harness-selected alternative for this checkpoint's turn only.
    #[serde(default)]
    pub fallback_model: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompletedTurn {
    pub reply_id: Uuid,
    pub text: String,
    pub usage: Usage,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionIntent {
    pub id: Uuid,
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub name: String,
    pub arguments: Value,
    pub writes: bool,
    pub idempotency_key: String,
}
#[derive(Clone, Debug, PartialEq)]
pub enum ActionDecision {
    Execute,
    Reuse(Value),
    InFlight,
    OutcomeUnknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveAction {
    pub intent: ActionIntent,
    pub state: String,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub updated_at: i64,
    #[serde(skip)]
    pub(crate) cursor: i64,
    #[serde(default = "complete_evidence")]
    pub evidence_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) key_fingerprint: Option<String>,
    #[serde(skip)]
    pub(crate) imported: bool,
    #[serde(skip)]
    pub(crate) retained_bytes: usize,
}
fn complete_evidence() -> bool {
    true
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillVersion {
    pub name: String,
    pub version: i64,
    pub content: String,
    pub created_at: i64,
}

impl Store {
    /// Opens the database as its owner, creating a new schema or migrating the
    /// exact previous version without execution journals. An unexpected previous
    /// schema is left untouched; other unsupported schemas retain the reset policy.
    /// Call this from exactly one process. Does not create parent folders.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::start(path.as_ref(), crate::schema::OpenMode::Owner)
    }

    /// Opens a database the host already created, for workers and tools. A
    /// missing file, an empty database or any other schema is an error; the
    /// file is never created, migrated, renamed or replaced.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self> {
        Self::start(path.as_ref(), crate::schema::OpenMode::Existing)
    }

    fn start(path: &Path, mode: crate::schema::OpenMode) -> Result<Self> {
        let path = path.to_owned();
        let connection_path = path.clone();
        let (sender, receiver) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("xylo-sqlite".into())
            .spawn(move || {
                register_vec();
                match crate::schema::open(&connection_path, mode) {
                    Ok((mut conn, reset)) => {
                        let _ = ready_tx.send(Ok(reset));
                        for job in receiver {
                            job(&mut conn);
                        }
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                    }
                }
            })
            .context("Start database worker")?;
        let reset = ready_rx
            .recv()
            .context("Database worker stopped during startup")??;
        let mut store = Self {
            sender,
            slots: Arc::new(QueueSlots {
                free: Mutex::new(QUEUE_CAPACITY),
                released: Condvar::new(),
            }),
            reset: reset.map(Arc::new),
            restored_context_sessions: Arc::default(),
            forgotten_action_sessions: Arc::default(),
            active_actions: Arc::default(),
            completed_import_turns: Arc::default(),
            incomplete_action_turns: Arc::default(),
            live_action_updates: tokio::sync::broadcast::channel(256).0,
            extension_executions: Arc::default(),
            extension_bindings: Arc::default(),
        };
        let restored = store.call(|conn| {
            let mut statement = conn.prepare("SELECT DISTINCT session_id FROM checkpoint_messages UNION SELECT session_id FROM messages WHERE json_extract(json,'$.role')='assistant'")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.map(|row| Ok(Uuid::parse_str(&row?)?)).collect::<Result<std::collections::HashSet<_>>>()
        })?;
        store.restored_context_sessions = Arc::new(restored);
        Ok(store)
    }

    /// Set when opening replaced an unusable database with an empty one; the
    /// host shows its text once so the user knows their data was reset.
    pub fn reset_notice(&self) -> Option<crate::schema::DatabaseReset> {
        self.reset.as_deref().cloned()
    }

    pub(crate) fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let slot = self.slots.acquire(Instant::now() + QUEUE_WAIT)?;
        let (tx, rx) = mpsc::sync_channel(1);
        self.sender
            .send(Box::new(move |conn| {
                let _ = tx.send(operation(conn));
                drop(slot);
            }))
            .map_err(|_| anyhow!("Database worker unavailable"))?;
        rx.recv().context("Database operation interrupted")?
    }

    pub fn settings(&self) -> Result<AppSettings> {
        self.call(|conn| {
            let json: Option<String> = conn
                .query_row("SELECT value FROM settings WHERE key='app'", [], |r| {
                    r.get(0)
                })
                .optional()?;
            Ok(match json {
                Some(json) => serde_json::from_str(&json)?,
                None => AppSettings::default(),
            })
        })
    }
    pub fn save_settings(&self, settings: &AppSettings) -> Result<()> {
        ensure!(
            settings.pricing_overrides.len() <= 512
                && settings
                    .pricing_overrides
                    .iter()
                    .all(|(model, price)| !model.trim().is_empty()
                        && model.len() <= 200
                        && price.valid()),
            "Enter valid model prices"
        );
        ensure!(
            settings
                .status_line_command
                .as_ref()
                .is_none_or(|command| command.len() <= 8192),
            "Status command exceeds 8 KiB"
        );
        // Settings are plain JSON on disk: refuse secrets in custom headers.
        for provider in std::iter::once(&settings.provider).chain(&settings.providers) {
            crate::providers::validate_custom_headers(&provider.headers)?;
        }
        let json = serde_json::to_string(settings)?;
        self.call(move |conn| { conn.execute("INSERT INTO settings(key,value) VALUES('app',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [json])?; Ok(()) })
    }
    /// Native execution is separate from ordinary tool rules and remains scope-bound.
    pub fn native_execution_granted(&self, workspace: &std::path::Path, key: &str) -> Result<bool> {
        let workspace = workspace.to_string_lossy().into_owned();
        let key = key.to_owned();
        self.call(move |conn| Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM native_execution_grants WHERE workspace=?1 AND key=?2)",params![workspace,key],|row|row.get(0))?))
    }
    /// Records a user-reviewed native language-server grant, never a general allow rule.
    pub fn grant_native_execution(&self, workspace: &std::path::Path, key: &str) -> Result<()> {
        ensure!(
            key.starts_with("native-lsp:"),
            "Only explicit native LSP grants may be recorded"
        );
        let workspace = workspace.to_string_lossy().into_owned();
        let key = key.to_owned();
        self.call(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO native_execution_grants(workspace,key) VALUES(?1,?2)",
                params![workspace, key],
            )?;
            Ok(())
        })
    }

    /// Renames a conversation. A generated title never replaces one the user chose.
    /// Returns the stored conversation, or `None` when it does not exist.
    pub fn rename_conversation(
        &self,
        id: Uuid,
        title: &str,
        source: TitleSource,
    ) -> Result<Option<Conversation>> {
        let title = title.trim().to_owned();
        ensure!(
            !title.is_empty() && title.chars().count() <= 120,
            "Titles must be 1 to 120 characters"
        );
        self.call(move |conn| {
            let tx = write_transaction(conn)?;
            let Some(json) = tx
                .query_row(
                    "SELECT json FROM conversations WHERE id=?1",
                    [id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            else {
                return Ok(None);
            };
            let mut conversation: Conversation = serde_json::from_str(&json)?;
            if source == TitleSource::Generated && conversation.title_source == TitleSource::User {
                return Ok(Some(conversation));
            }
            conversation.title = title;
            conversation.title_source = source;
            tx.execute(
                "UPDATE conversations SET json=?1 WHERE id=?2",
                params![serde_json::to_string(&conversation)?, id.to_string()],
            )?;
            tx.commit()?;
            Ok(Some(conversation))
        })
    }

    /// Pins or unpins a conversation. Returns the stored conversation, or
    /// `None` when it does not exist.
    pub fn set_conversation_pinned(&self, id: Uuid, pinned: bool) -> Result<Option<Conversation>> {
        self.update_conversation(id, move |conversation| conversation.pinned = pinned)
    }

    /// Archives or restores a conversation. Archived chats keep their history
    /// but are left out of the chat list and chat search. Returns the stored
    /// conversation, or `None` when it does not exist.
    pub fn set_conversation_archived(
        &self,
        id: Uuid,
        archived: bool,
    ) -> Result<Option<Conversation>> {
        self.update_conversation(id, move |conversation| {
            conversation.archived = archived;
            if archived {
                conversation.pinned = false;
            }
        })
    }

    /// Updates the selected checkout after a conversation changes worktrees.
    pub fn set_conversation_workspace(
        &self,
        id: Uuid,
        workspace: std::path::PathBuf,
    ) -> Result<Option<Conversation>> {
        self.update_conversation(id, move |conversation| conversation.workspace = workspace)
    }

    /// Applies `change` to one stored conversation inside a write transaction.
    fn update_conversation(
        &self,
        id: Uuid,
        change: impl FnOnce(&mut Conversation) + Send + 'static,
    ) -> Result<Option<Conversation>> {
        self.call(move |conn| {
            let tx = write_transaction(conn)?;
            let Some(json) = tx
                .query_row(
                    "SELECT json FROM conversations WHERE id=?1",
                    [id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            else {
                return Ok(None);
            };
            let mut conversation: Conversation = serde_json::from_str(&json)?;
            change(&mut conversation);
            tx.execute(
                "UPDATE conversations SET json=?1 WHERE id=?2",
                params![serde_json::to_string(&conversation)?, id.to_string()],
            )?;
            tx.commit()?;
            Ok(Some(conversation))
        })
    }

    /// Stores what an interrupted or failed turn had streamed, so the transcript
    /// keeps it after a reload. Idempotent per turn; a later completion of the
    /// same turn replaces it, and a turn that already completed is left alone.
    pub fn record_interrupted_reply(
        &self,
        session_id: Uuid,
        turn_id: Uuid,
        text: &str,
        status: MessageStatus,
    ) -> Result<()> {
        ensure!(
            status != MessageStatus::Complete,
            "Use complete_turn for completed replies"
        );
        let mut message = ChatMessage {
            activity_count: 0,
            activity_summary: None,
            turn_id: Some(turn_id),
            reasoning: None,
            id: interrupted_reply_id(turn_id),
            session_id,
            role: "assistant".into(),
            content: text.to_owned(),
            created_at: now_ms(),
            attachments: Vec::new(),
            status,
        };
        crate::turn_activity::summarize_activity(&self.active_actions()?, &mut message);
        self.call(move |conn| {
            let tx = write_transaction(conn)?;
            let completed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM completed_turns WHERE session_id=?1 AND turn_id=?2)",
                params![session_id.to_string(), turn_id.to_string()],
                |row| row.get(0),
            )?;
            if !completed {
                crate::turn_activity::hydrate_activity(&tx, &mut message)?;
                tx.execute(
                    "INSERT INTO messages(id,session_id,json,created_at) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET json=excluded.json",
                    params![message.id.to_string(), session_id.to_string(), serde_json::to_string(&message)?, message.created_at],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn conversation(&self, id: Uuid) -> Result<Option<Conversation>> {
        self.call(move |conn| {
            let json: Option<String> = conn
                .query_row(
                    "SELECT json FROM conversations WHERE id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            json.map(|json| Ok(serde_json::from_str(&json)?))
                .transpose()
        })
    }
    /// Distinct chat roots, including archives, for retained checkout discovery.
    pub fn conversation_workspaces(&self) -> Result<Vec<std::path::PathBuf>> {
        self.call(move |conn| {
            let mut statement = conn
                .prepare("SELECT DISTINCT json_extract(json,'$.workspace') FROM conversations")?;
            let rows = statement.query_map([], |row| {
                row.get::<_, String>(0).map(std::path::PathBuf::from)
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
    /// Managed-worktree chats, including archives, for host lifecycle checks.
    pub fn worktree_conversations(&self) -> Result<Vec<Conversation>> {
        self.call(move |conn| json_rows(conn,
            "SELECT json FROM conversations WHERE json_extract(json,'$.workspace') LIKE '%/.xylo/worktrees/%'", []))
    }
    pub fn conversations(&self, limit: usize) -> Result<Vec<Conversation>> {
        self.call(move |conn| {
            json_rows(
                conn,
                "SELECT json FROM conversations WHERE json_extract(json,'$.archived') IS NOT 1 ORDER BY updated_at DESC LIMIT ?1",
                [limit.min(MAX_PAGE) as i64],
            )
        })
    }
    /// Typed keyset pages; newly touched chats can move to the first page without
    /// making the cursor depend on a changing numeric offset.
    pub fn conversations_page(
        &self,
        before: Option<ConversationCursor>,
        limit: usize,
    ) -> Result<ConversationPage> {
        let limit = limit.clamp(1, 200);
        self.call(move|connection|{
            let (timestamp,id)=before.map_or((None,None),|cursor|(Some(cursor.updated_at),Some(cursor.id.to_string())));
            let mut statement=connection.prepare("SELECT json FROM conversations WHERE json_extract(json,'$.archived') IS NOT 1 AND (?1 IS NULL OR updated_at<?1 OR (updated_at=?1 AND id<?2)) ORDER BY updated_at DESC,id DESC LIMIT ?3")?;
            let mut conversations=statement.query_map(params![timestamp,id,(limit+1) as i64],|row|row.get::<_,String>(0))?.map(|row|Ok(serde_json::from_str::<Conversation>(&row?)?)).collect::<Result<Vec<_>>>()?;
            let has_more=conversations.len()>limit;conversations.truncate(limit);
            let before=if has_more{conversations.last().map(|conversation|ConversationCursor{updated_at:conversation.updated_at,id:conversation.id})}else{None};
            Ok(ConversationPage{conversations,before,has_more})
        })
    }
    pub fn upsert_conversation(&self, conversation: &Conversation) -> Result<()> {
        let c = conversation.clone();
        let json = serde_json::to_string(&c)?;
        self.call(move |conn| { conn.execute("INSERT INTO conversations(id,json,updated_at) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET json=excluded.json,updated_at=excluded.updated_at",params![c.id.to_string(),json,c.updated_at])?; Ok(()) })
    }
    /// Atomically records a user turn once and touches its conversation. Both the
    /// host and worker may call this on resume without duplicating display history.
    /// A conversation's original title/workspace remain stable on later turns.
    pub fn register_user_turn(&self, turn: &TurnRequest) -> Result<()> {
        let turn = turn.clone();
        self.call(move |conn| {
            let tx = write_transaction(conn)?;
            register_user_turn_in_transaction(&tx, &turn)?;
            tx.commit()?;
            Ok(())
        })
    }
    pub fn messages(&self, session_id: Uuid, limit: usize) -> Result<Vec<ChatMessage>> {
        self.call(move |conn| {
            let mut messages: Vec<ChatMessage> = json_rows(conn,"SELECT json FROM (SELECT rowid,json,created_at FROM messages WHERE session_id=?1 ORDER BY created_at DESC,rowid DESC LIMIT ?2) ORDER BY created_at,rowid",params![session_id.to_string(),limit.min(MAX_PAGE) as i64])?;
            for message in &mut messages { crate::turn_activity::hydrate_activity(conn, message)?; }
            Ok(messages)
        })
    }
    /// Pages display history in append order, independently of tied wall clocks.
    /// `before` in the result is the cursor for the next older page, when present.
    /// The database must not be VACUUMed while these rowid cursors are in use.
    pub fn messages_page(
        &self,
        session_id: Uuid,
        before: Option<i64>,
        limit: usize,
    ) -> Result<HistoryPage> {
        let limit = limit.clamp(1, 200);
        self.call(move |conn| {
            let mut statement = conn.prepare(
                "SELECT rowid,json FROM messages WHERE session_id=?1 AND (?2 IS NULL OR rowid<?2) ORDER BY rowid DESC LIMIT ?3",
            )?;
            let mut rows = statement
                .query_map(params![session_id.to_string(), before, (limit + 1) as i64], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let has_more = rows.len() > limit;
            rows.truncate(limit);
            let next_before = has_more.then(|| rows.last().map(|row| row.0)).flatten();
            rows.reverse();
            let mut messages = rows.into_iter().map(|(_, json)| serde_json::from_str::<ChatMessage>(&json).map_err(Into::into)).collect::<Result<Vec<_>>>()?;
            for message in &mut messages { crate::turn_activity::hydrate_activity(conn, message)?; }
            Ok(HistoryPage { session_id, messages, before: next_before, has_more })
        })
    }
    pub fn append_message(&self, message: &ChatMessage) -> Result<()> {
        let m = message.clone();
        let json = serde_json::to_string(&m)?;
        self.call(move |conn| {
            conn.execute(
                "INSERT INTO messages(id,session_id,json,created_at) VALUES(?1,?2,?3,?4)",
                params![
                    m.id.to_string(),
                    m.session_id.to_string(),
                    json,
                    m.created_at
                ],
            )?;
            Ok(())
        })
    }
    pub fn completed_turn(&self, session_id: Uuid, turn_id: Uuid) -> Result<Option<CompletedTurn>> {
        self.call(move |conn| {
            let json: Option<String> = conn
                .query_row(
                    "SELECT json FROM completed_turns WHERE session_id=?1 AND turn_id=?2",
                    params![session_id.to_string(), turn_id.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            json.map(|j| serde_json::from_str(&j).map_err(Into::into))
                .transpose()
        })
    }
    /// One transaction commits the engine boundary, display history and completion
    /// receipt. Replaying a worker request after a lost Completed event is read-only.
    pub fn complete_turn(
        &self,
        session_id: Uuid,
        turn_id: Uuid,
        messages: &Value,
        text: &str,
        usage: &Usage,
    ) -> Result<()> {
        self.complete_turn_as(
            session_id,
            turn_id,
            messages,
            text,
            usage,
            MessageStatus::Complete,
        )
    }

    /// [`Store::complete_turn`], storing the assistant reply with `status`:
    /// [`MessageStatus::Truncated`] for an answer that reached the output
    /// limit and is kept as far as it got.
    pub fn complete_turn_as(
        &self,
        session_id: Uuid,
        turn_id: Uuid,
        messages: &Value,
        text: &str,
        usage: &Usage,
        status: MessageStatus,
    ) -> Result<()> {
        self.complete_turn_with_reasoning(session_id, turn_id, messages, text, usage, status, None)
    }

    /// Completes a reply with its separately disclosed provider reasoning.
    #[allow(
        clippy::too_many_arguments,
        reason = "The reply fields share one atomic transaction."
    )]
    pub fn complete_turn_with_reasoning(
        &self,
        session_id: Uuid,
        turn_id: Uuid,
        messages: &Value,
        text: &str,
        usage: &Usage,
        status: MessageStatus,
        reasoning: Option<String>,
    ) -> Result<()> {
        let messages = crate::checkpoint::message_rows(messages)?;
        let receipt = CompletedTurn {
            reply_id: assistant_reply_id(turn_id),
            text: text.into(),
            usage: usage.clone(),
        };
        let receipt_json = serde_json::to_string(&receipt)?;
        let actions = self.active_actions()?;
        self.call(move|conn|{let tx=write_transaction(conn)?;
            let existing:Option<String>=tx.query_row("SELECT json FROM completed_turns WHERE session_id=?1 AND turn_id=?2",params![session_id.to_string(),turn_id.to_string()],|r|r.get(0)).optional()?;
            if let Some(existing)=existing{ensure!(serde_json::from_str::<Value>(&existing)?==serde_json::from_str::<Value>(&receipt_json)?,"Turn was already completed with a different result");return Ok(())}
            let timestamp=now_ms();
            crate::checkpoint::write_rows(&tx,session_id,turn_id,&messages,None,timestamp)?;
            let mut message=ChatMessage{ activity_count: 0, activity_summary: None, turn_id: Some(turn_id), reasoning,id:assistant_reply_id(turn_id),session_id,role:"assistant".into(),content:receipt.text,created_at:timestamp,attachments:Vec::new(), status,};
            crate::turn_activity::summarize_activity(&actions, &mut message);
            tx.execute("INSERT INTO messages(id,session_id,json,created_at) VALUES(?1,?2,?3,?4)",params![message.id.to_string(),session_id.to_string(),serde_json::to_string(&message)?,timestamp])?;
            tx.execute("DELETE FROM messages WHERE id=?1",[interrupted_reply_id(turn_id).to_string()])?;
            tx.execute("INSERT INTO completed_turns(session_id,turn_id,json,completed_at) VALUES(?1,?2,?3,?4)",params![session_id.to_string(),turn_id.to_string(),receipt_json,timestamp])?;
            tx.execute("DELETE FROM turn_usage WHERE session_id=?1 AND turn_id=?2",params![session_id.to_string(),turn_id.to_string()])?;
            tx.execute("UPDATE conversations SET updated_at=?1,json=json_set(json,'$.updated_at',?1) WHERE id=?2",params![timestamp,session_id.to_string()])?;
            tx.execute("INSERT OR IGNORE INTO learning_jobs(session_id,turn_id,state,created_at,updated_at) VALUES(?1,?2,'pending',?3,?3)",params![session_id.to_string(),turn_id.to_string(),timestamp])?;
            tx.commit()?;Ok(())})
    }
    pub(crate) fn active_actions(&self) -> Result<Vec<ActiveAction>> {
        Ok(self
            .active_actions
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?
            .clone())
    }
    pub fn actions_by_name(&self, name: &str, limit: usize) -> Result<Vec<ActiveAction>> {
        let held = self
            .active_actions
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?;
        let mut actions: Vec<_> = held.iter().filter(|a| a.intent.name == name).collect();
        actions.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
        actions.truncate(limit.min(MAX_PAGE));
        Ok(actions.into_iter().cloned().collect())
    }
    /// Deduplicate only within this live Store. No execution state is saved to disk.
    pub fn begin_action(&self, intent: &ActionIntent) -> Result<ActionDecision> {
        ensure!(
            !intent.idempotency_key.trim().is_empty(),
            "Action key cannot be empty"
        );
        let oversized =
            crate::live_actions::serialized_size(intent)? > crate::live_actions::MAX_INTENT_BYTES;
        let mut actions = self
            .active_actions
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?;
        if let Some(old) = actions.iter().find(|a| {
            a.intent.session_id == intent.session_id
                && a.intent.turn_id == intent.turn_id
                && a.matches_key(&intent.idempotency_key)
        }) {
            ensure!(
                !old.imported,
                "Observed activity cannot authorize execution or result reuse"
            );
            ensure!(
                old.intent.name == intent.name
                    && old.matches_arguments(&intent.arguments)
                    && old.intent.writes == intent.writes,
                "Action key was reused for a different operation"
            );
            return match old.state.as_str() {
                "succeeded" => Ok(ActionDecision::Reuse(
                    old.result
                        .clone()
                        .context("Successful action has no result")?,
                )),
                "outcome_unknown" => Ok(ActionDecision::OutcomeUnknown),
                "pending" | "running" => Ok(ActionDecision::InFlight),
                "denied" => bail!("Action was denied"),
                "failed" => {
                    bail!("Action failed; choose a new attempt key after reviewing the failure")
                }
                _ => bail!("Unknown action state"),
            };
        }
        ensure!(
            !actions.iter().any(|a| a.intent.id == intent.id),
            "Action ID already exists"
        );
        // Observability never rejects a legitimate large tool input. Retain its
        // fingerprint and bounded metadata, while the caller executes the original.
        let arguments_fingerprint = oversized
            .then(|| crate::live_actions::fingerprint(&intent.arguments))
            .transpose()?;
        let key_fingerprint = (intent.idempotency_key.len() > 2048)
            .then(|| crate::live_actions::fingerprint(&intent.idempotency_key))
            .transpose()?;
        let retained_intent = if oversized {
            ActionIntent {
                id: intent.id,
                session_id: intent.session_id,
                turn_id: intent.turn_id,
                name: intent.name.chars().take(256).collect(),
                arguments: Value::Null,
                writes: intent.writes,
                idempotency_key: if key_fingerprint.is_some() {
                    String::new()
                } else {
                    intent.idempotency_key.clone()
                },
            }
        } else {
            intent.clone()
        };
        let intent_bytes = crate::live_actions::serialized_size(&retained_intent)?;
        self.make_live_action_room(
            &mut actions,
            intent.session_id,
            intent.turn_id,
            intent_bytes,
            true,
        )?;
        let cursor = actions.last().map_or(1, |a| a.cursor + 1);
        let action = ActiveAction {
            cursor,
            imported: false,
            evidence_complete: !oversized,
            arguments_fingerprint,
            key_fingerprint,
            retained_bytes: intent_bytes,
            intent: retained_intent,
            state: "pending".into(),
            result: None,
            error: None,
            updated_at: now_ms(),
        };
        actions.push(action.clone());
        if oversized {
            self.mark_live_action_incomplete(intent.session_id, intent.turn_id)?;
        }
        let _ = self
            .live_action_updates
            .send(crate::LiveActionUpdate::Action {
                action: Box::new(action),
            });
        Ok(ActionDecision::Execute)
    }
    pub fn mark_action_running(&self, id: Uuid) -> Result<()> {
        self.transition(id, "pending", "running", None, None)
    }
    pub fn deny_action(&self, id: Uuid) -> Result<()> {
        self.transition(id, "pending", "denied", None, None)
    }
    pub fn finish_action(&self, id: Uuid, result: &Value) -> Result<()> {
        self.transition(
            id,
            "running",
            "succeeded",
            Some(serde_json::to_string(result)?),
            None,
        )
    }
    pub fn fail_action(&self, id: Uuid, error: &str) -> Result<()> {
        self.transition(id, "running", "failed", None, Some(error.into()))
    }
    /// Mark a transport-disconnected write as unknown instead of claiming failure.
    pub fn mark_action_unknown(&self, id: Uuid, error: &str) -> Result<()> {
        self.transition(id, "running", "outcome_unknown", None, Some(error.into()))
    }
    /// Retain the actual observation, including committed artifact references,
    /// even when a tool reports failure. This never makes the action replayable.
    pub fn fail_action_with_result(&self, id: Uuid, result: &Value, error: &str) -> Result<()> {
        self.transition(
            id,
            "running",
            "failed",
            Some(serde_json::to_string(result)?),
            Some(error.into()),
        )
    }
    /// Preserve evidence of a partial or uncertain write without recording
    /// success or authorizing another execution of the same action.
    pub fn mark_action_unknown_with_result(
        &self,
        id: Uuid,
        result: &Value,
        error: &str,
    ) -> Result<()> {
        self.transition(
            id,
            "running",
            "outcome_unknown",
            Some(serde_json::to_string(result)?),
            Some(error.into()),
        )
    }
    fn transition(
        &self,
        id: Uuid,
        from: &'static str,
        to: &'static str,
        result: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        let mut actions = self
            .active_actions
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?;
        let index = actions
            .iter()
            .position(|a| a.intent.id == id && a.state == from && !a.imported)
            .context("Action is absent or has an incompatible state")?;
        let total: usize = actions.iter().map(|a| a.retained_bytes).sum();
        let intent_bytes = serde_json::to_vec(&actions[index].intent)?.len();
        let result_bytes = result.as_ref().map_or(0, String::len);
        let mut error = error.map(|e| e.chars().take(2048).collect::<String>());
        if total - actions[index].retained_bytes
            + intent_bytes
            + error.as_ref().map_or(0, String::len)
            > crate::live_actions::MAX_LIVE_BYTES
        {
            error = None;
        }
        let base_bytes = intent_bytes + error.as_ref().map_or(0, String::len);
        let complete = result_bytes <= crate::live_actions::MAX_RESULT_BYTES
            && total - actions[index].retained_bytes + base_bytes + result_bytes
                <= crate::live_actions::MAX_LIVE_BYTES;
        let action = &mut actions[index];
        action.state = to.into();
        action.result = if complete {
            result.map(|r| serde_json::from_str(&r)).transpose()?
        } else {
            None
        };
        action.error = error;
        action.updated_at = now_ms();
        action.evidence_complete &= complete;
        action.retained_bytes = base_bytes + if complete { result_bytes } else { 0 };
        if !complete {
            self.mark_live_action_incomplete(action.intent.session_id, action.intent.turn_id)?;
        }
        let _ = self
            .live_action_updates
            .send(crate::LiveActionUpdate::Action {
                action: Box::new(action.clone()),
            });
        Ok(())
    }
    pub fn action(&self, id: Uuid) -> Result<Option<ActiveAction>> {
        Ok(self
            .active_actions
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?
            .iter()
            .find(|a| a.intent.id == id)
            .cloned())
    }
    pub fn action_for_key(
        &self,
        session_id: Uuid,
        turn_id: Uuid,
        key: &str,
    ) -> Result<Option<ActiveAction>> {
        Ok(self
            .active_actions
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?
            .iter()
            .find(|a| {
                !a.imported
                    && a.intent.session_id == session_id
                    && a.intent.turn_id == turn_id
                    && a.matches_key(key)
            })
            .cloned())
    }
    pub fn unfinished_actions(&self, session_id: Uuid) -> Result<Vec<ActiveAction>> {
        Ok(self
            .active_actions
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?
            .iter()
            .filter(|a| {
                a.intent.session_id == session_id
                    && matches!(a.state.as_str(), "pending" | "running" | "outcome_unknown")
            })
            .cloned()
            .collect())
    }
    pub fn connectors(&self) -> Result<Vec<ConnectorConfig>> {
        self.call(|conn| json_rows(conn, "SELECT json FROM connectors ORDER BY name", []))
    }
    pub fn save_connector(&self, config: &ConnectorConfig) -> Result<()> {
        let c = config.clone();
        self.call(move|conn|{conn.execute("INSERT INTO connectors(id,name,json) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET name=excluded.name,json=excluded.json",params![c.id.to_string(),c.name,serde_json::to_string(&c)?])?;Ok(())})
    }
    pub fn delete_connector(&self, id: Uuid) -> Result<()> {
        self.call(move |conn| {
            conn.execute("DELETE FROM connectors WHERE id=?1", [id.to_string()])?;
            Ok(())
        })
    }

    /// Every stored version of a learned skill, newest first. Evaluation and
    /// promotion belong to the learning service.
    pub fn skill_versions(&self, name: &str) -> Result<Vec<SkillVersion>> {
        let name = name.to_owned();
        self.call(move |conn| {
            json_rows(
                conn,
                "SELECT json FROM skills WHERE name=?1 ORDER BY version DESC",
                [name],
            )
        })
    }
}

fn json_rows<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |r| r.get::<_, String>(0))?;
    rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
}

fn register_vec() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // sqlite-vec's entrypoint is the extension ABI. Registration is process-wide,
        // once before any service connection; both crates link the same bundled SQLite.
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::ffi::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> i32,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}
/// Message id of a turn's completed assistant reply. Derived from the turn,
/// like the user message id, so the UI can match the streamed reply to the
/// stored one exactly.
pub fn assistant_reply_id(turn_id: Uuid) -> Uuid {
    Uuid::new_v5(&turn_id, b"assistant-message")
}

/// Message id of a turn's interrupted reply; stable so recording is idempotent.
fn interrupted_reply_id(turn_id: Uuid) -> Uuid {
    Uuid::new_v5(&turn_id, b"assistant-interrupted")
}
