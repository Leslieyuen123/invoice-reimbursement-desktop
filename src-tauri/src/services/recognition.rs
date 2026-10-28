use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use sha2::{Digest, Sha256};

use crate::db::items::{InvoiceItem, ItemPatch, ItemRepository};
use crate::domain::error::AppError;
use crate::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, ItemStatus, RecognitionStatus, derive_item_status,
};
use crate::infra::extraction::DocumentExtractor;
use crate::infra::files::{AppPaths, open_contained_regular_file};

/// Original formats the extractor can turn into a normalized PDF.
///
/// Office documents and ZIP archives are imported on purpose, but they can
/// never be part of an export package, so the UI must say so instead of
/// retrying forever.
pub fn original_can_be_normalized(original_name: &str) -> bool {
    Path::new(original_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "pdf" | "jpg" | "jpeg" | "png"
            )
        })
}

const CITY_NAMES: &[&str] = &[
    "北京",
    "上海",
    "天津",
    "重庆",
    "石家庄",
    "太原",
    "呼和浩特",
    "沈阳",
    "大连",
    "长春",
    "吉林",
    "哈尔滨",
    "南京",
    "苏州",
    "无锡",
    "杭州",
    "宁波",
    "温州",
    "合肥",
    "福州",
    "厦门",
    "泉州",
    "南昌",
    "济南",
    "青岛",
    "郑州",
    "武汉",
    "长沙",
    "广州",
    "深圳",
    "珠海",
    "佛山",
    "东莞",
    "南宁",
    "海口",
    "三亚",
    "成都",
    "贵阳",
    "昆明",
    "拉萨",
    "西安",
    "兰州",
    "西宁",
    "银川",
    "乌鲁木齐",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecognitionOutcome {
    pub invoice_date: Option<NaiveDate>,
    pub suggested_period: String,
    pub category: Option<Category>,
    pub amount_cents: Option<i64>,
    pub city: Option<String>,
    pub company: Option<String>,
    pub warnings: Vec<String>,
    pub recognition_status: RecognitionStatus,
    pub confirmation_status: ConfirmationStatus,
}

#[derive(Clone)]
pub struct RecognitionService {
    items: ItemRepository,
    paths: Option<AppPaths>,
    extractor: Arc<dyn DocumentExtractor>,
}

impl RecognitionService {
    pub fn new(items: ItemRepository, extractor: Arc<dyn DocumentExtractor>) -> Self {
        Self {
            items,
            paths: None,
            extractor,
        }
    }

    pub fn with_paths(
        items: ItemRepository,
        paths: AppPaths,
        extractor: Arc<dyn DocumentExtractor>,
    ) -> Self {
        Self {
            items,
            paths: Some(paths),
            extractor,
        }
    }

    pub async fn recognize_item(&self, id: uuid::Uuid) -> Result<InvoiceItem, AppError> {
        self.recognize_item_with_policy(id, true).await
    }

    async fn recognize_item_with_policy(
        &self,
        id: uuid::Uuid,
        preserve_manual_confirmation: bool,
    ) -> Result<InvoiceItem, AppError> {
        let item = self.items.get_by_id(id).await?;
        let received_date = item
            .batch_membership_date()
            .unwrap_or_else(|| item.fetched_at.date_naive());
        let path = PathBuf::from(&item.original_path);
        let extractor = self.extractor.clone();
        let extraction = tokio::task::spawn_blocking(move || extractor.extract(&path))
            .await
            .map_err(|error| AppError::Internal {
                message: format!("document extraction task failed: {error}"),
            })
            .and_then(|result| result);
        let extracted = match extraction {
            Ok(extracted) => extracted,
            Err(extraction_error) => {
                self.persist_recognition_patch(
                    id,
                    ItemPatch {
                        note: reason_note(
                            &item.note,
                            &extraction_failure_note(&item.original_name, &extraction_error),
                        ),
                        invoice_date: Some(None),
                        suggested_period: Some(None),
                        suggested_category: Some(None),
                        amount_cents: Some(None),
                        city: Some(None),
                        company: Some(None),
                        recognition_status: Some(RecognitionStatus::Failed),
                        confirmation_status: Some(ConfirmationStatus::Pending),
                        ..ItemPatch::default()
                    },
                    preserve_manual_confirmation,
                )
                .await?;
                return Err(extraction_error);
            }
        };
        if !preserve_manual_confirmation
            && extracted.normalized_pdf.is_none()
            && extracted.text.trim().is_empty()
        {
            // An explicit retry over a format the extractor cannot read used to
            // wipe the reviewed date and amount and drop the invoice back to
            // "awaiting confirmation". Keep the manual values instead.
            return Err(AppError::validation(
                "file",
                "该原件格式不支持自动识别，已保留人工填写的内容",
            ));
        }
        // A service without storage paths used to skip normalization silently and
        // still mark the invoice as recognized, which is exactly how an
        // unexportable batch is created. Refuse instead of pretending.
        if item.normalized_pdf_path.is_none()
            && extracted.normalized_pdf.is_some()
            && self.paths.is_none()
        {
            return Err(AppError::Internal {
                message: "recognition service has no application paths to store the normalized PDF"
                    .to_owned(),
            });
        }
        let created_normalized = match (
            item.normalized_pdf_path.as_ref(),
            extracted.normalized_pdf.as_deref(),
            self.paths.as_ref(),
        ) {
            (None, Some(bytes), Some(paths)) => {
                let paths = paths.clone();
                let bytes = bytes.to_vec();
                Some(
                    tokio::task::spawn_blocking(move || paths.persist_normalized_pdf(id, &bytes))
                        .await
                        .map_err(|error| AppError::Internal {
                            message: format!("normalized PDF storage task failed: {error}"),
                        })??,
                )
            }
            _ => None,
        };
        let unsupported_note = extracted
            .warnings
            .iter()
            .any(|warning| warning == "unsupported_for_recognition")
            .then(|| unsupported_format_note(&item.original_name));
        let recognized =
            recognize_with_warnings(&extracted.text, received_date, &extracted.warnings);
        // A readable document that still has no amount needs a reason the user
        // can see: otherwise it looks like the OCR read nothing at all.
        let failure_note = unsupported_note.or_else(|| {
            recognized
                .warnings
                .iter()
                .any(|warning| warning == OCR_TEXT_WITHOUT_AMOUNT)
                .then(|| amount_not_parsed_note(&item.original_name))
        });
        let semantic_fingerprint = semantic_invoice_fingerprint(
            &extracted.text,
            recognized.amount_cents,
            recognized.city.as_deref(),
        );
        let semantic_dedupe_eligible = item.confirmation_status == ConfirmationStatus::Pending
            && item.batch_id.is_none()
            && item.dedupe_status == DedupeStatus::Unique;
        let automatic_final_category = (recognized.confirmation_status
            == ConfirmationStatus::Confirmed)
            .then_some(recognized.category)
            .flatten();

        let persisted = self
            .persist_recognition_patch(
                id,
                ItemPatch {
                    normalized_pdf_path: created_normalized
                        .as_ref()
                        .map(|path| Some(path.to_string_lossy().into_owned())),
                    invoice_date: Some(recognized.invoice_date),
                    suggested_period: Some(Some(recognized.suggested_period)),
                    suggested_category: Some(recognized.category),
                    final_category_if_missing: automatic_final_category,
                    amount_cents: Some(recognized.amount_cents),
                    city: Some(recognized.city),
                    company: Some(recognized.company),
                    recognition_status: Some(recognized.recognition_status),
                    confirmation_status: Some(recognized.confirmation_status),
                    note: match failure_note {
                        // A readable document clears whatever a previous attempt
                        // left behind, so a stale reason is never shown again.
                        None => is_recognition_note(item.note.as_deref()).then_some(None),
                        Some(reason) => reason_note(&item.note, &reason),
                    },
                    ..ItemPatch::default()
                },
                preserve_manual_confirmation,
            )
            .await;
        match persisted {
            Ok(item)
                if created_normalized.as_ref().is_some_and(|path| {
                    item.normalized_pdf_path.as_deref() != Some(path.to_string_lossy().as_ref())
                }) =>
            {
                self.cleanup_created_normalized(created_normalized.unwrap())
                    .await?;
                Ok(item)
            }
            Ok(item) => {
                if semantic_dedupe_eligible
                    && let Some(fingerprint) = semantic_fingerprint.as_deref()
                {
                    return self
                        .items
                        .record_semantic_identity(id, fingerprint, item.updated_at)
                        .await;
                }
                Ok(item)
            }
            Err(error) => {
                if let Some(path) = created_normalized {
                    self.cleanup_created_normalized(path).await?;
                }
                Err(error)
            }
        }
    }

    async fn cleanup_created_normalized(&self, path: PathBuf) -> Result<(), AppError> {
        let Some(paths) = self.paths.clone() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || paths.delete_normalized_pdf(&path))
            .await
            .map_err(|error| AppError::Internal {
                message: format!("normalized PDF cleanup task failed: {error}"),
            })?
    }

    pub async fn retry(&self, id: uuid::Uuid) -> Result<InvoiceItem, AppError> {
        self.recognize_item_with_policy(id, false).await
    }

    /// Persists a normalized PDF for an invoice that never got one.
    ///
    /// Recognition only writes the normalized PDF while it runs for a pending
    /// invoice. Invoices confirmed manually (or whose normalized file went
    /// missing) stayed "ready for a batch" while the export rejected the whole
    /// batch with 票据缺少归一化 PDF, and nothing could repair them. This
    /// backfill writes only `normalized_pdf_path`: reviewed fields, category,
    /// amount and batch membership stay untouched, and the affected batch is
    /// moved back to draft so it is exported again.
    pub async fn backfill_normalized_pdf(&self, id: uuid::Uuid) -> Result<InvoiceItem, AppError> {
        let item = self.items.get_by_id(id).await?;
        let Some(paths) = self.paths.clone() else {
            return Err(AppError::Internal {
                message: "recognition service has no application paths".to_owned(),
            });
        };
        if item.normalized_pdf_path.as_deref().is_some_and(|path| {
            open_contained_regular_file(Path::new(path), &paths.normalized, "normalizedPdf").is_ok()
        }) {
            return Ok(item);
        }
        if !original_can_be_normalized(&item.original_name) {
            return Err(AppError::validation(
                "normalizedPdf",
                "该原件格式不支持生成归一化 PDF，请改用 PDF、JPG 或 PNG 原件，或将其移出批次",
            ));
        }

        let source = PathBuf::from(&item.original_path);
        let extractor = self.extractor.clone();
        let extracted = tokio::task::spawn_blocking(move || extractor.extract(&source))
            .await
            .map_err(|error| AppError::Internal {
                message: format!("document extraction task failed: {error}"),
            })
            .and_then(|result| result)?;
        let Some(bytes) = extracted.normalized_pdf else {
            return Err(AppError::validation(
                "normalizedPdf",
                "无法从该原件生成归一化 PDF，请更换清晰、完整的原件后重试",
            ));
        };

        let destination =
            tokio::task::spawn_blocking(move || paths.persist_normalized_pdf(id, &bytes))
                .await
                .map_err(|error| AppError::Internal {
                    message: format!("normalized PDF storage task failed: {error}"),
                })??;
        self.items
            .update_fields(
                id,
                ItemPatch {
                    normalized_pdf_path: Some(Some(destination.to_string_lossy().into_owned())),
                    ..ItemPatch::default()
                },
            )
            .await
    }

    async fn persist_recognition_patch(
        &self,
        id: uuid::Uuid,
        patch: ItemPatch,
        preserve_manual_confirmation: bool,
    ) -> Result<InvoiceItem, AppError> {
        if preserve_manual_confirmation {
            self.items.update_recognition_fields(id, patch).await
        } else {
            self.items.update_fields(id, patch).await
        }
    }
}

