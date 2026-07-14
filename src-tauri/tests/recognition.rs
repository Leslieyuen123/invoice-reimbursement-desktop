use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{NaiveDate, TimeZone, Utc};
use invoice_reimbursement::db;
use invoice_reimbursement::db::accounts::{
    MailboxAccountRepository, MailboxProvider, NewMailboxAccount,
};
use invoice_reimbursement::db::batches::BatchRepository;
use invoice_reimbursement::db::items::{ItemPatch, ItemRepository, NewItemRecord};
use invoice_reimbursement::domain::error::AppError;
use invoice_reimbursement::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, ItemStatus, NewBatch, RecognitionStatus, SourceType,
};
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::services::recognition::{
    RecognitionService, recognize, recognize_with_warnings,
};
use uuid::Uuid;

struct FakeExtractor {
    results: Mutex<VecDeque<Result<ExtractedDocument, AppError>>>,
    paths: Mutex<Vec<PathBuf>>,
}

impl FakeExtractor {
    fn new(results: Vec<Result<ExtractedDocument, AppError>>) -> Self {
        Self {
            results: Mutex::new(results.into()),
            paths: Mutex::new(Vec::new()),
        }
    }

    fn paths(&self) -> Vec<PathBuf> {
        self.paths.lock().unwrap().clone()
    }
}

impl DocumentExtractor for FakeExtractor {
    fn extract(&self, path: &Path) -> Result<ExtractedDocument, AppError> {
        self.paths.lock().unwrap().push(path.to_path_buf());
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .expect("fake extractor response")
    }
}

struct GatedExtractor {
    started: SyncSender<()>,
    proceed: Mutex<Receiver<()>>,
    result: Mutex<Option<ExtractedDocument>>,
}

impl DocumentExtractor for GatedExtractor {
    fn extract(&self, _path: &Path) -> Result<ExtractedDocument, AppError> {
        self.started.send(()).unwrap();
        self.proceed.lock().unwrap().recv().unwrap();
        Ok(self.result.lock().unwrap().take().unwrap())
    }
}

