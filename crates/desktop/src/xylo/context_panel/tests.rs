use super::{ContextBreakdown, updated_label};
use xylo_core::ContextUsage;

fn usage() -> ContextUsage {
    ContextUsage {
        capacity: 200_000,
        output_reserve: 8_000,
        system: 6_000,
        tools: 9_000,
        tool_count: 18,
        deferred_tool_count: 40,
        messages: 30_000,
        summary: 5_000,
        compacted: true,
        autocompact_threshold: 160_000,
        provider_input: Some(51_000),
    }
}

#[test]
fn used_is_the_request_composition_not_summed_turn_usage() {
    let breakdown = ContextBreakdown::new(&usage());
    assert_eq!(breakdown.used, 50_000);
    assert_eq!(breakdown.buffer, 40_000);
    assert_eq!(breakdown.free, 110_000);
    assert_eq!(breakdown.percent(), Some(25));
    assert_eq!(breakdown.percent_label(), "25%");
}

#[test]
fn percent_is_bounded_and_unknown_capacity_has_no_ring() {
    let mut full = usage();
    full.messages = 400_000;
    let breakdown = ContextBreakdown::new(&full);
    assert_eq!(breakdown.percent(), Some(100));
    assert_eq!(breakdown.free, 0);
    let tiny = ContextBreakdown::new(&ContextUsage {
        capacity: 1_000_000,
        messages: 10,
        ..ContextUsage::default()
    });
    assert_eq!(tiny.percent_label(), "<1%");
    let unknown = ContextBreakdown::new(&ContextUsage {
        messages: 10,
        ..ContextUsage::default()
    });
    assert_eq!(unknown.percent(), None);
    assert_eq!(unknown.fraction(10), 0.);
}

#[test]
fn freshness_reads_as_relative_time() {
    assert_eq!(updated_label(10_000, 10_500), "just now");
    assert_eq!(updated_label(0, 180_000), "3 min ago");
    assert_eq!(updated_label(0, 7_200_000), "2 h ago");
}

#[test]
fn the_ring_appears_only_once_a_chat_uses_five_percent() {
    let used = |messages: u64| {
        ContextBreakdown::new(&ContextUsage {
            capacity: 100_000,
            messages,
            ..ContextUsage::default()
        })
        .percent()
        .filter(|percent| *percent >= super::RING_THRESHOLD)
    };
    assert_eq!(used(4_999), None);
    assert_eq!(used(5_000), Some(5));
    assert_eq!(used(17_000), Some(17));
}