impl RecognitionOutcome {
    pub fn status(&self, dedupe_status: DedupeStatus) -> ItemStatus {
        derive_item_status(
            self.recognition_status,
            self.confirmation_status,
            dedupe_status,
        )
    }
}

pub fn recognize(text: &str, received_date: NaiveDate) -> RecognitionOutcome {
    recognize_with_warnings(text, received_date, &[])
}

/// Prefix shared by every reason this module writes into `items.note`.
const RECOGNITION_NOTE_PREFIX: &str = "识别失败：";

/// Whether a note was written by this pipeline instead of typed by the user.
fn is_recognition_note(note: Option<&str>) -> bool {
    note.is_some_and(|note| note.trim_start().starts_with(RECOGNITION_NOTE_PREFIX))
}

/// Build a note update for a failed attempt.
///
/// The item drawer exposes `note` as a free-text remark, so a remark the user
/// typed is never overwritten; only an empty note or an older reason from this
/// pipeline is replaced.
fn reason_note(existing: &Option<String>, reason: &str) -> Option<Option<String>> {
    let trimmed = existing.as_deref().map(str::trim);
    if trimmed.is_some_and(|text| !text.is_empty()) && !is_recognition_note(existing.as_deref()) {
        return None;
    }
    let note = format!("{RECOGNITION_NOTE_PREFIX}{reason}");
    (existing.as_deref() != Some(note.as_str())).then_some(Some(note))
}

