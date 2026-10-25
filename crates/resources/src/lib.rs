//! Automatic task admission based on current hardware, memory availability and
//! task demand. There are no user-selected RAM tiers or fixed root-task counts.

pub mod action_lease;
pub mod shared;
pub use action_lease::{ActionLeaseScope, current_action_lease, scope_action};

#[cfg(target_os = "macos")]
mod macos_memory;

use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use sysinfo::System;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

tokio::task_local! {
    static SHARED_LEASE: Arc<shared::SharedResourceLease>;
}

/// Retain a profile-wide reservation across an async native operation. Use this
/// crate's spawn_blocking for admitted native work so cancellation of the outer
/// async future cannot prematurely release the reservation.
pub async fn scope_shared<F: std::future::Future>(
    lease: Arc<shared::SharedResourceLease>,
    future: F,
) -> F::Output {
    SHARED_LEASE.scope(lease, future).await
}

/// Native blocking work inherits the active admission lease by ownership. The
/// operating-system lock is released only after the closure actually finishes,
/// even when its JoinHandle or the enclosing async operation is dropped.
pub fn spawn_blocking<F, R>(function: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let lease = SHARED_LEASE.try_with(Arc::clone).ok();
    let action = current_action_lease();
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        let _action = action.map(ActionLeaseScope::enter);
        function()
    })
}

/// Conservative scheduling estimates for the native operations that allocate
/// significant parse/compiler/guest memory. These are not hard RSS limits.
pub fn capability_demand(name: &str) -> Option<TaskDemand> {
    let memory_mib = match name {
        "document_open"
        | "document_save_copy"
        | "document_export_docx"
        | "document_export_xlsx" => 384,
        "helper_plugin_inspect"
        | "helper_plugin_install"
        | "plugin_run"
        | "plugin_run_installed" => 512,
        _ => return None,
    };
    Some(TaskDemand {
        memory_bytes: memory_mib * 1024 * 1024,
        cpu_share: 1.0,
    })
}

const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct ResourceSnapshot {
    pub total_memory_bytes: u64,
    pub available_memory_bytes: u64,
    pub cpu_threads: usize,
    pub cpu_busy_percent: f32,
}

/// Demand is updated by the subsystem that knows the actual model/workload.
/// Remote inference needs little local CPU; native inference should account for
/// model/KV memory in its backend before asking for admission.
#[derive(Clone, Copy, Debug)]
pub struct TaskDemand {
    pub memory_bytes: u64,
    pub cpu_share: f32,
}

impl TaskDemand {
    pub fn interactive(context_tokens: u32) -> Self {
        Self {
            memory_bytes: 64 * MIB + u64::from(context_tokens) * 2048,
            cpu_share: 0.25,
        }
    }
}

struct State {
    system: System,
    reservations: HashMap<Uuid, TaskDemand>,
}
struct Shared {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Clone)]
pub struct ResourceManager(Arc<Shared>);

