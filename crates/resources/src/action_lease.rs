//! In-process execution ownership follows late blocking jobs. This context does
//! not qualify native subprocess lifetimes: clients can close inherited handles.
use std::{cell::RefCell, fs::File, marker::PhantomData, rc::Rc, sync::Arc};
tokio::task_local! {static ACTION:Arc<File>;}
thread_local! {static BLOCKING:RefCell<Option<Arc<File>>>=const{RefCell::new(None)};}
pub fn current_action_lease() -> Option<Arc<File>> {
    ACTION
        .try_with(Arc::clone)
        .ok()
        .or_else(|| BLOCKING.with(|value| value.borrow().clone()))
}
pub async fn scope_action<F: std::future::Future>(lease: Arc<File>, future: F) -> F::Output {
    ACTION.scope(lease, future).await
}
/// A synchronous broker dispatch remains on its blocking thread. Native async
/// work receives the same context with scope_action/spawn_blocking.
pub struct ActionLeaseScope(Option<Arc<File>>, PhantomData<Rc<()>>);
impl ActionLeaseScope {
    pub fn enter(lease: Arc<File>) -> Self {
        Self(
            BLOCKING.with(|value| value.replace(Some(lease))),
            PhantomData,
        )
    }
}
impl Drop for ActionLeaseScope {
    fn drop(&mut self) {
        BLOCKING.with(|value| {
            value.replace(self.0.take());
        });
    }
}