fn original_extension(name: &str) -> String {
    std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| format!(".{}", extension.to_ascii_lowercase()))
        .unwrap_or_else(|| "（无扩展名）".to_owned())
}

/// Reason for a format the extractor accepts as a file but cannot read.
///
/// Word, Excel and archive originals used to come back as "recognized but
/// awaiting confirmation" with no text and no normalized PDF, so they looked
/// healthy in the inbox while being impossible to export. Say so instead.
fn unsupported_format_note(original_name: &str) -> String {
    format!(
        "原件格式 {} 不支持自动识别，也无法生成归一化 PDF；请人工填写日期与金额并改用 PDF/JPG/PNG 原件，或将其移出批次",
        original_extension(original_name)
    )
}

/// Reason for a readable original whose total amount still could not be parsed.
///
/// The literal marker is part of the note so the same string can be searched in
/// logs and shown to the user without a second lookup.
fn amount_not_parsed_note(original_name: &str) -> String {
    format!(
        "已从原件格式 {} 识别到文字，但没有解析出金额（{OCR_TEXT_WITHOUT_AMOUNT}）；请人工核对金额，或更换更清晰的原件后重新识别",
        original_extension(original_name)
    )
}

/// Reason for an extractor error, in terms a user can act on.
fn extraction_failure_note(original_name: &str, error: &AppError) -> String {
    let detail = error.to_string();
    let detail = detail.trim();
    let explained = if detail.contains("exceeds extraction resource limits") {
        "文件过于复杂或超出处理上限"
    } else if detail.contains("PDF uses an unsupported structure") {
        "PDF 使用了暂不支持的结构（如加密或特殊压缩），请另存为标准 PDF 后重试"
    } else if detail.contains("unsupported") {
        "该格式不支持自动识别"
    } else if detail.contains("password") || detail.contains("encrypt") {
        "文件已加密，需要先解除密码"
    } else if detail.is_empty() {
        "读取失败"
    } else {
        "文件损坏或不是有效的发票文件"
    };
    format!(
        "原件格式 {} {explained}；请改用 PDF/JPG/PNG 原件，或人工录入后移出待处理队列",
        original_extension(original_name)
    )
}

pub fn recognize_with_warnings(
    text: &str,
    received_date: NaiveDate,
    extraction_warnings: &[String],
) -> RecognitionOutcome {
    let (recognized_date, invalid_date) = labeled_date(text);
    let invoice_date = recognized_date;
    let mut warnings = extraction_warnings.to_vec();
    if invalid_date {
        warnings.push("invalid_invoice_date".to_owned());
    }
    let amount_cents = match labeled_amount(text) {
        Ok(amount) => amount,
        Err(()) => {
            warnings.push("invalid_amount".to_owned());
            None
        }
    };
    // "The OCR read nothing" and "the OCR read text but no total" must not look
    // the same to the UI: the first is a recognition problem, the second one is
    // a layout/parse problem the user can still repair by filling the amount in.
    if amount_cents.is_none()
        && !warnings.iter().any(|warning| warning == "invalid_amount")
        && text.chars().any(|character| !character.is_whitespace())
    {
        warnings.push(OCR_TEXT_WITHOUT_AMOUNT.to_owned());
    }
    // The user's rule: when none of the four real categories matches, the
    // invoice is classified as "其他" instead of being left without a category
    // (which kept it out of every reimbursement batch).
    let category = Some(scored_category(text).unwrap_or(Category::Other));
    let company = labeled_company_value(text, "销售方名称")
        .or_else(|| company_value_from_parallel_ocr_sections(text, 1))
        .or_else(|| company_value_from_compact_section(text, "销售方信息"))
        .or_else(|| company_value_from_ocr_section(text, &["销售方信息", "销 售 方 信 息"]))
        .or_else(|| flattened_seller_company(text))
        .or_else(|| labeled_company_value(text, "购买方名称"))
        .or_else(|| company_value_from_parallel_ocr_sections(text, 0))
        .or_else(|| company_value_from_compact_section(text, "购买方信息"))
        .or_else(|| company_value_from_ocr_section(text, &["购买方信息", "购 买 方 信 息"]));
    let city = company
        .as_deref()
        .and_then(city_from_company_name)
        .or_else(|| recognized_city(text));
    let period_date = recognized_date.unwrap_or(received_date);
    let suggested_period = format!("{:04}-{:02}", period_date.year(), period_date.month());
    let confirmation_status = if invoice_date.is_some()
        && amount_cents.is_some()
        && category.is_some()
        && warnings.is_empty()
    {
        ConfirmationStatus::Confirmed
    } else {
        ConfirmationStatus::Pending
    };

    RecognitionOutcome {
        invoice_date,
        suggested_period,
        category,
        amount_cents,
        city,
        company,
        warnings,
        recognition_status: RecognitionStatus::Succeeded,
        confirmation_status,
    }
}

fn recognized_city(text: &str) -> Option<String> {
    if let Some(city) = CITY_NAMES
        .iter()
        .filter(|city| text.contains(&format!("{city}市")))
        .max_by_key(|city| city.chars().count())
    {
        return Some((*city).to_owned());
    }

    CITY_NAMES
        .iter()
        .filter(|city| {
            text.match_indices(*city)
                .any(|(index, _)| bare_city_has_location_context(text, index, city))
        })
        .max_by_key(|city| city.chars().count())
        .map(|city| (*city).to_owned())
}

fn city_from_company_name(company: &str) -> Option<String> {
    CITY_NAMES
        .iter()
        .filter(|city| company.contains(**city))
        .max_by_key(|city| city.chars().count())
        .map(|city| (*city).to_owned())
}

