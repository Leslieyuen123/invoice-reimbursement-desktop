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
    BatchStatus, Category, ConfirmationStatus, DedupeStatus, ItemStatus, NewBatch,
    RecognitionStatus, SourceType,
};
use invoice_reimbursement::infra::extraction::{DocumentExtractor, ExtractedDocument};
use invoice_reimbursement::infra::files::AppPaths;
use invoice_reimbursement::services::batches::BatchService;
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

async fn mark_batch_exported(pool: &sqlx::SqlitePool, batch_id: Uuid) {
    sqlx::query(
        "UPDATE batches SET status = 'exported', last_exported_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind("2026-07-14T08:00:00Z")
    .bind("2026-07-14T08:00:00Z")
    .bind(batch_id.to_string())
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn recognition_persists_normalized_pdf_for_export() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool);
    let record = sample_item(Uuid::new_v4());
    repository.insert(&record).await.unwrap();
    let normalized_bytes = b"%PDF-1.4 normalized invoice".to_vec();
    let extractor = Arc::new(FakeExtractor::new(vec![Ok(ExtractedDocument {
        text: "开票日期：2026年06月18日 餐饮 食品 价税合计 ￥128.50".to_owned(),
        normalized_pdf: Some(normalized_bytes.clone()),
        warnings: Vec::new(),
    })]));
    let service = RecognitionService::with_paths(repository, paths.clone(), extractor);

    let recognized = service.recognize_item(record.id).await.unwrap();

    let normalized_path = PathBuf::from(
        recognized
            .normalized_pdf_path
            .expect("normalized path should be persisted"),
    );
    assert!(normalized_path.starts_with(&paths.normalized));
    assert_eq!(std::fs::read(normalized_path).unwrap(), normalized_bytes);
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
fn flattened_pdf_text_recovers_trailing_invoice_values() {
    let recognized = recognize(
        "电子发票（普通发票） 发票号码：\n开票日期：\n购买方信息\n名称：\n销售方信息\n名称：\n项目名称 车牌号 车辆类型 通行日期起 通行日期止 金额 税率/征收率 税额\n价税合计（大写） （小写）\n备注\n2024/12/12 localhost:63342/template.html\n26317907120600246026 2026年06月14日 91310230MA1K0PRP1X 上海路桥发展有限公司 91310000631588023C *生产生活服务*通行费 沪ACA9778 客车 20260614 1.42 3% 0.04 ¥1.42 壹圆肆角陆分 姜娟 ¥1.46 20260614 上海大绍文化传媒有限公司 ¥0.04",
        NaiveDate::from_ymd_opt(2026, 7, 20).unwrap(),
    );

    assert_eq!(
        recognized.invoice_date,
        NaiveDate::from_ymd_opt(2026, 6, 14)
    );
    assert_eq!(recognized.amount_cents, Some(146));
    assert_eq!(recognized.category, Some(Category::Transport));
    assert_eq!(recognized.company, None);
    assert_eq!(recognized.warnings, ["invalid_invoice_date"]);
    assert_eq!(recognized.confirmation_status, ConfirmationStatus::Pending);
}

#[test]
fn flattened_date_search_window_counts_unicode_characters() {
    let remarks_prefix = "说".repeat(100);
    let text = format!(
        "电子发票（普通发票） 发票号码：\n开票日期：\n购买方信息\n名称：\n销售方信息\n名称：\n项目名称\n价税合计\n备注\n{remarks_prefix}\n26317907120600246026 2026年06月14日"
    );

    let recognized = recognize(&text, NaiveDate::from_ymd_opt(2026, 7, 20).unwrap());

    assert_eq!(
        recognized.invoice_date,
        NaiveDate::from_ymd_opt(2026, 6, 14)
    );
    assert_eq!(recognized.suggested_period, "2026-06");
    assert_eq!(recognized.warnings, ["invalid_invoice_date"]);
    assert_eq!(recognized.confirmation_status, ConfirmationStatus::Pending);
}

#[test]
fn empty_invoice_date_does_not_consume_a_later_travel_date() {
    let recognized = recognize(
        "开票日期：\n通行日期：2026-06-14\n餐饮 食品 价税合计 ￥128.50",
        NaiveDate::from_ymd_opt(2026, 7, 20).unwrap(),
    );

    assert_eq!(recognized.invoice_date, None);
    assert_eq!(recognized.suggested_period, "2026-07");
    assert_eq!(recognized.warnings, ["invalid_invoice_date"]);
    assert_eq!(recognized.confirmation_status, ConfirmationStatus::Pending);
}

#[test]
fn incomplete_metadata_falls_back_to_received_month_and_needs_confirmation() {
    let recognized = recognize("电子票据", NaiveDate::from_ymd_opt(2026, 7, 2).unwrap());

    assert_eq!(recognized.invoice_date, None);
    assert_eq!(recognized.suggested_period, "2026-07");
    assert_eq!(recognized.category, None);
    assert_eq!(
        recognized.status(DedupeStatus::Unique),
        ItemStatus::PendingConfirmation
    );
}

#[test]
fn complete_amount_and_category_without_invoice_date_needs_confirmation() {
    let recognized = recognize(
        "餐饮 食品 价税合计 ￥128.50",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.invoice_date, None);
    assert_eq!(recognized.suggested_period, "2026-07");
    assert_eq!(recognized.amount_cents, Some(12_850));
    assert_eq!(recognized.category, Some(Category::Dining));
    assert_eq!(recognized.confirmation_status, ConfirmationStatus::Pending);
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
fn invalid_invoice_date_uses_an_earlier_explicit_generic_date() {
    let recognized = recognize(
        "日期：2026-06-18 开票日期：2026-02-31 餐饮服务 价税合计 ¥128.50",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(
        recognized.invoice_date,
        NaiveDate::from_ymd_opt(2026, 6, 18)
    );
    assert_eq!(recognized.suggested_period, "2026-06");
    assert_eq!(recognized.warnings, ["invalid_invoice_date"]);
    assert_eq!(
        recognized.status(DedupeStatus::Unique),
        ItemStatus::PendingConfirmation
    );
}

#[test]
fn recognition_outcome_status_uses_the_supplied_dedupe_status() {
    let recognized = recognize(
        "开票日期：2026-06-18 餐饮服务 价税合计 ¥128.50",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(
        recognized.status(DedupeStatus::SuspectedDuplicate),
        ItemStatus::SuspectedDuplicate
    );
}

#[test]
fn invalid_or_empty_invoice_date_uses_a_later_explicit_generic_date() {
    for (text, expected_date, expected_period) in [
        (
            "开票日期：2026-02-31 日期：2026年06月19日 餐饮服务 价税合计 ¥128.50",
            NaiveDate::from_ymd_opt(2026, 6, 19).unwrap(),
            "2026-06",
        ),
        (
            "开票日期：\n日期：2026-07-20 餐饮服务 价税合计 ¥128.50",
            NaiveDate::from_ymd_opt(2026, 7, 20).unwrap(),
            "2026-07",
        ),
    ] {
        let recognized = recognize(text, NaiveDate::from_ymd_opt(2026, 8, 2).unwrap());

        assert_eq!(recognized.invoice_date, Some(expected_date), "{text}");
        assert_eq!(recognized.suggested_period, expected_period, "{text}");
        assert_eq!(recognized.warnings, ["invalid_invoice_date"], "{text}");
        assert_eq!(recognized.recognition_status, RecognitionStatus::Succeeded);
        assert_eq!(recognized.confirmation_status, ConfirmationStatus::Pending);
    }
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
fn amount_uses_multiline_lowercase_total_after_the_uppercase_total() {
    for (text, expected) in [
        (
            "价税合计 (大写)\n壹仟零壹拾肆圆整\n（小写)\n￥1014.00",
            101_400,
        ),
        (
            "价税合计 (大写)\n壹佰捌拾壹圆叁角陆分\n（小写）¥181.36",
            18_136,
        ),
    ] {
        let recognized = recognize(text, NaiveDate::from_ymd_opt(2026, 7, 20).unwrap());
        assert_eq!(recognized.amount_cents, Some(expected), "{text}");
    }
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
fn transport_category_recognizes_current_invoice_and_itinerary_terms() {
    for text in [
        "旅客运输服务 *交通运输服务*客运服务费",
        "美团打车行程单 出行费用合计",
    ] {
        let recognized = recognize(text, NaiveDate::from_ymd_opt(2026, 7, 20).unwrap());
        assert_eq!(recognized.category, Some(Category::Transport), "{text}");
    }
}

#[test]
fn toll_invoice_vehicle_fields_are_transport() {
    let recognized = recognize(
        "电子发票（普通发票）\n*生产生活服务*通行费\n车牌号：沪ACA9778\n车辆类型：客车",
        NaiveDate::from_ymd_opt(2026, 7, 20).unwrap(),
    );

    assert_eq!(recognized.category, Some(Category::Transport));
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
fn company_reads_buyer_name_from_separated_ocr_section_layout() {
    let recognized = recognize(
        "购买方信息\n销售方信息\n名称：上海大绍文化传媒有限公司\n名称：上海路桥发展有限公司\n统一社会信用代码/纳税人识别号：91310230MA1K0PRP1X\n统一社会信用代码/纳税人识别号：91310000631588023C\n项目名称",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(
        recognized.company.as_deref(),
        Some("上海大绍文化传媒有限公司")
    );
}

#[test]
fn company_reads_buyer_from_character_fragmented_ocr_header() {
    let recognized = recognize(
        "购\n买\n方\n信\n息\n名称：上海大绍文化传媒有限公司\n统一社会信用代码/纳税人识别号：91310230MA1K0PRP1X\n销\n售\n方\n信\n息\n名称：阿斯兰航空服务（上海）有限公司",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(
        recognized.company.as_deref(),
        Some("上海大绍文化传媒有限公司")
    );
}

#[test]
fn company_is_not_guessed_from_a_reversed_ocr_section_header() {
    let recognized = recognize(
        "页眉示例有限公司\n息 信 方 买 购 名称：\n息 信 方 售 销 名称：\n2026年06月15日 上海大绍文化传媒有限公司 91310230MA1K0PRP1X 上海路团科技有限公司 91310105MA1FW5NA99\n备注其他有限公司",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.company, None);
}

#[test]
fn company_is_not_guessed_from_unlabeled_buyer_and_seller_sections() {
    let recognized = recognize(
        "购买方信息\n上海星河科技有限公司\n纳税人识别号：91310000\n销售方信息\n北京远方服务有限公司\n纳税人识别号：91110000",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.company, None);
}

#[test]
fn company_requires_a_verified_flattened_record_tail_before_template_fallback() {
    let recognized = recognize(
        "电子发票（普通发票） 发票号码：\n开票日期：\n购买方信息\n名称：\n销售方信息\n名称：\n项目名称 服务费\n购买方 上海星河科技有限公司\n销售方 北京远方服务有限公司\n价税合计（小写） ￥128.50\n备注\n无结构化尾部",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.company, None);
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
fn company_name_stops_at_inline_invoice_field_labels() {
    for (text, expected) in [
        (
            "购买方名称：上海星河科技有限公司 纳税人识别号：91310000 地址：上海市浦东新区",
            "上海星河科技有限公司",
        ),
        (
            "购买方名称：上海星河科技有限公司 地址：上海市浦东新区",
            "上海星河科技有限公司",
        ),
        (
            "购买方名称：上海星河科技有限公司 销售方名称：北京远方服务有限公司",
            "上海星河科技有限公司",
        ),
        (
            "销售方名称：北京远方服务有限公司 纳税人识别号：91110000 地址：北京市朝阳区",
            "北京远方服务有限公司",
        ),
    ] {
        let recognized = recognize(text, NaiveDate::from_ymd_opt(2026, 7, 2).unwrap());

        assert_eq!(recognized.company.as_deref(), Some(expected), "{text}");
    }
}

#[test]
fn empty_buyer_line_falls_back_to_the_seller_without_consuming_the_next_line() {
    let recognized = recognize(
        "购买方名称：\n销售方名称：北京远方服务有限公司 纳税人识别号：91110000",
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
    );

    assert_eq!(recognized.company.as_deref(), Some("北京远方服务有限公司"));
}

#[test]
fn company_scans_field_candidates_past_explanations_and_empty_templates() {
    for text in [
        "说明：此处的购买方名称用于展示字段含义\n购买方名称：上海星河科技有限公司",
        "购买方名称：\n购买方名称：上海星河科技有限公司",
    ] {
        let recognized = recognize(text, NaiveDate::from_ymd_opt(2026, 7, 2).unwrap());

        assert_eq!(
            recognized.company.as_deref(),
            Some("上海星河科技有限公司"),
            "{text}"
        );
    }
}

#[test]
fn city_dictionary_prefers_explicit_and_longest_city_names() {
    for (text, expected) in [
        ("购买方地址：北京市朝阳区", "北京"),
        ("销售方地址：上海市浦东新区", "上海"),
        ("项目地点：广东省广州市天河区", "广州"),
        ("服务地点：广东省深圳市南山区", "深圳"),
        ("收货地址：江苏省南京市鼓楼区", "南京"),
    ] {
        let recognized = recognize(text, NaiveDate::from_ymd_opt(2026, 7, 2).unwrap());

        assert_eq!(recognized.city.as_deref(), Some(expected), "{text}");
    }
}

#[test]
fn bare_city_names_do_not_match_roads_companies_or_product_names() {
    for text in [
        "南京路",
        "北京远方公司",
        "商品：上海牌",
        "地址：南京路100号",
    ] {
        let recognized = recognize(text, NaiveDate::from_ymd_opt(2026, 7, 2).unwrap());

        assert_eq!(recognized.city, None, "{text}");
    }
}

#[test]
fn bare_city_names_require_an_explicit_location_context() {
    for (text, expected) in [
        ("地址：江苏省南京鼓楼区", "南京"),
        ("项目地点：北京朝阳区", "北京"),
        ("城市：上海", "上海"),
        ("出发地：广州白云机场", "广州"),
        ("到达地：深圳宝安机场", "深圳"),
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
async fn extraction_warnings_persist_pending_without_a_final_category() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool);
    let record = sample_item(Uuid::new_v4());
    repository.insert(&record).await.unwrap();
    let extractor = Arc::new(FakeExtractor::new(vec![Ok(ExtractedDocument {
        text: "开票日期：2026-05-08\n价税合计：¥128.50\n餐饮 食品".to_owned(),
        normalized_pdf: None,
        warnings: vec!["low_confidence".to_owned()],
    })]));
    let service = RecognitionService::new(repository, extractor);

    let recognized = service.recognize_item(record.id).await.unwrap();

    assert_eq!(recognized.suggested_category, Some(Category::Dining));
    assert_eq!(recognized.confirmation_status, ConfirmationStatus::Pending);
    assert_eq!(recognized.final_category, None);
}

#[tokio::test]
async fn confident_recognition_confirms_and_copies_the_suggested_category_to_final() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool);
    let record = sample_item(Uuid::new_v4());
    repository.insert(&record).await.unwrap();
    let extractor = Arc::new(FakeExtractor::new(vec![Ok(ExtractedDocument {
        text: "发票日期：2026-05-08\n日期：2026-05-08\n价税合计：¥128.50\n出租车 客运服务"
            .to_owned(),
        normalized_pdf: None,
        warnings: Vec::new(),
    })]));
    let service = RecognitionService::new(repository, extractor);

    let recognized = service.recognize_item(record.id).await.unwrap();

    assert_eq!(
        recognized.confirmation_status,
        ConfirmationStatus::Confirmed
    );
    assert_eq!(recognized.suggested_category, Some(Category::Transport));
    assert_eq!(recognized.final_category, Some(Category::Transport));
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
    let repository = ItemRepository::new(pool.clone());
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

    assert_eq!(recognized.invoice_date, None);
    assert_eq!(recognized.suggested_period.as_deref(), Some("2026-07"));
    assert_eq!(recognized.suggested_category, Some(Category::Dining));
    assert_eq!(recognized.amount_cents, Some(12_850));
    assert_eq!(recognized.company.as_deref(), Some("星河科技有限公司"));
    assert_eq!(recognized.city.as_deref(), Some("深圳"));
    assert_eq!(recognized.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(recognized.confirmation_status, ConfirmationStatus::Pending);
    assert_eq!(recognized.final_category, Some(Category::Hospitality));
    assert_eq!(recognized.note.as_deref(), Some("manual note"));
    assert_eq!(recognized.event_tag.as_deref(), Some("annual meeting"));
    assert_eq!(recognized.project_tag.as_deref(), Some("P-42"));
    assert_eq!(recognized.dedupe_status, DedupeStatus::Resolved);
    assert_eq!(extractor.paths(), [PathBuf::from(&record.original_path)]);
    assert_eq!(repository.get_by_id(record.id).await.unwrap(), recognized);

    let batch = BatchRepository::new(pool.clone())
        .create(NewBatch::try_new("July claims", "2026-07-01", "2026-07-31", None).unwrap())
        .await
        .unwrap();
    let candidates = BatchService::new(pool)
        .list_candidates(batch.id, Some("invoice".to_owned()), None, 50)
        .await
        .unwrap();
    assert_eq!(candidates.items.len(), 1);
    assert_eq!(candidates.items[0].invoice_date, None);
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
    record.confirmation_status = ConfirmationStatus::Pending;
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
    let batch = BatchRepository::new(pool.clone())
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
    record.source_uid_validity = Some(10);
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
    record.final_category = Some(Category::Hospitality);
    record.note = Some("manual note".to_owned());
    record.event_tag = Some("client visit".to_owned());
    record.project_tag = Some("P-88".to_owned());
    let inserted = repository.insert_deduplicated(record).await.unwrap();
    sqlx::query("UPDATE items SET batch_id = ? WHERE id = ?")
        .bind(batch.id.to_string())
        .bind(inserted.id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let before = repository.get_by_id(inserted.id).await.unwrap();
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

#[tokio::test]
async fn concurrent_review_keeps_normalized_pdf_and_drafts_the_exported_batch() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool.clone());
    let batch_repository = BatchRepository::new(pool.clone());
    let batch = batch_repository
        .create(NewBatch::try_new("Reviewed May claims", "2026-05-01", "2026-05-31", None).unwrap())
        .await
        .unwrap();
    let mut record = sample_item(Uuid::new_v4());
    record.batch_id = Some(batch.id);
    repository.insert(&record).await.unwrap();
    let item_id = record.id;
    let normalized_bytes = b"%PDF-1.4 normalized during concurrent review".to_vec();
    let (started_tx, started_rx) = sync_channel(1);
    let (proceed_tx, proceed_rx) = sync_channel(1);
    let extractor = Arc::new(GatedExtractor {
        started: started_tx,
        proceed: Mutex::new(proceed_rx),
        result: Mutex::new(Some(ExtractedDocument {
            text: "开票日期：2026-06-18 餐饮 食品 价税合计 ￥128.50".to_owned(),
            normalized_pdf: Some(normalized_bytes.clone()),
            warnings: Vec::new(),
        })),
    });
    let service = RecognitionService::with_paths(repository.clone(), paths.clone(), extractor);
    let recognition = tokio::spawn(async move { service.recognize_item(item_id).await });
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();

    let manual_date = NaiveDate::from_ymd_opt(2026, 5, 9).unwrap();
    let reviewed = repository
        .update_fields(
            item_id,
            ItemPatch {
                invoice_date: Some(Some(manual_date)),
                suggested_period: Some(Some("2026-05".to_owned())),
                final_category: Some(Some(Category::Hospitality)),
                amount_cents: Some(Some(9_999)),
                city: Some(Some("上海".to_owned())),
                company: Some(Some("人工确认公司".to_owned())),
                recognition_status: Some(RecognitionStatus::Succeeded),
                confirmation_status: Some(ConfirmationStatus::Confirmed),
                note: Some(Some("识别期间已确认".to_owned())),
                event_tag: Some(Some("客户会议".to_owned())),
                project_tag: Some(Some("P-2026-05".to_owned())),
                ..ItemPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(reviewed.status(), ItemStatus::Ready);
    mark_batch_exported(&pool, batch.id).await;
    assert_eq!(
        batch_repository.get(batch.id).await.unwrap().status,
        BatchStatus::Exported
    );
    proceed_tx.send(()).unwrap();
    let recognized = recognition.await.unwrap().unwrap();

    let expected_path = paths.normalized.join(format!("{item_id}.pdf"));
    assert_eq!(
        recognized.normalized_pdf_path.as_deref(),
        Some(expected_path.to_string_lossy().as_ref())
    );
    assert_eq!(std::fs::read(expected_path).unwrap(), normalized_bytes);
    assert_eq!(recognized.invoice_date, Some(manual_date));
    assert_eq!(recognized.suggested_period.as_deref(), Some("2026-05"));
    assert_eq!(recognized.final_category, Some(Category::Hospitality));
    assert_eq!(recognized.amount_cents, Some(9_999));
    assert_eq!(recognized.city.as_deref(), Some("上海"));
    assert_eq!(recognized.company.as_deref(), Some("人工确认公司"));
    assert_eq!(recognized.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(
        recognized.confirmation_status,
        ConfirmationStatus::Confirmed
    );
    assert_eq!(recognized.note.as_deref(), Some("识别期间已确认"));
    assert_eq!(recognized.event_tag.as_deref(), Some("客户会议"));
    assert_eq!(recognized.project_tag.as_deref(), Some("P-2026-05"));
    assert_eq!(recognized.batch_id, Some(batch.id));
    assert_eq!(recognized.status(), ItemStatus::Ready);
    let persisted_batch = batch_repository.get(batch.id).await.unwrap();
    assert_eq!(persisted_batch.status, BatchStatus::Draft);
    assert!(persisted_batch.last_exported_at.is_some());
}

#[tokio::test]
async fn guarded_normalization_failure_rolls_back_path_and_cleans_generated_files() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::create(directory.path().join("storage")).unwrap();
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let repository = ItemRepository::new(pool.clone());
    let batch_repository = BatchRepository::new(pool.clone());
    let batch = batch_repository
        .create(NewBatch::try_new("Reviewed May claims", "2026-05-01", "2026-05-31", None).unwrap())
        .await
        .unwrap();
    let mut record = sample_item(Uuid::new_v4());
    record.batch_id = Some(batch.id);
    repository.insert(&record).await.unwrap();
    let item_id = record.id;
    let normalized_bytes = b"%PDF-1.4 rollback normalized invoice".to_vec();
    let (started_tx, started_rx) = sync_channel(1);
    let (proceed_tx, proceed_rx) = sync_channel(1);
    let extractor = Arc::new(GatedExtractor {
        started: started_tx,
        proceed: Mutex::new(proceed_rx),
        result: Mutex::new(Some(ExtractedDocument {
            text: "开票日期：2026-06-18 餐饮 食品 价税合计 ￥128.50".to_owned(),
            normalized_pdf: Some(normalized_bytes),
            warnings: Vec::new(),
        })),
    });
    let service = RecognitionService::with_paths(repository.clone(), paths.clone(), extractor);
    let recognition = tokio::spawn(async move { service.recognize_item(item_id).await });
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();

    let manual_date = NaiveDate::from_ymd_opt(2026, 5, 9).unwrap();
    let reviewed = repository
        .update_fields(
            item_id,
            ItemPatch {
                invoice_date: Some(Some(manual_date)),
                suggested_period: Some(Some("2026-05".to_owned())),
                final_category: Some(Some(Category::Hospitality)),
                amount_cents: Some(Some(9_999)),
                city: Some(Some("上海".to_owned())),
                company: Some(Some("人工确认公司".to_owned())),
                recognition_status: Some(RecognitionStatus::Succeeded),
                confirmation_status: Some(ConfirmationStatus::Confirmed),
                note: Some(Some("识别期间已确认".to_owned())),
                event_tag: Some(Some("客户会议".to_owned())),
                project_tag: Some(Some("P-2026-05".to_owned())),
                ..ItemPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(reviewed.status(), ItemStatus::Ready);
    mark_batch_exported(&pool, batch.id).await;
    sqlx::query(&format!(
        "CREATE TRIGGER reject_guarded_batch_reset BEFORE UPDATE ON batches \
         WHEN OLD.id = '{}' AND NEW.status = 'draft' \
         BEGIN SELECT RAISE(ABORT, 'reject guarded batch reset'); END",
        batch.id
    ))
    .execute(&pool)
    .await
    .unwrap();
    proceed_tx.send(()).unwrap();

    let error = recognition.await.unwrap().unwrap_err();

    assert!(
        matches!(error, AppError::Internal { ref message } if message.contains("reject guarded batch reset")),
        "unexpected error: {error:?}"
    );
    let expected_path = paths.normalized.join(format!("{item_id}.pdf"));
    let expected_staging = paths.staging.join(format!("{item_id}.normalized.part"));
    assert!(!expected_path.exists());
    assert!(!expected_staging.exists());
    let persisted = repository.get_by_id(item_id).await.unwrap();
    assert_eq!(persisted.normalized_pdf_path, None);
    assert_eq!(persisted.invoice_date, Some(manual_date));
    assert_eq!(persisted.suggested_period.as_deref(), Some("2026-05"));
    assert_eq!(persisted.final_category, Some(Category::Hospitality));
    assert_eq!(persisted.amount_cents, Some(9_999));
    assert_eq!(persisted.city.as_deref(), Some("上海"));
    assert_eq!(persisted.company.as_deref(), Some("人工确认公司"));
    assert_eq!(persisted.recognition_status, RecognitionStatus::Succeeded);
    assert_eq!(persisted.confirmation_status, ConfirmationStatus::Confirmed);
    assert_eq!(persisted.note.as_deref(), Some("识别期间已确认"));
    assert_eq!(persisted.event_tag.as_deref(), Some("客户会议"));
    assert_eq!(persisted.project_tag.as_deref(), Some("P-2026-05"));
    assert_eq!(persisted.batch_id, Some(batch.id));
    assert_eq!(persisted.status(), ItemStatus::Ready);
    let persisted_batch = batch_repository.get(batch.id).await.unwrap();
    assert_eq!(persisted_batch.status, BatchStatus::Exported);
    assert!(persisted_batch.last_exported_at.is_some());
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
        source_uid_validity: None,
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
