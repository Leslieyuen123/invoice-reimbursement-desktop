use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, Error, Serialize, PartialEq, Eq)]
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

pub(crate) fn sanitize_message(message: &str, secrets: &[&str]) -> String {
    let mut secrets = secrets
        .iter()
        .copied()
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
    let redacted = secrets
        .into_iter()
        .fold(message.to_owned(), |message, secret| {
            message.replace(secret, "[redacted]")
        });
    redacted
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(512)
        .collect::<String>()
        .trim()
        .to_owned()
}

pub(crate) fn sanitize_app_error(error: AppError, secrets: &[&str]) -> AppError {
    let sanitize = |message: String| sanitize_message(&message, secrets);
    match error {
        AppError::Validation { field, message } => AppError::Validation {
            field,
            message: sanitize(message),
        },
        AppError::NotFound { entity, message } => AppError::NotFound {
            entity,
            message: sanitize(message),
        },
        AppError::Conflict { message } => AppError::Conflict {
            message: sanitize(message),
        },
        AppError::External {
            service,
            retryable,
            message,
        } => AppError::External {
            service,
            retryable,
            message: sanitize(message),
        },
        AppError::Internal { message } => AppError::Internal {
            message: sanitize(message),
        },
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
