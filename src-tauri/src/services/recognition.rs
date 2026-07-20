use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::db::items::{InvoiceItem, ItemPatch, ItemRepository};
use crate::domain::error::AppError;
use crate::domain::model::{
    Category, ConfirmationStatus, DedupeStatus, ItemStatus, RecognitionStatus, derive_item_status,
};
use crate::infra::extraction::DocumentExtractor;
use crate::infra::files::AppPaths;

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
        let received_date = item.fetched_at.date_naive();
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
        let recognized =
            recognize_with_warnings(&extracted.text, received_date, &extracted.warnings);

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
                    amount_cents: Some(recognized.amount_cents),
                    city: Some(recognized.city),
                    company: Some(recognized.company),
                    recognition_status: Some(recognized.recognition_status),
                    confirmation_status: Some(recognized.confirmation_status),
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
            Ok(item) => Ok(item),
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
    let category = scored_category(text);
    let company = labeled_company_value(text, "购买方名称")
        .or_else(|| company_value_from_compact_section(text, "购买方信息"))
        .or_else(|| company_value_from_ocr_section(text, &["购买方信息", "购 买 方 信 息"]))
        .or_else(|| labeled_company_value(text, "销售方名称"))
        .or_else(|| company_value_from_compact_section(text, "销售方信息"))
        .or_else(|| company_value_from_ocr_section(text, &["销售方信息", "销 售 方 信 息"]));
    let city = recognized_city(text);
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
        return (flattened_invoice_date(text), true);
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

    let (_, tail) = text.split_once("备注")?;
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
    let empty_company_names = text
        .lines()
        .map(str::trim)
        .filter(|line| matches!(*line, "名称：" | "名称:"))
        .count();
    has_empty_invoice_date
        && empty_company_names >= 2
        && TEMPLATE_SIGNALS.iter().all(|signal| text.contains(signal))
}

fn labeled_amount(text: &str) -> Result<Option<i64>, ()> {
    let Some(after_label) = ["价税合计（小写）", "价税合计(小写)"]
        .into_iter()
        .find_map(|label| text.split_once(label).map(|(_, value)| value))
        .or_else(|| {
            text.split_once("价税合计").map(|(_, value)| {
                value
                    .find("小写")
                    .filter(|index| value[..*index].chars().count() <= 48)
                    .map(|index| &value[index + "小写".len()..])
                    .unwrap_or(value)
            })
        })
        .or_else(|| text.split_once("合计").map(|(_, value)| value))
    else {
        return Ok(None);
    };
    let value = after_label
        .trim_start_matches(['：', ':', '）', ')', '（', '('])
        .trim_start()
        .trim_start_matches("RMB")
        .trim_start()
        .trim_start_matches(['¥', '￥'])
        .trim_start();
    let token: String = value
        .chars()
        .take_while(|character| {
            character.is_ascii_digit() || matches!(*character, '.' | ',' | '-' | '+')
        })
        .collect();
    if let Some(amount) = parse_amount_cents(&token) {
        return Ok(Some(amount));
    }
    if token.is_empty() {
        return Ok(max_currency_amount(text));
    }
    Err(())
}

fn max_currency_amount(text: &str) -> Option<i64> {
    text.char_indices()
        .filter_map(|(index, marker)| {
            matches!(marker, '¥' | '￥')
                .then(|| &text[index + marker.len_utf8()..])
                .and_then(parse_amount_prefix)
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
