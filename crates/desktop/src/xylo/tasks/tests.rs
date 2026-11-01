use super::{LocalZone, cadence_label, offset_label, run_label, schedule_expression, shift_days};
use chrono::FixedOffset;

fn zone(east_minutes: i32, name: &str) -> LocalZone {
    LocalZone {
        offset: FixedOffset::east_opt(east_minutes * 60).unwrap(),
        name: name.into(),
    }
}

#[test]
fn local_times_store_as_utc_cron() {
    let utc = zone(0, "UTC");
    assert_eq!(
        schedule_expression(1, "18:45", utc.offset).unwrap(),
        "0 45 18 * * Mon-Fri *"
    );
    assert!(schedule_expression(0, "24:00", utc.offset).is_err());
    assert_eq!(
        schedule_expression(3, "", utc.offset).unwrap(),
        "0 0 * * * * *"
    );
    // India is UTC+05:30: 09:00 local is 03:30 UTC the same day.
    let ist = zone(330, "IST");
    assert_eq!(
        schedule_expression(0, "09:00", ist.offset).unwrap(),
        "0 30 3 * * * *"
    );
    // 02:00 Monday in India is 20:30 Sunday UTC, so the days move back.
    assert_eq!(
        schedule_expression(1, "02:00", ist.offset).unwrap(),
        "0 30 20 * * Sun-Thu *"
    );
    assert_eq!(
        schedule_expression(2, "02:00", ist.offset).unwrap(),
        "0 30 20 * * Sun *"
    );
    // 18:00 Monday in UTC-08:00 is 02:00 Tuesday UTC, so the days move on.
    let pst = zone(-480, "PST");
    assert_eq!(
        schedule_expression(1, "18:00", pst.offset).unwrap(),
        "0 0 2 * * Tue-Sat *"
    );
    // Hourly tasks keep the top of the local hour; only the minute moves.
    assert_eq!(
        schedule_expression(3, "", ist.offset).unwrap(),
        "0 30 * * * * *"
    );
}

#[test]
fn stored_cron_reads_back_in_local_time() {
    for east in [0, 330, -480, 765, -570] {
        let local = zone(east, "LOCAL");
        for (frequency, label) in [
            (0, "every day at 09:05 LOCAL"),
            (1, "weekdays at 09:05 LOCAL"),
            (2, "every monday at 09:05 LOCAL"),
            (3, "every hour at :00"),
        ] {
            let cron = schedule_expression(frequency, "9:05", local.offset).unwrap();
            assert_eq!(cadence_label(&cron, &local), label, "offset {east}");
        }
        for (frequency, label) in [
            (1, "weekdays at 00:15 LOCAL"),
            (2, "every monday at 23:50 LOCAL"),
        ] {
            let time = if frequency == 1 { "00:15" } else { "23:50" };
            let cron = schedule_expression(frequency, time, local.offset).unwrap();
            assert_eq!(cadence_label(&cron, &local), label, "offset {east}");
        }
    }
    assert_eq!(
        cadence_label("0 */5 * * * * *", &zone(0, "UTC")),
        "0 */5 * * * * *"
    );
}

#[test]
fn days_and_offsets_have_plain_labels() {
    assert_eq!(shift_days("Mon-Fri", -1).as_deref(), Some("Sun-Thu"));
    assert_eq!(shift_days("Sat", 1).as_deref(), Some("Sun"));
    assert_eq!(shift_days("*", 1).as_deref(), Some("*"));
    assert_eq!(shift_days("Xyz", 0), None);
    let east = |minutes: i32| FixedOffset::east_opt(minutes * 60).unwrap();
    assert_eq!(offset_label(east(0)), "UTC");
    assert_eq!(offset_label(east(330)), "UTC+05:30");
    assert_eq!(offset_label(east(-570)), "UTC-09:30");
}

#[test]
fn next_runs_use_their_own_offset_and_zone_name() {
    use chrono::TimeZone;
    // Los Angeles leaves daylight saving on 2026-11-01 at 09:00 UTC. A run
    // either side of it shows its own offset and name.
    let before = chrono::Utc
        .with_ymd_and_hms(2026, 10, 31, 16, 0, 0)
        .unwrap();
    let after = chrono::Utc.with_ymd_and_hms(2026, 11, 2, 17, 0, 0).unwrap();
    assert_eq!(run_label(before, &zone(-420, "PDT")), "Oct 31, 09:00 PDT");
    assert_eq!(run_label(after, &zone(-480, "PST")), "Nov 2, 09:00 PST");
}

/// With TZ fixed to Los Angeles, the zone at each instant comes from the
/// system's zone rules: the run after the change reads PST, not today's PDT.
#[cfg(unix)]
#[test]
fn the_zone_at_an_instant_follows_daylight_saving() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "xylo::tasks::tests::zone_at_instant_in_los_angeles",
            "--ignored",
            "--nocapture",
        ])
        .env("TZ", "America/Los_Angeles")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Runs in a child process with TZ set (see the test above); changing TZ in
/// this process would race other tests.
#[cfg(unix)]
#[test]
#[ignore = "run by the_zone_at_an_instant_follows_daylight_saving with TZ set"]
fn zone_at_instant_in_los_angeles() {
    use chrono::TimeZone;
    let before = chrono::Utc
        .with_ymd_and_hms(2026, 10, 31, 16, 0, 0)
        .unwrap();
    let after = chrono::Utc.with_ymd_and_hms(2026, 11, 2, 17, 0, 0).unwrap();
    assert_eq!(
        run_label(before, &LocalZone::at(before)),
        "Oct 31, 09:00 PDT"
    );
    assert_eq!(run_label(after, &LocalZone::at(after)), "Nov 2, 09:00 PST");
}
