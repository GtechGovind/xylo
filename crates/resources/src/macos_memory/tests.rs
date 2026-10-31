use super::*;

fn pages() -> PageCounts {
    PageCounts {
        free: 106_753,
        inactive: 582_235,
        page_size: 16_384,
    }
}

#[test]
fn physical_free_and_inactive_are_counted_once() {
    let pages = pages();
    let actual = select_available(32 << 30, 2 << 30, Some(pages), Pressure::Normal);
    assert_eq!(actual, 11_288_379_392);
    // The compressor and purgeable counters have no role in this sum.
    let old_estimate = (pages.free + pages.inactive + 6_430 - 522_936) * pages.page_size;
    assert_eq!(old_estimate, 2_825_945_088);
    assert!(actual > old_estimate);
}

#[test]
fn native_estimate_is_clamped_without_arithmetic_overflow() {
    let pages = PageCounts {
        free: u64::MAX,
        inactive: u64::MAX,
        page_size: 16_384,
    };
    assert_eq!(
        select_available(32 << 30, 2 << 30, Some(pages), Pressure::Normal),
        32 << 30
    );
    assert_eq!(
        select_available(0, u64::MAX, Some(pages), Pressure::Normal),
        0
    );
}

#[test]
fn missing_or_invalid_native_counters_keep_bounded_fallback() {
    assert_eq!(select_available(32 << 30, 123, None, Pressure::Normal), 123);
    for page_size in [0, 3] {
        let pages = PageCounts {
            page_size,
            ..pages()
        };
        assert_eq!(
            select_available(32 << 30, 123, Some(pages), Pressure::Normal),
            123
        );
    }
    assert_eq!(select_available(50, 100, None, Pressure::Normal), 50);
}

#[test]
fn elevated_unknown_or_unavailable_pressure_never_broadens_admission() {
    for raw in [Some(0), Some(2), Some(4), Some(6), Some(u32::MAX), None] {
        assert_eq!(
            select_available(32 << 30, 123, Some(pages()), Pressure::from_raw(raw)),
            123,
            "pressure={raw:?}"
        );
    }
    assert_eq!(Pressure::from_raw(Some(1)), Pressure::Normal);
}

#[test]
fn native_estimate_retains_headroom_and_existing_reservations() {
    let hardware = crate::ResourceSnapshot {
        total_memory_bytes: 32 << 30,
        available_memory_bytes: select_available(
            32 << 30,
            2 << 30,
            Some(pages()),
            Pressure::Normal,
        ),
        cpu_threads: 10,
        cpu_busy_percent: 0.,
    };
    let demand = crate::TaskDemand {
        memory_bytes: 8 << 30,
        cpu_share: 1.,
    };
    assert!(crate::fits(hardware, demand, 0, 0.));
    assert!(!crate::fits(hardware, demand, 2 << 30, 0.));
    assert!(!crate::fits(
        hardware,
        crate::TaskDemand {
            memory_bytes: 10 << 30,
            ..demand
        },
        0,
        0.
    ));
}

#[test]
fn live_counter_comparison() {
    let mut system = sysinfo::System::new().unwrap();
    system.refresh_memory();
    let total = system.total_memory();
    let fallback = system.available_memory();
    let (statistics, page_size) = read_vm_statistics().expect("Public Mach VM counters");
    let pages = PageCounts {
        free: u64::from(statistics.free_count),
        inactive: u64::from(statistics.inactive_count),
        page_size,
    };
    let pressure_raw = read_pressure_raw();
    let pressure = Pressure::from_raw(pressure_raw);
    let selected = select_available(total, fallback, Some(pages), pressure);
    let purgeable = u64::from(statistics.purgeable_count);
    let speculative = u64::from(statistics.speculative_count);
    let compressor = u64::from(statistics.compressor_page_count);
    let same_counter_old_estimate = pages
        .free
        .saturating_add(pages.inactive)
        .saturating_add(purgeable)
        .saturating_sub(compressor)
        .saturating_mul(page_size);
    eprintln!(
        "macOS VM comparison: total={total}, page_size={page_size}, free={}, inactive={}, purgeable={purgeable}, speculative={speculative}, compressor={compressor}, pressure_raw={pressure_raw:?}, pressure={pressure:?}, sysinfo_fallback={fallback}, old_formula_same_counters={same_counter_old_estimate}, native_free_plus_inactive={}, selected={selected}, fallback_used={}",
        pages.free,
        pages.inactive,
        pages
            .free
            .saturating_add(pages.inactive)
            .saturating_mul(page_size),
        pressure != Pressure::Normal,
    );
    assert!(selected <= total);
    if pressure != Pressure::Normal {
        assert_eq!(selected, fallback.min(total));
    }
}