#[test]
fn recognizes_labeled_invoice_metadata() {
    let recognized = recognize(
        "开票日期：2026年06月18日 餐饮服务 价税合计 ¥128.50",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(
        recognized.invoice_date,
        NaiveDate::from_ymd_opt(2026, 6, 18)
    );
    assert_eq!(recognized.suggested_period, "2026-06");
    assert_eq!(recognized.category, Some(Category::Dining));
    assert_eq!(recognized.amount_cents, Some(12_850));
}

#[test]
fn incomplete_metadata_falls_back_to_received_month_and_needs_confirmation() {
    let recognized = recognize("电子票据", NaiveDate::from_ymd_opt(2026, 7, 2).unwrap());

    assert_eq!(recognized.invoice_date, NaiveDate::from_ymd_opt(2026, 7, 2));
    assert_eq!(recognized.suggested_period, "2026-07");
    assert_eq!(recognized.category, None);
    assert_eq!(recognized.status(), ItemStatus::PendingConfirmation);
}

#[test]
fn invoice_date_label_has_priority_and_accepts_iso_dates() {
    let recognized = recognize(
        "日期：2026年05月01日 开票日期：2026-06-18",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(
        recognized.invoice_date,
        NaiveDate::from_ymd_opt(2026, 6, 18)
    );
    assert_eq!(recognized.suggested_period, "2026-06");
}

#[test]
fn generic_date_label_is_used_when_invoice_date_label_is_absent() {
    let recognized = recognize(
        "电子发票 日期: 2026年06月19日",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(
        recognized.invoice_date,
        NaiveDate::from_ymd_opt(2026, 6, 19)
    );
}

#[test]
fn invalid_calendar_date_falls_back_and_blocks_auto_confirmation() {
    let recognized = recognize(
        "日期：2026-06-18 开票日期：2026-02-31 餐饮服务 价税合计 ¥128.50",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.invoice_date, NaiveDate::from_ymd_opt(2026, 7, 2));
    assert_eq!(recognized.warnings, ["invalid_invoice_date"]);
    assert_eq!(recognized.status(), ItemStatus::PendingConfirmation);
}

#[test]
fn amount_accepts_rmb_marker_whitespace_and_thousands_grouping() {
    let recognized = recognize(
        "价税合计（小写）： RMB 1,234.56",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.amount_cents, Some(123_456));
}

#[test]
fn amount_uses_label_priority_instead_of_text_order_or_tax_amount() {
    let recognized = recognize(
        "税额 ￥9.99 合计 ￥100.00 价税合计 ￥200.00 价税合计（小写） ￥1,234.56",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.amount_cents, Some(123_456));
}

#[test]
fn invalid_amount_formats_negative_values_and_overflow_are_rejected() {
    for text in [
        "价税合计 ￥1,23.45",
        "价税合计 ￥12.345",
        "价税合计 ￥-1.00",
        "价税合计 ￥92233720368547758.08",
    ] {
        let recognized = recognize(text, NaiveDate::from_ymd_opt(2026, 7, 2).unwrap());

        assert_eq!(recognized.amount_cents, None, "accepted {text}");
        assert_eq!(recognized.warnings, ["invalid_amount"], "{text}");
    }
}

#[test]
fn category_scoring_supports_all_four_keyword_sets() {
    for (text, expected) in [
        ("出租车 网约车", Category::Transport),
        ("餐饮 食品", Category::Dining),
        ("住宿 酒店", Category::Accommodation),
        ("招待 礼品", Category::Hospitality),
    ] {
        let recognized = recognize(text, NaiveDate::from_ymd_opt(2026, 7, 2).unwrap());

        assert_eq!(recognized.category, Some(expected), "{text}");
    }
}

#[test]
fn tied_category_scores_are_left_unclassified() {
    let recognized = recognize(
        "出租车 网约车 餐饮 食品",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.category, None);
}

#[test]
fn a_single_or_repeated_keyword_does_not_reach_the_category_threshold() {
    for text in ["酒店", "出租车 出租车 出租车"] {
        let recognized = recognize(text, NaiveDate::from_ymd_opt(2026, 7, 2).unwrap());

        assert_eq!(recognized.category, None, "{text}");
    }
}

#[test]
fn company_prefers_the_buyer_name_segment_over_the_seller() {
    let recognized = recognize(
        "销售方名称：北京远方服务有限公司\n购买方名称：上海星河科技有限公司\n购买方税号：123",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.company.as_deref(), Some("上海星河科技有限公司"));
}

#[test]
fn company_falls_back_to_the_seller_name_segment() {
    let recognized = recognize(
        "销售方名称：北京远方服务有限公司\n销售方税号：123",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.company.as_deref(), Some("北京远方服务有限公司"));
}

#[test]
fn city_dictionary_prefers_explicit_and_longest_city_names() {
    for (text, expected) in [
        ("购买方地址：北京市朝阳区", "北京"),
        ("销售方地址：上海市浦东新区", "上海"),
        ("项目地点：广东省广州市天河区", "广州"),
        ("服务地点：广东省深圳市南山区", "深圳"),
    ] {
        let recognized = recognize(text, NaiveDate::from_ymd_opt(2026, 7, 2).unwrap());

        assert_eq!(recognized.city.as_deref(), Some(expected), "{text}");
    }
}

#[test]
fn city_is_absent_when_no_dictionary_entry_matches() {
    let recognized = recognize(
        "电子发票 无明确地点",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.city, None);
}

#[test]
fn extraction_warnings_keep_successful_recognition_pending() {
    let warnings = vec!["low_confidence".to_owned()];
    let recognized = recognize_with_warnings(
        "开票日期：2026-06-18 餐饮服务 价税合计 ￥128.50",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
        &warnings,
    );

    assert_eq!(recognized.warnings, warnings);
    assert_eq!(recognized.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(recognized.confirmation_status, ConfirmationStatus::Pending);
}

#[tokio::test]
async fn repository_get_by_id_returns_the_persisted_item() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool);
    let record = sample_item(Uuid::new_v4());
    let inserted = repository.insert(&record).await.unwrap();

    let fetched = repository.get_by_id(record.id).await.unwrap();

    assert_eq!(fetched, inserted);
}

#[tokio::test]
async fn recognize_item_persists_automatic_fields_and_preserves_manual_fields() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool);
    let mut record = sample_item(Uuid::new_v4());
    record.final_category = Some(Category::Hospitality);
    record.note = Some("manual note".to_owned());
    record.event_tag = Some("annual meeting".to_owned());
    record.project_tag = Some("P-42".to_owned());
    record.dedupe_status = DedupeStatus::Resolved;
    repository.insert(&record).await.unwrap();
    let extractor = Arc::new(FakeExtractor::new(vec![Ok(ExtractedDocument {
        text: "餐饮 食品 价税合计 ￥128.50 购买方名称：星河科技有限公司\n地址：广东省深圳市南山区"
            .to_owned(),
        normalized_pdf: None,
        warnings: Vec::new(),
    })]));
    let service = RecognitionService::new(repository.clone(), extractor.clone());

    let recognized = service.recognize_item(record.id).await.unwrap();

    assert_eq!(recognized.invoice_date, NaiveDate::from_ymd_opt(2026, 7, 2));
    assert_eq!(recognized.suggested_period.as_deref(), Some("2026-07"));
    assert_eq!(recognized.suggested_category, Some(Category::Dining));
    assert_eq!(recognized.amount_cents, Some(12_850));
    assert_eq!(recognized.company.as_deref(), Some("星河科技有限公司"));
    assert_eq!(recognized.city.as_deref(), Some("深圳"));
    assert_eq!(recognized.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(
        recognized.confirmation_status,
        ConfirmationStatus::Confirmed
    );
    assert_eq!(recognized.final_category, Some(Category::Hospitality));
    assert_eq!(recognized.note.as_deref(), Some("manual note"));
    assert_eq!(recognized.event_tag.as_deref(), Some("annual meeting"));
    assert_eq!(recognized.project_tag.as_deref(), Some("P-42"));
    assert_eq!(recognized.dedupe_status, DedupeStatus::Resolved);
    assert_eq!(extractor.paths(), [PathBuf::from(&record.original_path)]);
    assert_eq!(repository.get_by_id(record.id).await.unwrap(), recognized);
}

#[tokio::test]
async fn extraction_failure_clears_automatic_fields_before_returning_the_original_error() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool);
    let mut record = sample_item(Uuid::new_v4());
    record.invoice_date = NaiveDate::from_ymd_opt(2025, 12, 31);
    record.suggested_period = Some("2025-12".to_owned());
    record.suggested_category = Some(Category::Transport);
    record.amount_cents = Some(99_999);
    record.city = Some("北京".to_owned());
    record.company = Some("旧公司".to_owned());
    record.recognition_status = RecognitionStatus::Succeeded;
    record.confirmation_status = ConfirmationStatus::Confirmed;
    record.final_category = Some(Category::Hospitality);
    record.note = Some("keep me".to_owned());
    record.dedupe_status = DedupeStatus::Resolved;
    repository.insert(&record).await.unwrap();
    let extractor = Arc::new(FakeExtractor::new(vec![Err(AppError::External {
        service: "document_extractor".to_owned(),
        retryable: true,
        message: "temporary extraction failure".to_owned(),
    })]));
    let service = RecognitionService::new(repository.clone(), extractor);

    let error = service.recognize_item(record.id).await.unwrap_err();

    assert_eq!(
        error,
        AppError::External {
            service: "document_extractor".to_owned(),
            retryable: true,
            message: "temporary extraction failure".to_owned(),
        }
    );
    let failed = repository.get_by_id(record.id).await.unwrap();
    assert_eq!(failed.invoice_date, None);
    assert_eq!(failed.suggested_period, None);
    assert_eq!(failed.suggested_category, None);
    assert_eq!(failed.amount_cents, None);
    assert_eq!(failed.city, None);
    assert_eq!(failed.company, None);
    assert_eq!(failed.recognition_status, RecognitionStatus::Failed);
    assert_eq!(failed.confirmation_status, ConfirmationStatus::Pending);
    assert_eq!(failed.final_category, Some(Category::Hospitality));
    assert_eq!(failed.note.as_deref(), Some("keep me"));
    assert_eq!(failed.dedupe_status, DedupeStatus::Resolved);
}