fn bare_city_has_location_context(text: &str, start: usize, city: &str) -> bool {
    const CONTEXT_LABELS: &[&str] = &[
        "地址",
        "地点",
        "城市",
        "所在地",
        "出发地",
        "到达地",
        "出发",
        "到达",
    ];
    const NON_CITY_SUFFIXES: &[&str] =
        &["路", "街", "道", "巷", "弄", "大道", "公路", "高速", "牌"];

    let end = start + city.len();
    let after_city = &text[end..];
    if NON_CITY_SUFFIXES
        .iter()
        .any(|suffix| after_city.starts_with(suffix))
    {
        return false;
    }

    let line_start = text[..start].rfind('\n').map_or(0, |newline| newline + 1);
    let line_end = after_city
        .find('\n')
        .map_or(text.len(), |newline| end + newline);
    let before_city = &text[line_start..start];
    let context_before = CONTEXT_LABELS.iter().any(|label| {
        before_city.rfind(label).is_some_and(|label_start| {
            before_city[label_start + label.len()..].chars().count() <= 12
        })
    });
    let after_city_on_line =
        after_city[..line_end - end].trim_start_matches([' ', '\t', '：', ':']);
    let context_after = ["出发", "到达"]
        .iter()
        .any(|label| after_city_on_line.starts_with(label));

    context_before || context_after
}

fn labeled_company_value(text: &str, label: &str) -> Option<String> {
    const END_LABELS: &[&str] = &[
        "购买方名称",
        "销售方名称",
        "购买方纳税人识别号",
        "销售方纳税人识别号",
        "纳税人识别号",
        "购买方税号",
        "销售方税号",
        "统一社会信用代码",
        "税号",
        "地址、电话",
        "地址电话",
        "地址",
        "电话",
        "开户行及账号",
        "开户行",
        "账号",
    ];

    text.match_indices(label)
        .filter(|(index, _)| company_label_has_field_boundary(text, *index, label))
        .find_map(|(index, _)| company_value_after_label(&text[index + label.len()..], END_LABELS))
}

fn company_value_from_ocr_section(text: &str, section_labels: &[&str]) -> Option<String> {
    const END_LABELS: &[&str] = &[
        "名称",
        "统一社会信用代码",
        "纳税人识别号",
        "税号",
        "地址",
        "电话",
        "项目名称",
    ];

    section_labels.iter().find_map(|section_label| {
        let (_, after_section) = text.split_once(section_label)?;
        let section = after_section
            .split_once("项目名称")
            .map_or(after_section, |(value, _)| value);
        section.match_indices("名称").find_map(|(index, _)| {
            let starts_field = section[..index]
                .chars()
                .next_back()
                .is_none_or(|character| character.is_whitespace() || character == '|');
            let after_label = &section[index + "名称".len()..];
            let has_separator = after_label
                .trim_start_matches([' ', '\t'])
                .starts_with(['：', ':']);
            (starts_field && has_separator)
                .then(|| company_value_after_label(after_label, END_LABELS))
                .flatten()
        })
    })
}

fn company_value_from_parallel_ocr_sections(text: &str, index: usize) -> Option<String> {
    let buyer_header = text.find("购买方信息")?;
    let seller_header = text.find("销售方信息")?;
    let first_name = text.find("名称")?;
    if buyer_header > first_name || seller_header > first_name {
        return None;
    }

    const END_LABELS: &[&str] = &[
        "名称",
        "统一社会信用代码",
        "纳税人识别号",
        "税号",
        "地址",
        "电话",
        "项目名称",
    ];
    text.match_indices("名称")
        .filter(|(position, _)| company_label_has_field_boundary(text, *position, "名称"))
        .filter_map(|(position, _)| {
            company_value_after_label(&text[position + "名称".len()..], END_LABELS)
        })
        .nth(index)
}

fn flattened_seller_company(text: &str) -> Option<String> {
    if !is_known_flattened_invoice_template(text) {
        return None;
    }
    let (_, tail) = text
        .split_once("开票人：")
        .or_else(|| text.split_once("开票人:"))?;
    let fields = tail
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let mut company_tax_pairs = fields.windows(2).filter_map(|pair| {
        (looks_like_company_name(pair[0]) && looks_like_tax_id(pair[1])).then_some(pair[0])
    });

    company_tax_pairs.nth(1).map(str::to_owned)
}

fn semantic_invoice_fingerprint(
    text: &str,
    amount_cents: Option<i64>,
    city: Option<&str>,
) -> Option<String> {
    let invoice_number = labeled_ascii_identifier(text, &["发票号码", "发票号"])
        .or_else(|| flattened_invoice_number(text))?;
    let seller_tax_id = labeled_ascii_identifier(
        text,
        &["销售方纳税人识别号", "销售方统一社会信用代码", "销售方税号"],
    )
    .or_else(|| flattened_seller_tax_id(text))?;
    let amount_cents = amount_cents?;
    let city = city?.trim();
    if city.is_empty() {
        return None;
    }

    let identity = format!("{invoice_number}|{seller_tax_id}|{amount_cents}|{city}");
    Some(format!("{:x}", Sha256::digest(identity.as_bytes())))
}

fn labeled_ascii_identifier(text: &str, labels: &[&str]) -> Option<String> {
    labels.iter().find_map(|label| {
        text.match_indices(label).find_map(|(index, _)| {
            let value = text[index + label.len()..].trim_start_matches(|character: char| {
                character.is_whitespace() || matches!(character, '：' | ':')
            });
            let identifier: String = value
                .chars()
                .take_while(|character| character.is_ascii_alphanumeric())
                .collect();
            valid_invoice_identifier(&identifier).then_some(identifier)
        })
    })
}

