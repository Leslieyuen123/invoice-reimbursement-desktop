//! The inbox-level "批量确认可推导的票据" action: every pending invoice whose
//! missing values can be derived from its own data is confirmed, everything
//! else is skipped with a stable reason, and re-running it is a no-op.

use chrono::{NaiveDate, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::items::{ItemRepository, NewItemRecord};
use invoice_reimbursement::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, RecognitionStatus, SourceType,
};
use invoice_reimbursement::services::batches::{BatchService, RecoverableSettleInput};
use sqlx::SqlitePool;
use uuid::Uuid;

fn invoice(id: Uuid, name: &str) -> NewItemRecord {
    let now = Utc::now();
    NewItemRecord {
        id,
        original_name: name.to_owned(),
        original_path: format!("/tmp/{id}-{name}"),
        normalized_pdf_path: Some(format!("/tmp/{id}.pdf")),
        sha256: format!("sha256-{id}"),
        mime_type: "application/pdf".to_owned(),
        source_type: SourceType::ManualUpload,
        source_account_id: None,
        source_mailbox: None,
        source_uid_validity: None,
        source_uid: None,
        source_message_id: None,
        source_part_id: None,
        fetched_at: now,
        source_received_date: None,
        invoice_date: Some(NaiveDate::from_ymd_opt(2026, 6, 18).unwrap()),
        suggested_period: Some("2026-06".to_owned()),
        batch_id: None,
        suggested_category: None,
        final_category: None,
        amount_cents: Some(12_850),
        currency: "CNY".to_owned(),
        city: None,
        company: None,
        recognition_status: RecognitionStatus::Succeeded,
        confirmation_status: ConfirmationStatus::Pending,
        dedupe_status: DedupeStatus::Unique,
        duplicate_of_id: None,
        note: None,
        event_tag: None,
        project_tag: None,
        created_at: now,
        updated_at: now,
    }
}

async fn seed(pool: &SqlitePool) -> Seed {
    let repository = ItemRepository::new(pool.clone());
    let seed = Seed {
        only_category_missing: Uuid::new_v4(),
        uses_suggested_category: Uuid::new_v4(),
        missing_amount: Uuid::new_v4(),
        missing_invoice_date: Uuid::new_v4(),
        unnormalizable: Uuid::new_v4(),
        already_confirmed: Uuid::new_v4(),
    };

    let mut only_category = invoice(seed.only_category_missing, "only-category.pdf");
    // No category and no suggestion: the user's standing rule files it as 其他.
    only_category.suggested_category = None;
    only_category.final_category = None;
    only_category.suggested_period = None;
    repository.insert(&only_category).await.unwrap();

    let mut suggested = invoice(seed.uses_suggested_category, "suggested.pdf");
    suggested.suggested_category = Some(Category::Dining);
    suggested.final_category = None;
    repository.insert(&suggested).await.unwrap();

    let mut without_amount = invoice(seed.missing_amount, "no-amount.pdf");
    without_amount.amount_cents = None;
    repository.insert(&without_amount).await.unwrap();

    let mut without_date = invoice(seed.missing_invoice_date, "no-date.pdf");
    without_date.invoice_date = None;
    without_date.suggested_period = None;
    without_date.suggested_category = Some(Category::Transport);
    repository.insert(&without_date).await.unwrap();

    let mut zipped = invoice(seed.unnormalizable, "placeholder.zip");
    zipped.normalized_pdf_path = None;
    repository.insert(&zipped).await.unwrap();

    let mut confirmed = invoice(seed.already_confirmed, "already-confirmed.pdf");
    confirmed.final_category = Some(Category::Other);
    confirmed.confirmation_status = ConfirmationStatus::Confirmed;
    repository.insert(&confirmed).await.unwrap();

    seed
}

struct Seed {
    only_category_missing: Uuid,
    uses_suggested_category: Uuid,
    missing_amount: Uuid,
    missing_invoice_date: Uuid,
    unnormalizable: Uuid,
    already_confirmed: Uuid,
}

#[tokio::test]
async fn settles_derivable_invoices_and_reports_why_the_rest_were_skipped() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let seed = seed(&pool).await;
    let repository = ItemRepository::new(pool.clone());
    let confirmed_before = repository.get_by_id(seed.already_confirmed).await.unwrap();

    let outcome = BatchService::new(pool.clone())
        .settle_recoverable_items(RecoverableSettleInput::default())
        .await
        .unwrap();

    assert_eq!(outcome.confirmed_count, 2);
    assert_eq!(outcome.confirmed_item_ids.len(), 2);
    assert_eq!(outcome.filled_invoice_date_count, 0);
    assert_eq!(outcome.applied_category_count, 2);
    let skipped_codes = outcome
        .skipped
        .iter()
        .map(|skipped| skipped.code.as_str())
        .collect::<Vec<_>>();
    assert_eq!(skipped_codes.len(), 3);
    assert!(skipped_codes.contains(&"missing_amount"));
    assert!(skipped_codes.contains(&"missing_invoice_date"));
    assert!(skipped_codes.contains(&"missing_normalized_pdf"));

    let fallback = repository
        .get_by_id(seed.only_category_missing)
        .await
        .unwrap();
    assert_eq!(fallback.final_category, Some(Category::Other));
    assert_eq!(fallback.confirmation_status, ConfirmationStatus::Confirmed);
    assert_eq!(fallback.suggested_period.as_deref(), Some("2026-06"));
    assert_eq!(
        fallback.status(),
        invoice_reimbursement::domain::model::ItemStatus::Ready
    );

    let suggested = repository
        .get_by_id(seed.uses_suggested_category)
        .await
        .unwrap();
    assert_eq!(suggested.final_category, Some(Category::Dining));
    assert_eq!(suggested.confirmation_status, ConfirmationStatus::Confirmed);

    for skipped in [
        seed.missing_amount,
        seed.missing_invoice_date,
        seed.unnormalizable,
    ] {
        let item = repository.get_by_id(skipped).await.unwrap();
        assert_eq!(item.confirmation_status, ConfirmationStatus::Pending);
        assert_eq!(item.final_category, None);
    }

    let confirmed_after = repository.get_by_id(seed.already_confirmed).await.unwrap();
    assert_eq!(confirmed_after.updated_at, confirmed_before.updated_at);

    // Re-running is idempotent: the two confirmed invoices leave the queue and
    // only the genuinely un-derivable ones are reported again.
    let second = BatchService::new(pool.clone())
        .settle_recoverable_items(RecoverableSettleInput::default())
        .await
        .unwrap();
    assert_eq!(second.confirmed_count, 0);
    assert_eq!(second.applied_category_count, 0);
    assert_eq!(
        second
            .skipped
            .iter()
            .map(|skipped| skipped.code.as_str())
            .collect::<Vec<_>>()
            .len(),
        3
    );
}