impl Default for ResourceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceManager {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_memory();
        system.refresh_cpu_usage();
        Self(Arc::new(Shared {
            state: Mutex::new(State {
                system,
                reservations: HashMap::new(),
            }),
            changed: Notify::new(),
        }))
    }

    pub fn snapshot(&self) -> Result<ResourceSnapshot> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Resource monitor unavailable"))?;
        state.system.refresh_memory();
        state.system.refresh_cpu_usage();
        Ok(snapshot(&state.system))
    }

    /// A queued task wakes when another lease ends or hardware availability changes.
    pub async fn acquire(
        &self,
        demand: TaskDemand,
        cancel: CancellationToken,
    ) -> Result<ResourceLease> {
        ensure!(
            demand.memory_bytes > 0 && demand.cpu_share.is_finite() && demand.cpu_share > 0.,
            "Invalid task resource estimate"
        );
        loop {
            ensure!(!cancel.is_cancelled(), "Task cancelled while queued");
            if let Some(lease) = self.try_acquire(demand)? {
                return Ok(lease);
            }
            tokio::select! {_=self.0.changed.notified()=>{},_=tokio::time::sleep(Duration::from_millis(500))=>{},_=cancel.cancelled()=>{}}
        }
    }

    fn try_acquire(&self, demand: TaskDemand) -> Result<Option<ResourceLease>> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Resource monitor unavailable"))?;
        state.system.refresh_memory();
        state.system.refresh_cpu_usage();
        let hardware = snapshot(&state.system);
        let reserved_memory = state
            .reservations
            .values()
            .map(|d| d.memory_bytes)
            .sum::<u64>();
        let reserved_cpu = state
            .reservations
            .values()
            .map(|d| d.cpu_share)
            .sum::<f32>();
        if !fits(hardware, demand, reserved_memory, reserved_cpu) {
            return Ok(None);
        }
        let id = Uuid::new_v4();
        state.reservations.insert(id, demand);
        Ok(Some(ResourceLease {
            id,
            shared: self.0.clone(),
        }))
    }

    /// Background helper parallelism follows current headroom rather than a RAM profile.
    pub fn suggested_parallelism(&self, per_job_memory: u64) -> Result<usize> {
        let hardware = self.snapshot()?;
        let memory_slots = hardware
            .available_memory_bytes
            .saturating_sub(headroom(hardware.total_memory_bytes))
            / per_job_memory.max(MIB);
        let cpu_slots =
            (hardware.cpu_threads as f32 * (1. - hardware.cpu_busy_percent / 100.)).ceil() as usize;
        Ok((memory_slots as usize).min(cpu_slots.max(1)).max(1))
    }
}

pub struct ResourceLease {
    id: Uuid,
    shared: Arc<Shared>,
}
impl Drop for ResourceLease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.reservations.remove(&self.id);
        }
        self.shared.changed.notify_waiters();
    }
}

fn snapshot(system: &System) -> ResourceSnapshot {
    #[cfg(target_os = "macos")]
    let available_memory_bytes =
        macos_memory::available_memory(system.total_memory(), system.available_memory());
    #[cfg(not(target_os = "macos"))]
    let available_memory_bytes = system.available_memory();

    ResourceSnapshot {
        total_memory_bytes: system.total_memory(),
        available_memory_bytes,
        cpu_threads: system.cpus().len().max(
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1),
        ),
        cpu_busy_percent: system.global_cpu_usage().clamp(0., 100.),
    }
}

fn headroom(total: u64) -> u64 {
    (total / 20).max(256 * MIB)
}

fn fits(
    hardware: ResourceSnapshot,
    demand: TaskDemand,
    reserved_memory: u64,
    reserved_cpu: f32,
) -> bool {
    let available = hardware
        .available_memory_bytes
        .saturating_sub(headroom(hardware.total_memory_bytes));
    let cpu_headroom =
        (hardware.cpu_threads as f32 * (1. - hardware.cpu_busy_percent / 100.)).max(1.);
    demand.memory_bytes <= available.saturating_sub(reserved_memory)
        && reserved_cpu + demand.cpu_share <= cpu_headroom
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_pressure_changes_admission_on_the_same_machine() {
        let mut hardware = ResourceSnapshot {
            total_memory_bytes: 32 * 1024 * MIB,
            available_memory_bytes: 20 * 1024 * MIB,
            cpu_threads: 16,
            cpu_busy_percent: 10.,
        };
        let demand = TaskDemand {
            memory_bytes: 1024 * MIB,
            cpu_share: 2.,
        };
        assert!(fits(hardware, demand, 0, 0.));
        hardware.available_memory_bytes = 512 * MIB;
        assert!(!fits(hardware, demand, 0, 0.));
        hardware.available_memory_bytes = 20 * 1024 * MIB;
        hardware.cpu_busy_percent = 99.;
        assert!(!fits(hardware, demand, 0, 0.));
    }
    #[test]
    fn reservations_prevent_oversubscription_without_ram_tiers() {
        let hardware = ResourceSnapshot {
            total_memory_bytes: 16 * 1024 * MIB,
            available_memory_bytes: 4 * 1024 * MIB,
            cpu_threads: 8,
            cpu_busy_percent: 0.,
        };
        let demand = TaskDemand {
            memory_bytes: 2 * 1024 * MIB,
            cpu_share: 1.,
        };
        assert!(fits(hardware, demand, 0, 0.));
        assert!(!fits(hardware, demand, 2 * 1024 * MIB, 1.));
    }
}