fn valid_invoice_identifier(value: &str) -> bool {
    (8..=24).contains(&value.len())
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

fn flattened_invoice_number(text: &str) -> Option<String> {
    let record = flattened_invoice_record(text)?;
    let number = record.tail[..record.date_offset].trim();
    valid_invoice_identifier(number).then(|| number.to_owned())
}

fn flattened_seller_tax_id(text: &str) -> Option<String> {
    if !is_known_flattened_invoice_template(text) {
        return None;
    }
    let (_, tail) = text
        .split_once("开票人：")
        .or_else(|| text.split_once("开票人:"))?;
    let fields = tail
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let mut company_tax_pairs = fields.windows(2).filter_map(|pair| {
        (looks_like_company_name(pair[0]) && looks_like_tax_id(pair[1])).then_some(pair[1])
    });

    company_tax_pairs.nth(1).map(str::to_owned)
}

fn looks_like_company_name(value: &str) -> bool {
    let length = value.chars().count();
    (4..=80).contains(&length)
        && value
            .chars()
            .any(|character| ('\u{4e00}'..='\u{9fff}').contains(&character))
        && !value.contains(['：', ':', '¥', '￥'])
        && !["发票", "项目", "合计", "备注", "开票人"]
            .iter()
            .any(|label| value.contains(label))
}

fn looks_like_tax_id(value: &str) -> bool {
    let length = value.len();
    (15..=20).contains(&length)
        && value
            .chars()
            .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
        && value
            .chars()
            .filter(|character| character.is_ascii_digit())
            .count()
            >= 6
}

fn company_value_from_compact_section(text: &str, section_label: &str) -> Option<String> {
    const END_LABELS: &[&str] = &[
        "名称",
        "购买方信息",
        "销售方信息",
        "统一社会信用代码",
        "纳税人识别号",
        "税号",
        "地址",
        "电话",
        "项目名称",
    ];

    let compact: String = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let (_, after_section) = compact.split_once(section_label)?;
    let (_, after_name) = after_section.split_once("名称")?;
    let value = after_name.trim_start_matches(['：', ':']);
    let end = END_LABELS
        .iter()
        .filter_map(|label| value.find(label))
        .min()
        .unwrap_or(value.len());
    let company = value[..end].trim_matches([
        '：', ':', '，', ',', '。', '.', '；', ';', '（', '(', '）', ')',
    ]);
    (!company.is_empty() && company.ends_with("公司")).then(|| company.to_owned())
}

fn company_label_has_field_boundary(text: &str, index: usize, label: &str) -> bool {
    let starts_field = text[..index]
        .chars()
        .next_back()
        .is_none_or(|character| character.is_whitespace() || character == '|');
    let after_label = text[index + label.len()..].trim_start_matches([' ', '\t']);
    starts_field && after_label.starts_with(['：', ':'])
}

fn company_value_after_label(after_label: &str, end_labels: &[&str]) -> Option<String> {
    let line = after_label
        .trim_start_matches([' ', '\t'])
        .trim_start_matches(['：', ':'])
        .lines()
        .next()?
        .trim_start_matches([' ', '\t']);
    let end = end_labels
        .iter()
        .filter_map(|end_label| line.find(end_label))
        .min()
        .unwrap_or(line.len());
    let value = line[..end].trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn labeled_date(text: &str) -> (Option<NaiveDate>, bool) {
    if let Some((_, after_invoice_label)) = text.split_once("开票日期") {
        if let Some(date) = parse_labeled_date_value(after_invoice_label) {
            return (Some(date), false);
        }
        let (generic_date, _) = generic_labeled_date(text);
        if generic_date.is_some() {
            return (generic_date, true);
        }
        let flattened_date = flattened_invoice_date(text);
        let labeled_value = after_invoice_label
            .trim_start_matches(['：', ':'])
            .trim_start_matches([' ', '\t']);
        let empty_labeled_value = labeled_value.is_empty()
            || labeled_value.starts_with('\n')
            || labeled_value.starts_with("\r\n");
        return (
            flattened_date,
            flattened_date.is_none() || !empty_labeled_value,
        );
    }

    generic_labeled_date(text)
}

fn generic_labeled_date(text: &str) -> (Option<NaiveDate>, bool) {
    let mut invalid = false;
    for (index, _) in text.match_indices("日期") {
        let explicit_boundary = text[..index].chars().next_back().is_none_or(|character| {
            character.is_whitespace()
                || matches!(character, ':' | '：' | ',' | '，' | ';' | '；' | '。')
        });
        if !explicit_boundary {
            continue;
        }

        let after_label = &text[index + "日期".len()..];
        if let Some(date) = parse_labeled_date_value(after_label) {
            return (Some(date), invalid);
        }
        invalid = true;
    }
    (None, invalid)
}

fn parse_labeled_date_value(after_label: &str) -> Option<NaiveDate> {
    let value = after_label
        .trim_start_matches(['：', ':'])
        .trim_start_matches([' ', '\t']);
    parse_date_prefix(value)
}

fn parse_date_prefix(value: &str) -> Option<NaiveDate> {
    [(11, "%Y年%m月%d日"), (10, "%Y-%m-%d")]
        .into_iter()
        .find_map(|(length, format)| {
            let candidate: String = value.chars().take(length).collect();
            (candidate.chars().count() == length)
                .then(|| NaiveDate::parse_from_str(&candidate, format).ok())
                .flatten()
        })
}

fn flattened_invoice_date(text: &str) -> Option<NaiveDate> {
    let record = flattened_invoice_record(text)?;
    parse_date_prefix(&record.tail[record.date_offset..])
}

struct FlattenedInvoiceRecord<'a> {
    tail: &'a str,
    date_offset: usize,
}

fn flattened_invoice_record(text: &str) -> Option<FlattenedInvoiceRecord<'_>> {
    if !is_known_flattened_invoice_template(text) {
        return None;
    }

    let tail = tail_after_remarks_label(text)?;
    tail.char_indices().take(256).find_map(|(index, _)| {
        parse_date_prefix(&tail[index..])?;
        let before_date = &tail[..index];
        let invoice_number = before_date.split_whitespace().next_back()?;
        let valid_invoice_number = (8..=24).contains(&invoice_number.len())
            && invoice_number
                .chars()
                .all(|character| character.is_ascii_digit());
        let invoice_number_start = before_date.rfind(invoice_number)?;
        valid_invoice_number.then_some(FlattenedInvoiceRecord {
            tail: &tail[invoice_number_start..],
            date_offset: index - invoice_number_start,
        })
    })
}

