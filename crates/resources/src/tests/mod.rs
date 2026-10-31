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
