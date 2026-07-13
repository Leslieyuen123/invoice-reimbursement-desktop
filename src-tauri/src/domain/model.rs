use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::Type;

use super::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    PendingRecognition,
    PendingConfirmation,
    RecognitionFailed,
    SuspectedDuplicate,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum Category {
    Transport,
    Dining,
    Accommodation,
    Hospitality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum SourceType {
    Email,
    ManualUpload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum BatchStatus {
    Draft,
    Exported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum RecognitionStatus {
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum ConfirmationStatus {
    Pending,
    Confirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum DedupeStatus {
    Unique,
    SuspectedDuplicate,
    Resolved,
}

pub fn derive_item_status(
    recognition_status: RecognitionStatus,
    confirmation_status: ConfirmationStatus,
    dedupe_status: DedupeStatus,
) -> ItemStatus {
    if dedupe_status == DedupeStatus::SuspectedDuplicate {
        return ItemStatus::SuspectedDuplicate;
    }

    match (recognition_status, confirmation_status) {
        (RecognitionStatus::Failed, _) => ItemStatus::RecognitionFailed,
        (RecognitionStatus::Pending, _) => ItemStatus::PendingRecognition,
        (RecognitionStatus::Succeeded, ConfirmationStatus::Pending) => {
            ItemStatus::PendingConfirmation
        }
        (RecognitionStatus::Succeeded, ConfirmationStatus::Confirmed) => ItemStatus::Ready,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewBatch {
    pub name: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub note: Option<String>,
}

fn parse_date(input: &str, field: &str) -> Result<NaiveDate, AppError> {
    NaiveDate::parse_from_str(input, "%Y-%m-%d")
        .ok()
        .filter(|date| date.format("%Y-%m-%d").to_string() == input)
        .ok_or_else(|| AppError::validation(field, "必须是 YYYY-MM-DD"))
}

impl NewBatch {
    pub fn try_new(
        name: impl Into<String>,
        start_date: impl AsRef<str>,
        end_date: impl AsRef<str>,
        note: Option<String>,
    ) -> Result<Self, AppError> {
        let name = name.into().trim().to_owned();
        if name.is_empty() {
            return Err(AppError::validation("name", "批次名称不能为空"));
        }

        let start_date = parse_date(start_date.as_ref(), "startDate")?;
        let end_date = parse_date(end_date.as_ref(), "endDate")?;

        if start_date > end_date {
            return Err(AppError::validation(
                "dateRange",
                "开始日期不能晚于结束日期",
            ));
        }

        Ok(Self {
            name,
            start_date,
            end_date,
            note,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BatchStatus, Category, ConfirmationStatus, DedupeStatus, ItemStatus, NewBatch,
        RecognitionStatus, SourceType, derive_item_status,
    };
    use crate::domain::error::AppError;

    #[test]
    fn serializes_domain_enums_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&Category::Transport).unwrap(),
            "\"transport\""
        );
        assert_eq!(
            serde_json::to_string(&ItemStatus::PendingConfirmation).unwrap(),
            "\"pending_confirmation\""
        );
    }

    #[test]
    fn serializes_persisted_enums_as_snake_case() {
        let cases = [
            (
                serde_json::to_value(SourceType::ManualUpload).unwrap(),
                "manual_upload",
            ),
            (
                serde_json::to_value(BatchStatus::Exported).unwrap(),
                "exported",
            ),
            (
                serde_json::to_value(RecognitionStatus::Failed).unwrap(),
                "failed",
            ),
            (
                serde_json::to_value(ConfirmationStatus::Confirmed).unwrap(),
                "confirmed",
            ),
            (
                serde_json::to_value(DedupeStatus::SuspectedDuplicate).unwrap(),
                "suspected_duplicate",
            ),
        ];

        for (serialized, expected) in cases {
            assert_eq!(serialized, expected);
        }
    }

    #[test]
    fn rejects_a_reversed_batch_date_range() {
        let error = NewBatch::try_new("差旅报销", "2026-07-31", "2026-06-01", None).unwrap_err();

        assert!(matches!(
            error,
            AppError::Validation { ref field, .. } if field == "dateRange"
        ));
    }

    #[test]
    fn rejects_an_empty_batch_name() {
        let error = NewBatch::try_new("   ", "2026-06-01", "2026-06-30", None).unwrap_err();

        assert_eq!(
            error,
            AppError::Validation {
                field: "name".to_owned(),
                message: "批次名称不能为空".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_a_non_iso_start_date() {
        let error = NewBatch::try_new("差旅报销", "2026-6-01", "2026-06-30", None).unwrap_err();

        assert_eq!(
            error,
            AppError::Validation {
                field: "startDate".to_owned(),
                message: "必须是 YYYY-MM-DD".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_a_non_iso_end_date() {
        let error = NewBatch::try_new("差旅报销", "2026-06-01", "2026-6-30", None).unwrap_err();

        assert_eq!(
            error,
            AppError::Validation {
                field: "endDate".to_owned(),
                message: "必须是 YYYY-MM-DD".to_owned(),
            }
        );
    }

    #[test]
    fn trims_a_valid_batch_name() {
        let batch = NewBatch::try_new("  差旅报销  ", "2026-06-01", "2026-06-30", None).unwrap();

        assert_eq!(batch.name, "差旅报销");
    }

    #[test]
    fn suspected_duplicate_takes_precedence_over_confirmed_recognition() {
        let status = derive_item_status(
            RecognitionStatus::Succeeded,
            ConfirmationStatus::Confirmed,
            DedupeStatus::SuspectedDuplicate,
        );

        assert_eq!(status, ItemStatus::SuspectedDuplicate);
    }

    #[test]
    fn derives_each_item_status_in_precedence_order() {
        let cases = [
            (
                RecognitionStatus::Succeeded,
                ConfirmationStatus::Confirmed,
                DedupeStatus::SuspectedDuplicate,
                ItemStatus::SuspectedDuplicate,
            ),
            (
                RecognitionStatus::Failed,
                ConfirmationStatus::Confirmed,
                DedupeStatus::Unique,
                ItemStatus::RecognitionFailed,
            ),
            (
                RecognitionStatus::Pending,
                ConfirmationStatus::Confirmed,
                DedupeStatus::Unique,
                ItemStatus::PendingRecognition,
            ),
            (
                RecognitionStatus::Succeeded,
                ConfirmationStatus::Pending,
                DedupeStatus::Resolved,
                ItemStatus::PendingConfirmation,
            ),
            (
                RecognitionStatus::Succeeded,
                ConfirmationStatus::Confirmed,
                DedupeStatus::Resolved,
                ItemStatus::Ready,
            ),
        ];

        for (recognition, confirmation, dedupe, expected) in cases {
            assert_eq!(
                derive_item_status(recognition, confirmation, dedupe),
                expected
            );
        }
    }
}