fn tail_after_remarks_label(text: &str) -> Option<&str> {
    if let Some((_, tail)) = text.split_once("备注") {
        return Some(tail);
    }
    text.match_indices('备').find_map(|(index, _)| {
        let line_start = text[..index].rfind('\n').map_or(0, |newline| newline + 1);
        if !text[line_start..index].chars().all(char::is_whitespace) {
            return None;
        }
        let after_first_character = &text[index + "备".len()..];
        let after_whitespace = after_first_character.trim_start_matches(char::is_whitespace);
        if after_whitespace.len() == after_first_character.len() {
            return None;
        }
        let tail = after_whitespace.strip_prefix('注')?;
        tail.chars()
            .next()
            .is_none_or(|character| character.is_whitespace() || matches!(character, '：' | ':'))
            .then_some(tail)
    })
}

fn is_known_flattened_invoice_template(text: &str) -> bool {
    const TEMPLATE_SIGNALS: &[&str] = &[
        "电子发票",
        "发票号码",
        "购买方信息",
        "销售方信息",
        "项目名称",
        "价税合计",
        "备注",
    ];
    let has_empty_invoice_date = text
        .lines()
        .map(str::trim)
        .any(|line| matches!(line, "开票日期：" | "开票日期:"));
    let empty_company_names = text.matches("名称：").count() + text.matches("名称:").count();
    let compact: String = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    has_empty_invoice_date
        && empty_company_names >= 2
        && TEMPLATE_SIGNALS
            .iter()
            .all(|signal| compact.contains(signal))
}

/// Stable marker for "the OCR returned text, but no total amount could be
/// parsed from it".
///
/// The item note keeps this literal token, so the UI can tell "the OCR read
/// nothing at all" (empty text) from "the OCR read text and the amount needs a
/// human". It never changes the automatic confirmation rule: an invoice without
/// an amount was pending before this marker existed and stays pending.
pub const OCR_TEXT_WITHOUT_AMOUNT: &str = "ocr_text_without_amount";

/// Labels that name the invoice total outright, in priority order.
const STRONG_AMOUNT_LABELS: &[&str] = &[
    "合计金额",
    "金额合计",
    "合 计 金 额",
    "应付金额",
    "实付金额",
    "行程费用合计",
    "总金额",
    "小写金额",
    "小写",
];

/// The bare `合计` row of the item table. Only trusted when the number that
/// follows really looks like money.
const WEAK_AMOUNT_LABELS: &[&str] = &["合计", "合 计"];

/// Markers an amount can be prefixed with. OCR often separates `¥` from the
/// digits, so the whitespace between them is stripped before parsing.
const AMOUNT_CURRENCY_MARKERS: &[&str] = &["人民币", "RMB", "CNY", "¥", "￥"];

/// Units that follow counts and rates, never a total amount.
const NON_AMOUNT_UNITS: &[&str] = &[
    "个", "件", "张", "项", "天", "人", "次", "台", "份", "套", "箱", "包", "盒", "瓶", "条", "块",
    "米", "千克", "公斤", "克", "斤", "吨", "度", "小时", "分钟", "年", "月", "日", "%", "％",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum AmountLabelStrength {
    Strong,
    Weak,
}

enum AmountCandidate {
    /// The label was followed by a valid amount.
    Parsed(i64),
    /// The label was followed by something number-like but malformed.
    Malformed,
    /// The label was followed by a rate or a count instead of money.
    NotMoney,
    /// The label was followed by no number at all.
    Empty,
    /// No amount label exists in the text at all.
    Absent,
}

/// Value of a labelled invoice total in cents.
///
/// Labels are searched in priority order: the `价税合计` row first, then the
/// other total labels, then the bare `合计` row. A token that cannot be an
/// amount (`税率 1%`, `数量 18 个`) is ignored instead of being read as money,
/// while a malformed amount (`1,23.45`, `12.345`, `-1.00`) still reports
/// `invalid_amount` as before.
fn labeled_amount(text: &str) -> Result<Option<i64>, ()> {
    let normalized = normalize_amount_text(text);
    let text = normalized.as_str();
    match amount_candidate(text) {
        AmountCandidate::Parsed(cents) => Ok(Some(cents)),
        AmountCandidate::Malformed => Err(()),
        // A total row whose number is a rate, a count, or missing entirely:
        // fall back to the largest currency marked value like before.
        AmountCandidate::NotMoney | AmountCandidate::Empty => Ok(max_currency_amount(text)),
        // No total label at all: never guess from an unrelated currency value
        // such as the tax amount of the item table.
        AmountCandidate::Absent => Ok(None),
    }
}

fn amount_candidate(text: &str) -> AmountCandidate {
    if let Some((_, tail)) = text.split_once("价税合计") {
        return amount_after_label(small_amount_tail(tail), AmountLabelStrength::Strong);
    }
    if let Some(tail) = first_label_tail(text, STRONG_AMOUNT_LABELS) {
        return amount_after_label(tail, AmountLabelStrength::Strong);
    }
    if let Some(tail) = first_label_tail(text, WEAK_AMOUNT_LABELS) {
        return amount_after_label(tail, AmountLabelStrength::Weak);
    }
    AmountCandidate::Absent
}

/// Tail of the `价税合计` row: the uppercase total is skipped when the lowercase
/// row follows it, and a `金额` suffix (`价税合计金额`) is not part of the value.
fn small_amount_tail(tail: &str) -> &str {
    let tail = match tail.find("小写") {
        Some(index) if tail[..index].chars().count() <= 48 => &tail[index + "小写".len()..],
        _ => tail,
    };
    tail.strip_prefix("金额").unwrap_or(tail)
}

fn first_label_tail<'a>(text: &'a str, labels: &[&str]) -> Option<&'a str> {
    labels
        .iter()
        .find_map(|label| text.split_once(label).map(|(_, tail)| tail))
}

fn amount_after_label(after_label: &str, strength: AmountLabelStrength) -> AmountCandidate {
    let value = strip_amount_prefix(after_label);
    let token: String = value
        .text
        .chars()
        .take_while(|character| {
            character.is_ascii_digit() || matches!(*character, '.' | ',' | '-' | '+')
        })
        .collect();
    if token.is_empty() {
        return AmountCandidate::Empty;
    }
    // The token only contains ASCII characters, so its length in bytes is also
    // the offset of the characters that follow it.
    let rest = &value.text[token.len()..];
    let Some(cents) = parse_amount_cents(&token) else {
        return AmountCandidate::Malformed;
    };
    if is_non_amount_suffix(rest) {
        return AmountCandidate::NotMoney;
    }
    if strength == AmountLabelStrength::Weak
        && !weak_amount_is_credible(&token, value.has_currency_marker, rest)
    {
        return AmountCandidate::NotMoney;
    }
    AmountCandidate::Parsed(cents)
}

