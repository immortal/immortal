use std::{error::Error, time::Duration};

use tokio::time::Instant as TokioInstant;

use super::{OVERDUE_REPORT_INTERVAL, OverdueReports, keep_waiting};

#[test]
fn overdue_reports_are_rate_limited() {
    let mut reports = OverdueReports::default();
    let start = TokioInstant::now();
    let interval = Duration::from_mins(1);

    assert!(reports.report("the service start", start, interval));
    assert!(!reports.report(
        "the service start",
        start + Duration::from_secs(30),
        interval
    ));
    // The limit is shared: a different overdue wait inside the same interval
    // stays quiet too, so a stalled broker cannot flood standard error.
    assert!(!reports.report(
        "the end of the killed service",
        start + Duration::from_secs(59),
        interval
    ));
    assert!(reports.report(
        "the service start",
        start + Duration::from_mins(1),
        interval
    ));
}

#[test]
fn keeping_waiting_moves_the_deadline_into_the_future() -> Result<(), Box<dyn Error>> {
    let mut reports = OverdueReports::default();
    let mut deadline = Some(TokioInstant::now());
    let before = TokioInstant::now();

    keep_waiting(&mut reports, &mut deadline, "the service start");

    let rearmed = deadline.ok_or("an overdue wait must stay armed")?;
    assert!(rearmed >= before + OVERDUE_REPORT_INTERVAL);
    Ok(())
}
