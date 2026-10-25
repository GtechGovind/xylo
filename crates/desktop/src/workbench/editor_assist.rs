//! GPUI Kit's real completion/hover popovers and diagnostic decorations, backed
//! by the native language service. No file/process I/O runs on the UI thread.
use super::*;
use gpui_kit::component::input::{CompletionProvider, HoverProvider, Rope, RopeExt};
use lsp_types::{CompletionContext, CompletionResponse, Hover};
use serde_json::{Value, json};
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI32, Ordering},
};

type Reply = Result<Value, String>;
type Pending = Arc<Mutex<BTreeMap<Uuid, async_channel::Sender<Reply>>>>;
#[derive(Clone)]
pub(super) struct EditorAssist {
    host: HostClient,
    path: PathBuf,
    version: Arc<AtomicI32>,
    pending: Pending,
}
struct RequestGuard {
    id: Uuid,
    host: HostClient,
    pending: Pending,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.pending.lock().unwrap().remove(&self.id).is_some() {
            let _ = self.host.lsp_cancel(self.id);
        }
    }
}
impl EditorAssist {
    fn request(
        &self,
        text: &Rope,
        offset: usize,
        method: &str,
        extra: Value,
        cx: &App,
    ) -> Task<anyhow::Result<Value>> {
        let id = Uuid::new_v4();
        let (sender, receiver) = async_channel::bounded(1);
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.len() >= 32 {
                return Task::ready(Err(anyhow::anyhow!("Editor assistance is busy")));
            }
            pending.insert(id, sender);
        }
        let position = text.offset_to_position(offset.min(text.len()));
        let mut params = extra;
        params["position"] = json!(position);
        let result = self.host.lsp_request(
            id,
            self.path.clone(),
            self.version.load(Ordering::Relaxed),
            text.to_string(),
            method.to_owned(),
            params,
        );
        if let Err(error) = result {
            self.pending.lock().unwrap().remove(&id);
            return Task::ready(Err(error));
        }
        let guard = RequestGuard {
            id,
            host: self.host.clone(),
            pending: self.pending.clone(),
        };
        cx.background_executor().spawn(async move {
            let _guard = guard;
            receiver
                .recv()
                .await
                .map_err(|_| anyhow::anyhow!("Editor request cancelled"))?
                .map_err(anyhow::Error::msg)
        })
    }
    pub(super) fn complete(&self, id: Uuid, result: Reply) {
        if let Some(sender) = self.pending.lock().unwrap().remove(&id) {
            let _ = sender.try_send(result);
        }
    }
    fn close(&self) {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        for (id, sender) in pending {
            let _ = self.host.lsp_cancel(id);
            let _ = sender.try_send(Err("Editor file changed".into()));
        }
        let _ = self.host.lsp_close(self.path.clone());
    }
}
impl CompletionProvider for EditorAssist {
    fn is_completion_trigger(&self, _: usize, inserted: &str, _: &mut App) -> bool {
        inserted.chars().count() == 1
            && inserted
                .chars()
                .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '.' | ':'))
    }
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let request = self.request(
            text,
            offset,
            "textDocument/completion",
            json!({"context":trigger}),
            cx,
        );
        cx.background_executor().spawn(async move {
            let value = request.await?;
            if value.is_null() {
                Ok(CompletionResponse::Array(vec![]))
            } else {
                Ok(serde_json::from_value(value)?)
            }
        })
    }
}
impl HoverProvider for EditorAssist {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Option<Hover>>> {
        let request = self.request(text, offset, "textDocument/hover", json!({}), cx);
        cx.background_executor()
            .spawn(async move { Ok(serde_json::from_value(request.await?)?) })
    }
}
impl Workbench {
    pub(super) fn detach_editor_assist(&mut self) {
        if let Some(previous) = self.editor_assist.take() {
            previous.close();
        }
        self.editor_assist_task = None;
    }
    pub(super) fn attach_editor_assist(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.detach_editor_assist();
        let Some(host) = self.host.clone() else {
            return;
        };
        let version = self.editor_clock.fetch_add(1, Ordering::Relaxed) + 1;
        let assist = EditorAssist {
            host,
            path: path.clone(),
            version: self.editor_clock.clone(),
            pending: Arc::default(),
        };
        let provider = Rc::new(assist.clone());
        self.editor.update(cx, |editor, _| {
            editor.lsp_mut().completion_provider = Some(provider.clone());
            editor.lsp_mut().hover_provider = Some(provider);
        });
        let _ = assist
            .host
            .lsp_change(path, version, self.editor.read(cx).value().to_string());
        self.editor_assist = Some(assist);
    }
    pub(super) fn editor_changed(&mut self, cx: &mut Context<Self>) {
        self.editor_dirty = true;
        if self.draft_flush_pending()
            || self.memory_forms_flush_pending()
            || self.document_drafts_flush_pending()
            || self.recovery_flush_pending()
        {
            self.cancel_draft_flush(cx);
            self.cancel_memory_forms_flush(cx);
            self.cancel_document_drafts_flush(cx);
            self.cancel_recovery_flush(cx);
        }
        let Some(assist) = self.editor_assist.clone() else {
            cx.notify();
            return;
        };
        let version = assist.version.fetch_add(1, Ordering::Relaxed) + 1;
        let text = self.editor.read(cx).value().to_string();
        self.editor.update(cx, |editor, _| {
            if let Some(diagnostics) = editor.diagnostics_mut() {
                diagnostics.clear();
            }
        });
        self.editor_assist_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let _ = assist.host.lsp_change(assist.path, version, text);
        }));
        cx.notify();
    }
    pub(super) fn editor_diagnostics(
        &mut self,
        path: PathBuf,
        version: Option<i32>,
        diagnostics: Value,
        cx: &mut Context<Self>,
    ) {
        let Some(assist) = &self.editor_assist else {
            return;
        };
        if assist.path != path
            || version.is_some_and(|version| version != assist.version.load(Ordering::Relaxed))
        {
            return;
        }
        let Ok(items) = serde_json::from_value::<Vec<lsp_types::Diagnostic>>(diagnostics) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            if let Some(diagnostics) = editor.diagnostics_mut() {
                diagnostics.clear();
                diagnostics.extend(items);
            }
            cx.notify();
        });
    }
}