#[tokio::test]
async fn retry_clears_or_replaces_old_automatic_fields_and_preserves_everything_else() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool.clone());
    let account = MailboxAccountRepository::new(pool.clone())
        .insert(NewMailboxAccount {
            provider: MailboxProvider::Gmail,
            email: "finance@example.com".to_owned(),
            imap_host: "imap.example.com".to_owned(),
            imap_port: 993,
            enabled: true,
            sync_interval_minutes: 15,
        })
        .await
        .unwrap();
    let batch = BatchRepository::new(pool)
        .create(
            NewBatch::try_new(
                "July claims",
                "2026-07-01",
                "2026-07-31",
                Some("keep batch".to_owned()),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let mut canonical = sample_item(Uuid::new_v4());
    canonical.sha256 = "shared-hash".to_owned();
    let canonical = repository.insert_deduplicated(canonical).await.unwrap();
    let mut record = sample_item(Uuid::new_v4());
    record.sha256 = "shared-hash".to_owned();
    record.source_type = SourceType::Email;
    record.source_account_id = Some(account.id);
    record.source_mailbox = Some("INBOX".to_owned());
    record.source_uid = Some(77);
    record.source_message_id = Some("message-77".to_owned());
    record.source_part_id = Some("2".to_owned());
    record.invoice_date = NaiveDate::from_ymd_opt(2025, 1, 1);
    record.suggested_period = Some("2025-01".to_owned());
    record.suggested_category = Some(Category::Dining);
    record.amount_cents = Some(1);
    record.city = Some("上海".to_owned());
    record.company = Some("旧公司".to_owned());
    record.recognition_status = RecognitionStatus::Failed;
    record.batch_id = Some(batch.id);
    record.final_category = Some(Category::Hospitality);
    record.note = Some("manual note".to_owned());
    record.event_tag = Some("client visit".to_owned());
    record.project_tag = Some("P-88".to_owned());
    let before = repository.insert_deduplicated(record).await.unwrap();
    assert_eq!(before.duplicate_of_id, Some(canonical.id));
    let extractor = Arc::new(FakeExtractor::new(vec![Ok(ExtractedDocument {
        text: "开票日期：2026-06-18 价税合计 ￥500.00".to_owned(),
        normalized_pdf: None,
        warnings: Vec::new(),
    })]));
    let service = RecognitionService::new(repository.clone(), extractor);

    let retried = service.retry(before.id).await.unwrap();

    assert_eq!(retried.invoice_date, NaiveDate::from_ymd_opt(2026, 6, 18));
    assert_eq!(retried.suggested_period.as_deref(), Some("2026-06"));
    assert_eq!(retried.suggested_category, None);
    assert_eq!(retried.amount_cents, Some(50_000));
    assert_eq!(retried.city, None);
    assert_eq!(retried.company, None);
    assert_eq!(retried.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(retried.confirmation_status, ConfirmationStatus::Pending);
    assert_eq!(retried.final_category, before.final_category);
    assert_eq!(retried.batch_id, before.batch_id);
    assert_eq!(retried.note, before.note);
    assert_eq!(retried.event_tag, before.event_tag);
    assert_eq!(retried.project_tag, before.project_tag);
    assert_eq!(retried.dedupe_status, before.dedupe_status);
    assert_eq!(retried.duplicate_of_id, before.duplicate_of_id);
    assert_eq!(retried.source_type, before.source_type);
    assert_eq!(retried.source_account_id, before.source_account_id);
    assert_eq!(retried.source_mailbox, before.source_mailbox);
    assert_eq!(retried.source_uid, before.source_uid);
    assert_eq!(retried.source_message_id, before.source_message_id);
    assert_eq!(retried.source_part_id, before.source_part_id);
    assert_eq!(retried.original_path, before.original_path);
    assert_eq!(retried.sha256, before.sha256);
    assert_eq!(retried.status(), ItemStatus::SuspectedDuplicate);
}

#[tokio::test]
async fn missing_item_returns_stable_not_found_without_extracting() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool);
    let extractor = Arc::new(FakeExtractor::new(Vec::new()));
    let service = RecognitionService::new(repository, extractor.clone());
    let recognize_id = Uuid::from_u128(1);
    let retry_id = Uuid::from_u128(2);

    for (id, error) in [
        (
            recognize_id,
            service.recognize_item(recognize_id).await.unwrap_err(),
        ),
        (retry_id, service.retry(retry_id).await.unwrap_err()),
    ] {
        assert_eq!(
            error,
            AppError::NotFound {
                entity: "item".to_owned(),
                message: format!("item {id} was not found"),
            }
        );
    }
    assert!(extractor.paths().is_empty());
}