struct AmountValue<'a> {
    text: &'a str,
    has_currency_marker: bool,
}

/// Strips separators and currency markers that sit between a label and its
/// value, remembering whether one of them was a currency marker.
fn strip_amount_prefix(value: &str) -> AmountValue<'_> {
    let mut rest = value;
    let mut has_currency_marker = false;
    loop {
        let trimmed = rest.trim_start_matches(|character: char| {
            character.is_whitespace()
                || matches!(character, '：' | ':' | '）' | ')' | '（' | '(' | '|')
        });
        let mut next = trimmed;
        for marker in AMOUNT_CURRENCY_MARKERS {
            if let Some(remainder) = next.strip_prefix(marker) {
                has_currency_marker = true;
                next = remainder;
                break;
            }
        }
        if next.len() == trimmed.len() {
            return AmountValue {
                text: next.trim_start(),
                has_currency_marker,
            };
        }
        rest = next;
    }
}

/// Whether the characters right after a number prove that it is not money.
fn is_non_amount_suffix(rest: &str) -> bool {
    let rest = rest.trim_start();
    rest.starts_with(['%', '％']) || NON_AMOUNT_UNITS.iter().any(|unit| rest.starts_with(unit))
}

/// A bare `合计` row is only trusted when the number itself looks like money:
/// two decimals, a currency marker, or an explicit `元`/`圆` suffix. Otherwise
/// `合计 18` (a count) and `合计 1` (a rate) are indistinguishable from money.
fn weak_amount_is_credible(token: &str, has_currency_marker: bool, rest: &str) -> bool {
    let two_decimals = token
        .split_once('.')
        .is_some_and(|(_, fraction)| fraction.len() == 2);
    let followed_by_yuan = rest.trim_start().starts_with(['元', '圆']);
    two_decimals || has_currency_marker || followed_by_yuan
}

fn max_currency_amount(text: &str) -> Option<i64> {
    text.char_indices()
        .filter_map(|(index, _character)| {
            AMOUNT_CURRENCY_MARKERS
                .iter()
                .find(|marker| text[index..].starts_with(**marker))
                .and_then(|marker| parse_amount_prefix(&text[index + marker.len()..]))
        })
        .max()
}

fn parse_amount_prefix(value: &str) -> Option<i64> {
    let value = value.trim_start();
    let token: String = value
        .chars()
        .take_while(|character| character.is_ascii_digit() || matches!(*character, '.' | ','))
        .collect();
    parse_amount_cents(&token)
}

/// Maps the full width digits and separators OCR returns into the ASCII forms
/// the amount grammar understands. A full width sentence period between two
/// digits is a misread decimal point, so it becomes one as well.
fn normalize_amount_text(text: &str) -> String {
    let characters: Vec<char> = text
        .chars()
        .map(|character| match character {
            '\u{FF10}'..='\u{FF19}' => {
                let offset = u32::from(character) - u32::from('\u{FF10}');
                char::from_digit(offset, 10).unwrap_or(character)
            }
            '\u{FF0C}' => ',',
            '\u{FF0E}' => '.',
            '\u{FF0B}' => '+',
            '\u{FF0D}' | '\u{2212}' => '-',
            '\u{FF05}' => '%',
            '\u{3000}' => ' ',
            _ => character,
        })
        .collect();
    characters
        .iter()
        .enumerate()
        .map(|(index, character)| {
            let surrounded_by_digits = index > 0
                && index + 1 < characters.len()
                && characters[index - 1].is_ascii_digit()
                && characters[index + 1].is_ascii_digit();
            if *character == '。' && surrounded_by_digits {
                '.'
            } else {
                *character
            }
        })
        .collect()
}

fn parse_amount_cents(token: &str) -> Option<i64> {
    let mut decimal_parts = token.split('.');
    let integer = decimal_parts.next()?;
    let fraction = decimal_parts.next();
    if decimal_parts.next().is_some()
        || integer.is_empty()
        || !fraction.is_none_or(|fraction| {
            (1..=2).contains(&fraction.len())
                && fraction.chars().all(|value| value.is_ascii_digit())
        })
    {
        return None;
    }

    let groups: Vec<_> = integer.split(',').collect();
    let valid_integer = if groups.len() == 1 {
        integer.chars().all(|value| value.is_ascii_digit())
    } else {
        (1..=3).contains(&groups[0].len())
            && groups[0].chars().all(|value| value.is_ascii_digit())
            && groups[1..]
                .iter()
                .all(|group| group.len() == 3 && group.chars().all(|value| value.is_ascii_digit()))
    };
    if !valid_integer {
        return None;
    }

    let amount = token.replace(',', "").parse::<Decimal>().ok()?;
    amount.checked_mul(Decimal::from(100))?.to_i64()
}

fn scored_category(text: &str) -> Option<Category> {
    const RULES: [(Category, &[&str]); 4] = [
        (
            Category::Transport,
            &[
                "出租车",
                "网约车",
                "铁路",
                "航空",
                "客运",
                "滴滴",
                "旅客运输",
                "交通运输",
                "行程单",
                "打车",
                "出行",
                "机票",
                "通行费",
                "车牌号",
                "车辆类型",
            ],
        ),
        (Category::Dining, &["餐饮", "食品", "饭店", "餐厅"]),
        (Category::Accommodation, &["住宿", "酒店", "宾馆"]),
        (Category::Hospitality, &["招待", "礼品", "会务"]),
    ];

    let mut best = None;
    let mut best_score = 0;
    let mut tied = false;
    for (category, keywords) in RULES {
        let mut score = keywords
            .iter()
            .filter(|keyword| text.contains(**keyword))
            .count();
        if category == Category::Dining && text.contains("餐饮服务") {
            score += 1;
        }
        if score > best_score {
            best = Some(category);
            best_score = score;
            tied = false;
        } else if score == best_score {
            tied = true;
        }
    }

    (best_score >= 2 && !tied).then_some(best).flatten()
}

#[cfg(test)]
mod note_tests {
    use super::*;

