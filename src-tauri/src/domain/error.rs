use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error, Serialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum AppError {
    #[error("{message}")]
    Validation { field: String, message: String },
    #[error("{message}")]
    NotFound { entity: String, message: String },
    #[error("{message}")]
    Conflict { message: String },
    #[error("{message}")]
    External {
        service: String,
        retryable: bool,
        message: String,
    },
    #[error("{message}")]
    Internal { message: String },
}

impl AppError {
    pub fn validation(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::AppError;

    #[test]
    fn serializes_validation_error_with_a_snake_case_code() {
        let error = AppError::validation("name", "批次名称不能为空");

        assert_eq!(
            serde_json::to_value(error).unwrap(),
            json!({
                "code": "validation",
                "field": "name",
                "message": "批次名称不能为空",
            })
        );
    }

    #[test]
    fn serializes_all_other_error_variants_with_tagged_codes() {
        let cases = [
            (
                AppError::NotFound {
                    entity: "batch".to_owned(),
                    message: "批次不存在".to_owned(),
                },
                json!({
                    "code": "not_found",
                    "entity": "batch",
                    "message": "批次不存在",
                }),
            ),
            (
                AppError::Conflict {
                    message: "批次已导出".to_owned(),
                },
                json!({
                    "code": "conflict",
                    "message": "批次已导出",
                }),
            ),
            (
                AppError::External {
                    service: "imap".to_owned(),
                    retryable: true,
                    message: "邮箱同步失败".to_owned(),
                },
                json!({
                    "code": "external",
                    "service": "imap",
                    "retryable": true,
                    "message": "邮箱同步失败",
                }),
            ),
            (
                AppError::Internal {
                    message: "内部错误".to_owned(),
                },
                json!({
                    "code": "internal",
                    "message": "内部错误",
                }),
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(serde_json::to_value(error).unwrap(), expected);
        }
    }
}