#[tokio::test]
async fn failed_database_patch_leaves_the_entire_item_unchanged() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool.clone());
    let mut record = sample_item(Uuid::new_v4());
    record.invoice_date = NaiveDate::from_ymd_opt(2025, 1, 1);
    record.suggested_period = Some("2025-01".to_owned());
    record.suggested_category = Some(Category::Hospitality);
    record.amount_cents = Some(123);
    record.city = Some("旧城市".to_owned());
    record.company = Some("旧公司".to_owned());
    let before = repository.insert(&record).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_recognition_update \
         BEFORE UPDATE ON items \
         WHEN NEW.recognition_status = 'succeeded' \
         BEGIN SELECT RAISE(ABORT, 'reject recognition'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let extractor = Arc::new(FakeExtractor::new(vec![Ok(ExtractedDocument {
        text: "开票日期：2026-06-18 餐饮 食品 价税合计 ￥128.50".to_owned(),
        normalized_pdf: None,
        warnings: Vec::new(),
    })]));
    let service = RecognitionService::new(repository.clone(), extractor);

    let error = service.recognize_item(record.id).await.unwrap_err();

    assert!(matches!(
        error,
        AppError::Internal { ref message } if message.contains("reject recognition")
    ));
    assert_eq!(repository.get_by_id(record.id).await.unwrap(), before);
}

