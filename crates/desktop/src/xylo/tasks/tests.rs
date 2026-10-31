use super::{cadence_label, schedule_expression};
#[test]
fn schedules_use_valid_utc_fields() {
    assert_eq!(
        schedule_expression(1, "18:45").unwrap(),
        "0 45 18 * * Mon-Fri *"
    );
    assert!(schedule_expression(0, "24:00").is_err());
    assert_eq!(schedule_expression(3, "").unwrap(), "0 0 * * * * *");
}
#[test]
fn cadence_reads_back_every_expression_the_form_makes() {
    for (frequency, label) in [
        (0, "every day at 09:05"),
        (1, "weekdays at 09:05"),
        (2, "every monday at 09:05"),
        (3, "every hour"),
    ] {
        let cron = schedule_expression(frequency, "9:05").unwrap();
        assert_eq!(cadence_label(&cron), label);
    }
    assert_eq!(cadence_label("0 */5 * * * * *"), "0 */5 * * * * *");
}