    #[test]
    fn an_empty_note_receives_the_failure_reason() {
        let note = reason_note(&None, "原件格式 .docx 不支持自动识别")
            .expect("failing recognition must explain itself");
        assert!(
            note.expect("reason is present")
                .starts_with(RECOGNITION_NOTE_PREFIX)
        );
    }

    #[test]
    fn a_user_remark_is_never_overwritten() {
        let existing = Some("张三的餐费".to_owned());
        assert_eq!(
            reason_note(&existing, "原件格式 .docx 不支持自动识别"),
            None
        );
    }

    #[test]
    fn a_previous_reason_is_replaced_by_the_new_one() {
        let existing = Some(format!("{RECOGNITION_NOTE_PREFIX}旧的原因"));
        let note = reason_note(&existing, "新的原因").expect("stale reasons are refreshed");
        assert_eq!(note, Some(format!("{RECOGNITION_NOTE_PREFIX}新的原因")));
    }

    #[test]
    fn an_unchanged_reason_writes_nothing() {
        let existing = Some(format!("{RECOGNITION_NOTE_PREFIX}同样的原因"));
        assert_eq!(reason_note(&existing, "同样的原因"), None);
    }

    #[test]
    fn only_our_own_notes_are_recognized() {
        assert!(is_recognition_note(Some("  识别失败：x")));
        assert!(!is_recognition_note(Some("invoice link download failed")));
        assert!(!is_recognition_note(None));
    }

    #[test]
    fn unsupported_formats_name_the_extension() {
        let note = unsupported_format_note("发票.DOCX");
        assert!(note.contains(".docx"), "{note}");
        assert!(note.contains("PDF"));
    }

    #[test]
    fn a_damaged_document_is_explained_without_leaking_details() {
        let error = AppError::External {
            service: "document_extractor".to_owned(),
            retryable: false,
            message: "Unable to read document.".to_owned(),
        };
        let note = extraction_failure_note("scan.pdf", &error);
        assert!(note.contains(".pdf"), "{note}");
        assert!(!note.contains("Unable to read"), "{note}");
    }

    #[test]
    fn resource_limits_are_explained() {
        let error = AppError::External {
            service: "document_extractor".to_owned(),
            retryable: false,
            message: "Document exceeds extraction resource limits.".to_owned(),
        };
        assert!(extraction_failure_note("big.pdf", &error).contains("超出处理上限"));
    }

    #[test]
    fn unsupported_pdf_structures_are_not_reported_as_resource_limits() {
        let error = AppError::External {
            service: "document_extractor".to_owned(),
            retryable: false,
            message: "PDF uses an unsupported structure.".to_owned(),
        };
        let note = extraction_failure_note("special.pdf", &error);

        assert!(note.contains("暂不支持的结构"), "{note}");
        assert!(!note.contains("超出处理上限"), "{note}");
        assert!(!note.contains("该格式不支持"), "{note}");
    }

    #[test]
    fn encrypted_pdfs_ask_the_user_to_unlock_the_file() {
        let error = AppError::External {
            service: "document_extractor".to_owned(),
            retryable: false,
            message: "Document is encrypted; unlock the PDF before importing.".to_owned(),
        };
        let note = extraction_failure_note("locked.pdf", &error);

        assert!(note.contains("已加密"), "{note}");
        assert!(!note.contains("超出处理上限"), "{note}");
        assert!(!note.contains("暂不支持的结构"), "{note}");
    }
}

#[cfg(test)]
mod amount_tests {
    use super::*;

    fn cents(text: &str) -> Option<i64> {
        labeled_amount(text).expect("a valid amount must not be reported as invalid")
    }

    #[test]
    fn lowercase_total_labels_are_supported() {
        for (text, expected) in [
            ("价税合计（小写）¥648.00", 64_800),
            ("价税合计(小写)：￥648.00", 64_800),
            ("价税合计 (小写)\n￥1014.00", 101_400),
            ("价税合计金额：648.00", 64_800),
            ("合计金额 ¥1,234.56", 123_456),
            ("金额合计：￥1,234.56", 123_456),
            ("小写：648.00", 64_800),
            ("应付金额 648.00元", 64_800),
            ("行程费用合计 ￥1014.00", 101_400),
        ] {
            assert_eq!(cents(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn full_width_digits_and_separators_are_normalized() {
        for (text, expected) in [
            ("价税合计（小写）￥６４８．００", 64_800),
            ("价税合计（小写）￥１，２３４．５６", 123_456),
            ("合计金额 ６４８．００", 64_800),
            ("价税合计（小写）648。00", 64_800),
        ] {
            assert_eq!(cents(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn currency_markers_tolerate_whitespace_and_thousands_grouping() {
        for (text, expected) in [
            ("价税合计（小写）¥ 648.00", 64_800),
            ("价税合计（小写） ￥  1,234.56", 123_456),
            ("价税合计（小写）RMB 1,234.56", 123_456),
            ("价税合计（小写）人民币 1,234.56", 123_456),
            ("价税合计（小写）CNY 648.00", 64_800),
        ] {
            assert_eq!(cents(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn rates_and_counts_are_not_read_as_the_total() {
        for text in [
            "税率 1% 数量 18",
            "税率 1% 合计 1",
            "合计 18 个",
            "合计 1%",
            "价税合计（小写）1%",
            "数量 18 单价 6.42 合计",
        ] {
            assert_eq!(cents(text), None, "{text}");
        }
        assert_eq!(cents("税额 ¥6.42 税率 1% 数量 18"), None);
    }

    #[test]
    fn malformed_amounts_still_report_invalid() {
        for text in [
            "价税合计 ￥1,23.45",
            "价税合计 ￥12.345",
            "价税合计 ￥-1.00",
            "价税合计 ￥92233720368547758.08",
        ] {
            assert!(
                labeled_amount(text).is_err(),
                "malformed amount accepted: {text}"
            );
        }
    }

    #[test]
    fn the_full_total_row_wins_over_the_item_table_rows() {
        for (text, expected) in [
            ("税额 ￥9.99 合计 ￥100.00 价税合计 ￥200.00", 20_000),
            (
                "价税合计（大写）壹佰捌拾壹圆叁角陆分\n（小写）¥181.36",
                18_136,
            ),
            ("合计 ￥100.00\n价税合计（小写）￥1,234.56", 123_456),
        ] {
            assert_eq!(cents(text), Some(expected), "{text}");
        }
    }
}