#[tokio::test]
async fn concurrent_manual_update_is_preserved_while_extraction_is_running() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool);
    let record = sample_item(Uuid::new_v4());
    repository.insert(&record).await.unwrap();
    let item_id = record.id;
    let (started_tx, started_rx) = sync_channel(1);
    let (proceed_tx, proceed_rx) = sync_channel(1);
    let extractor = Arc::new(GatedExtractor {
        started: started_tx,
        proceed: Mutex::new(proceed_rx),
        result: Mutex::new(Some(ExtractedDocument {
            text: "开票日期：2026-06-18 餐饮 食品 价税合计 ￥128.50".to_owned(),
            normalized_pdf: None,
            warnings: Vec::new(),
        })),
    });
    let service = RecognitionService::new(repository.clone(), extractor);
    let recognition = tokio::spawn(async move { service.recognize_item(item_id).await });
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();

    let manual_update = tokio::time::timeout(
        Duration::from_secs(1),
        repository.update_fields(
            item_id,
            ItemPatch {
                note: Some(Some("edited during extraction".to_owned())),
                ..ItemPatch::default()
            },
        ),
    )
    .await;
    proceed_tx.send(()).unwrap();
    manual_update
        .expect("recognition must not hold a database connection while extracting")
        .unwrap();
    let recognized = recognition.await.unwrap().unwrap();

    assert_eq!(recognized.note.as_deref(), Some("edited during extraction"));
    assert_eq!(recognized.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(
        recognized.confirmation_status,
        ConfirmationStatus::Confirmed
    );
}

fn sample_item(id: Uuid) -> NewItemRecord {
    NewItemRecord {
        id,
        original_name: "invoice.pdf".to_owned(),
        original_path: format!("/invoices/{id}.pdf"),
        normalized_pdf_path: None,
        sha256: format!("sha256-{id}"),
        mime_type: "application/pdf".to_owned(),
        source_type: SourceType::ManualUpload,
        source_account_id: None,
        source_mailbox: None,
        source_uid: None,
        source_message_id: None,
        source_part_id: None,
        fetched_at: Utc.with_ymd_and_hms(2026, 7, 2, 9, 30, 0).single().unwrap(),
        invoice_date: None,
        suggested_period: None,
        batch_id: None,
        suggested_category: None,
        final_category: None,
        amount_cents: None,
        currency: "CNY".to_owned(),
        city: None,
        company: None,
        recognition_status: RecognitionStatus::Pending,
        confirmation_status: ConfirmationStatus::Pending,
        dedupe_status: DedupeStatus::Unique,
        duplicate_of_id: None,
        note: None,
        event_tag: None,
        project_tag: None,
        created_at: Utc.with_ymd_and_hms(2026, 7, 3, 9, 35, 0).single().unwrap(),
        updated_at: Utc.with_ymd_and_hms(2026, 7, 3, 9, 35, 0).single().unwrap(),
    }
}
