use chrono::{TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::mail_ledger::{
    MailLedgerFilter, MailLedgerRecord, MailLedgerRepository,
};
use invoice_reimbursement::domain::model::MailOutcome;
use uuid::Uuid;

fn received(minutes: i64) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 10, 9, 0, 0).unwrap() + chrono::Duration::minutes(minutes)
}

fn record(
    uid: u32,
    imported: u32,
    existing: u32,
    failed: u32,
    candidates: u32,
) -> MailLedgerRecord {
    MailLedgerRecord {
        account_id: Uuid::from_u128(1),
        mailbox: "INBOX".to_owned(),
        uid_validity: 7,
        uid,
        message_id: Some(format!("<mail-{uid}@example.com>")),
        subject: Some(format!("发票 {uid}")),
        sender: Some("billing@example.com".to_owned()),
        received_at: received(i64::from(uid)),
        candidate_count: candidates,
        imported_count: imported,
        existing_count: existing,
        failed_count: failed,
        reason: (failed != 0).then(|| "link_download_failed".to_owned()),
    }
}

#[tokio::test]
async fn ledger_derives_outcomes_and_reports_what_needs_attention() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let ledger = MailLedgerRepository::new(pool.clone());

    // A mail whose invoices all landed.
    ledger
        .record(&record(1, 2, 0, 0, 2))
        .await
        .expect("imported mail should record");
    // Already in the library: still settled.
    ledger
        .record(&record(2, 0, 1, 0, 1))
        .await
        .expect("already imported mail should record");
    // One of two links failed.
    ledger
        .record(&record(3, 1, 0, 1, 2))
        .await
        .expect("partial mail should record");
    // Nothing could be fetched.
    ledger
        .record(&record(4, 0, 0, 1, 1))
        .await
        .expect("failed mail should record");
    // Ordinary correspondence carries no invoice clues.
    ledger
        .record(&record(5, 0, 0, 0, 0))
        .await
        .expect("ignored mail should record");

    let counts = ledger.counts().await.unwrap();
    assert_eq!(counts.imported, 2);
    assert_eq!(counts.partial, 1);
    assert_eq!(counts.failed, 1);
    assert_eq!(counts.ignored, 1);
    assert_eq!(counts.needs_attention(), 2);

    let attention = ledger
        .list_page(
            &MailLedgerFilter {
                needs_attention: true,
                ..MailLedgerFilter::default()
            },
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        attention
            .entries
            .iter()
            .map(|entry| (entry.uid, entry.outcome))
            .collect::<Vec<_>>(),
        vec![(4, MailOutcome::Failed), (3, MailOutcome::Partial),]
    );
    assert_eq!(
        attention.entries[1].reason.as_deref(),
        Some("link_download_failed")
    );
    assert!(!attention.entries[0].marked_seen);
}

#[tokio::test]
async fn ledger_pages_newest_first_and_filters_by_outcome_and_search() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let ledger = MailLedgerRepository::new(pool.clone());
    for uid in 1..=5 {
        ledger
            .record(&record(uid, 1, 0, 0, 1))
            .await
            .expect("mail should record");
    }
    ledger
        .record(&MailLedgerRecord {
            subject: Some("与众不同的主题".to_owned()),
            ..record(6, 0, 0, 1, 1)
        })
        .await
        .expect("failed mail should record");

    let first = ledger
        .list_page(&MailLedgerFilter::default(), None, 2)
        .await
        .unwrap();
    assert_eq!(
        first
            .entries
            .iter()
            .map(|entry| entry.uid)
            .collect::<Vec<_>>(),
        vec![6, 5]
    );
    let cursor = first.next_cursor.expect("page should continue");
    let second = ledger
        .list_page(&MailLedgerFilter::default(), Some(cursor), 2)
        .await
        .unwrap();
    assert_eq!(
        second
            .entries
            .iter()
            .map(|entry| entry.uid)
            .collect::<Vec<_>>(),
        vec![4, 3]
    );

    let failed = ledger
        .list_page(
            &MailLedgerFilter {
                outcome: Some(MailOutcome::Failed),
                ..MailLedgerFilter::default()
            },
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(failed.entries.len(), 1);

    let searched = ledger
        .list_page(
            &MailLedgerFilter {
                query: Some("与众不同".to_owned()),
                ..MailLedgerFilter::default()
            },
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        searched
            .entries
            .iter()
            .map(|entry| entry.uid)
            .collect::<Vec<_>>(),
        vec![6]
    );
}

#[tokio::test]
async fn recording_a_rescan_updates_the_row_without_losing_the_seen_flag() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let ledger = MailLedgerRepository::new(pool.clone());
    ledger.record(&record(9, 1, 0, 0, 1)).await.unwrap();
    assert!(
        !ledger
            .list_page(&MailLedgerFilter::default(), None, 1)
            .await
            .unwrap()
            .entries[0]
            .marked_seen
    );

    ledger
        .mark_seen(Uuid::from_u128(1), "INBOX", 7, 9)
        .await
        .expect("seen flag should record");

    // A later range scan reports the same mail again, now with a failed link.
    ledger.record(&record(9, 1, 0, 1, 2)).await.unwrap();

    let entry = ledger
        .list_page(&MailLedgerFilter::default(), None, 1)
        .await
        .unwrap()
        .entries
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(entry.outcome, MailOutcome::Partial);
    assert_eq!(entry.failed_count, 1);
    assert!(
        entry.marked_seen,
        "the recorded seen flag must survive a rescan"
    );
}
