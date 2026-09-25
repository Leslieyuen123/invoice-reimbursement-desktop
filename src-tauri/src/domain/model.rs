use chrono::NaiveDate;
use serde::{Deserialize, Deserializer, Serialize, de};
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

/// How one scanned mail ended up in the invoice library.
///
/// `Partial` and `Failed` mean the user still has something to do in the
/// mailbox, which is why the ledger exposes them instead of only counting
/// imported invoices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum MailOutcome {
    Imported,
    Partial,
    Failed,
    Ignored,
}

impl MailOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Imported => "imported",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Ignored => "ignored",
        }
    }

    /// Whether the user should look at this mail again.
    pub const fn needs_attention(self) -> bool {
        matches!(self, Self::Partial | Self::Failed)
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewBatch {
    name: String,
    start_date: NaiveDate,
    end_date: NaiveDate,
    note: Option<String>,
}

impl<'de> Deserialize<'de> for NewBatch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct NewBatchInput {
            name: String,
            start_date: String,
            end_date: String,
            note: Option<String>,
        }

        let input = NewBatchInput::deserialize(deserializer)?;
        Self::try_new(input.name, input.start_date, input.end_date, input.note)
            .map_err(de::Error::custom)
    }
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

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn start_date(&self) -> NaiveDate {
        self.start_date
    }

    pub fn end_date(&self) -> NaiveDate {
        self.end_date
    }

    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use serde::{Serialize, de::DeserializeOwned};
    use serde_json::json;

    use super::{
        BatchStatus, Category, ConfirmationStatus, DedupeStatus, ItemStatus, NewBatch,
        RecognitionStatus, SourceType, derive_item_status,
    };
    use crate::domain::error::AppError;

    fn assert_enum_round_trips<T>(cases: &[(T, &str)])
    where
        T: Copy + Debug + DeserializeOwned + PartialEq + Serialize,
    {
        for (value, wire_value) in cases {
            let serialized = serde_json::to_string(value).unwrap();
            assert_eq!(serialized, format!("\"{wire_value}\""));
            assert_eq!(serde_json::from_str::<T>(&serialized).unwrap(), *value);
        }
    }

    #[test]
    fn round_trips_every_enum_variant_with_its_snake_case_wire_value() {
        assert_enum_round_trips(&[
            (ItemStatus::PendingRecognition, "pending_recognition"),
            (ItemStatus::PendingConfirmation, "pending_confirmation"),
            (ItemStatus::RecognitionFailed, "recognition_failed"),
            (ItemStatus::SuspectedDuplicate, "suspected_duplicate"),
            (ItemStatus::Ready, "ready"),
        ]);
        assert_enum_round_trips(&[
            (Category::Transport, "transport"),
            (Category::Dining, "dining"),
            (Category::Accommodation, "accommodation"),
            (Category::Hospitality, "hospitality"),
        ]);
        assert_enum_round_trips(&[
            (SourceType::Email, "email"),
            (SourceType::ManualUpload, "manual_upload"),
        ]);
        assert_enum_round_trips(&[
            (BatchStatus::Draft, "draft"),
            (BatchStatus::Exported, "exported"),
        ]);
        assert_enum_round_trips(&[
            (RecognitionStatus::Pending, "pending"),
            (RecognitionStatus::Succeeded, "succeeded"),
            (RecognitionStatus::Failed, "failed"),
        ]);
        assert_enum_round_trips(&[
            (ConfirmationStatus::Pending, "pending"),
            (ConfirmationStatus::Confirmed, "confirmed"),
        ]);
        assert_enum_round_trips(&[
            (DedupeStatus::Unique, "unique"),
            (DedupeStatus::SuspectedDuplicate, "suspected_duplicate"),
            (DedupeStatus::Resolved, "resolved"),
        ]);
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

        assert_eq!(batch.name(), "差旅报销");
    }

    #[test]
    fn deserialization_rejects_an_empty_batch_name() {
        let error = serde_json::from_value::<NewBatch>(json!({
            "name": "   ",
            "startDate": "2026-06-01",
            "endDate": "2026-06-30",
            "note": null,
        }))
        .unwrap_err();

        assert!(error.to_string().contains("批次名称不能为空"));
    }

    #[test]
    fn deserialization_rejects_a_reversed_batch_date_range() {
        let error = serde_json::from_value::<NewBatch>(json!({
            "name": "差旅报销",
            "startDate": "2026-07-31",
            "endDate": "2026-06-01",
            "note": null,
        }))
        .unwrap_err();

        assert!(error.to_string().contains("开始日期不能晚于结束日期"));
    }

    #[test]
    fn valid_batch_json_round_trips_through_the_validated_model() {
        let input = json!({
            "name": "  差旅报销  ",
            "startDate": "2026-06-01",
            "endDate": "2026-06-30",
            "note": "客户拜访",
        });

        let batch = serde_json::from_value::<NewBatch>(input).unwrap();

        assert_eq!(batch.name(), "差旅报销");
        assert_eq!(
            batch.start_date(),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap()
        );
        assert_eq!(
            batch.end_date(),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 30).unwrap()
        );
        assert_eq!(batch.note(), Some("客户拜访"));
        assert_eq!(
            serde_json::to_value(batch).unwrap(),
            json!({
                "name": "差旅报销",
                "startDate": "2026-06-01",
                "endDate": "2026-06-30",
                "note": "客户拜访",
            })
        );
    }

    #[test]
    fn derives_item_status_for_every_status_combination() {
        let cases = [
            (
                RecognitionStatus::Pending,
                ConfirmationStatus::Pending,
                DedupeStatus::Unique,
                ItemStatus::PendingRecognition,
            ),
            (
                RecognitionStatus::Pending,
                ConfirmationStatus::Pending,
                DedupeStatus::SuspectedDuplicate,
                ItemStatus::SuspectedDuplicate,
            ),
            (
                RecognitionStatus::Pending,
                ConfirmationStatus::Pending,
                DedupeStatus::Resolved,
                ItemStatus::PendingRecognition,
            ),
            (
                RecognitionStatus::Pending,
                ConfirmationStatus::Confirmed,
                DedupeStatus::Unique,
                ItemStatus::PendingRecognition,
            ),
            (
                RecognitionStatus::Pending,
                ConfirmationStatus::Confirmed,
                DedupeStatus::SuspectedDuplicate,
                ItemStatus::SuspectedDuplicate,
            ),
            (
                RecognitionStatus::Pending,
                ConfirmationStatus::Confirmed,
                DedupeStatus::Resolved,
                ItemStatus::PendingRecognition,
            ),
            (
                RecognitionStatus::Succeeded,
                ConfirmationStatus::Pending,
                DedupeStatus::Unique,
                ItemStatus::PendingConfirmation,
            ),
            (
                RecognitionStatus::Succeeded,
                ConfirmationStatus::Pending,
                DedupeStatus::SuspectedDuplicate,
                ItemStatus::SuspectedDuplicate,
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
                DedupeStatus::Unique,
                ItemStatus::Ready,
            ),
            (
                RecognitionStatus::Succeeded,
                ConfirmationStatus::Confirmed,
                DedupeStatus::SuspectedDuplicate,
                ItemStatus::SuspectedDuplicate,
            ),
            (
                RecognitionStatus::Succeeded,
                ConfirmationStatus::Confirmed,
                DedupeStatus::Resolved,
                ItemStatus::Ready,
            ),
            (
                RecognitionStatus::Failed,
                ConfirmationStatus::Pending,
                DedupeStatus::Unique,
                ItemStatus::RecognitionFailed,
            ),
            (
                RecognitionStatus::Failed,
                ConfirmationStatus::Pending,
                DedupeStatus::SuspectedDuplicate,
                ItemStatus::SuspectedDuplicate,
            ),
            (
                RecognitionStatus::Failed,
                ConfirmationStatus::Pending,
                DedupeStatus::Resolved,
                ItemStatus::RecognitionFailed,
            ),
            (
                RecognitionStatus::Failed,
                ConfirmationStatus::Confirmed,
                DedupeStatus::Unique,
                ItemStatus::RecognitionFailed,
            ),
            (
                RecognitionStatus::Failed,
                ConfirmationStatus::Confirmed,
                DedupeStatus::SuspectedDuplicate,
                ItemStatus::SuspectedDuplicate,
            ),
            (
                RecognitionStatus::Failed,
                ConfirmationStatus::Confirmed,
                DedupeStatus::Resolved,
                ItemStatus::RecognitionFailed,
            ),
        ];

        assert_eq!(cases.len(), 18);
        for (recognition, confirmation, dedupe, expected) in cases {
            assert_eq!(
                derive_item_status(recognition, confirmation, dedupe),
                expected
            );
        }
    }
}
