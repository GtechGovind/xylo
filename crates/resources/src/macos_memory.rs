//! Physical reclaimable-page estimate for macOS. The pinned sysinfo estimate
//! subtracts compressed-pager pages from free + inactive a second time. These
//! Mach counters already describe distinct physical page categories.

use std::mem::{MaybeUninit, offset_of, size_of};

#[derive(Clone, Copy, Debug)]
struct PageCounts {
    free: u64,
    inactive: u64,
    page_size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pressure {
    Normal,
    Other(u32),
    Unavailable,
}

impl Pressure {
    fn from_raw(value: Option<u32>) -> Self {
        // XNU's read-only sysctl converts its internal pressure level into the
        // public DISPATCH_MEMORYPRESSURE_* flags: 1 normal, 2 warn, 4 critical.
        // Only an exact normal result permits the corrected estimate.
        match value {
            Some(1) => Self::Normal,
            Some(value) => Self::Other(value),
            None => Self::Unavailable,
        }
    }
}

pub(super) fn available_memory(total: u64, fallback: u64) -> u64 {
    let pages = read_vm_statistics().map(|(statistics, page_size)| PageCounts {
        free: u64::from(statistics.free_count),
        inactive: u64::from(statistics.inactive_count),
        page_size,
    });
    // Sample pressure after the counters, so an observed elevated level cannot
    // be replaced by an earlier normal sample from this collection.
    select_available(total, fallback, pages, read_pressure())
}

fn select_available(
    total: u64,
    fallback: u64,
    pages: Option<PageCounts>,
    pressure: Pressure,
) -> u64 {
    let Some(pages) = pages.filter(|pages| pages.page_size.is_power_of_two()) else {
        return fallback.min(total);
    };
    if pressure != Pressure::Normal {
        return fallback.min(total);
    }
    // Speculative pages are included in free_count. Purgeable pages may overlap
    // other categories. Neither is added; compressor pages are not subtracted.
    pages
        .free
        .saturating_add(pages.inactive)
        .saturating_mul(pages.page_size)
        .min(total)
}

struct HostPort(libc::mach_port_t);

impl HostPort {
    fn new() -> Option<Self> {
        // SAFETY: this public Mach API takes no arguments and returns an owned
        // send right in this task. The guard releases it on every exit path.
        // libc deprecates its Mach bindings in favor of a separate crate; the
        // operating-system API remains public in mach/mach_init.h.
        #[allow(deprecated)]
        let port = unsafe { libc::mach_host_self() };
        (port != libc::MACH_PORT_NULL as libc::mach_port_t).then_some(Self(port))
    }
}

impl Drop for HostPort {
    fn drop(&mut self) {
        // SAFETY: the guard owns exactly the send right obtained by
        // mach_host_self, and deallocates it in the current task only once.
        #[allow(deprecated)] // Public SDK macro, exposed as a libc wrapper.
        unsafe {
            mach_port_deallocate(libc::mach_task_self(), self.0);
        }
    }
}

// Declared in the macOS SDK's public mach/mach_port.h; libc exposes the related
// port/task types and functions but not this particular routine.
unsafe extern "C" {
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

fn read_vm_statistics() -> Option<(libc::vm_statistics64, u64)> {
    let port = HostPort::new()?;
    // SAFETY: sysconf accepts this public constant and writes no caller memory.
    let page_size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    if !page_size.is_power_of_two() {
        return None;
    }
    let mut statistics = MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: the output is aligned, zero initialized and has exactly the
    // capacity described by count, expressed in Mach integer_t units.
    let result = unsafe {
        libc::host_statistics64(
            port.0,
            libc::HOST_VM_INFO64,
            statistics.as_mut_ptr().cast(),
            &mut count,
        )
    };
    // The SDK's HOST_VM_INFO64_REV1_COUNT ends before swapped_count. It includes
    // all counters used here, including the diagnostic compressed-pager count.
    let minimum_count =
        offset_of!(libc::vm_statistics64, swapped_count) / size_of::<libc::integer_t>();
    if result != libc::KERN_SUCCESS || (count as usize) < minimum_count {
        return None;
    }
    // SAFETY: all fields are integer scalars, initially zero, and a successful
    // call supplied at least the revision containing every counter we read.
    Some((unsafe { statistics.assume_init() }, page_size))
}

fn read_pressure() -> Pressure {
    Pressure::from_raw(read_pressure_raw())
}

fn read_pressure_raw() -> Option<u32> {
    let mut level = 0_u32;
    let mut length = size_of::<u32>();
    // SAFETY: the name is a NUL-terminated static string, the output pointer and
    // its capacity agree, and null newp with zero newlen makes this read-only.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.memorystatus_vm_pressure_level".as_ptr(),
            std::ptr::from_mut(&mut level).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && length == size_of::<u32>()).then_some(level)
}

#[cfg(test)]
mod tests;
