//! One-click diagnostics bundle.
//!
//! Working out why a mailbox stopped syncing used to mean reading the keychain
//! logs, the database, the preferences and the storage tree by hand. This
//! module collects the same facts into a single archive. Credentials never
//! enter it: e-mail addresses are masked, anything that looks like a password
//! is replaced, and invoice contents are summarised as counts instead of files.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::Serialize;
use sqlx::SqlitePool;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::domain::error::AppError;
use crate::infra::files::AppPaths;
use crate::services::settings::load_preferences;

/// How many history rows a bundle carries.
pub const RECENT_SYNC_RUNS: i64 = 20;
pub const RECENT_MAIL_ROWS: i64 = 20;
/// One pathological error message must not bloat the archive.
const MAX_TEXT_CHARS: usize = 400;
const MAX_SAMPLE_NAMES: usize = 5;
const BUNDLE_README: &str = "\
发票报销 诊断包
================

这个压缩包用于排查同步、识别或导出问题。内容：

- summary.txt  —— 人类可读的概览，先看这个。
- report.json  —— 同一份数据的结构化版本，便于比对或附加到问题报告。

不包含：邮箱授权码/应用专用密码、发票原件与归一化 PDF 的正文、导出报销包内容、
完整的邮箱地址（已打码为 a***@example.com）、任何逐张票据的金额明细。

包含：版本与系统信息、数据库计数与迁移版本、账号配置（打码）、
运行设置、最近同步记录与错误摘要、邮件台账最近记录、
以及存储目录的文件数量与缺失文件统计（只含文件名，不含完整路径）。
";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CountEntry {
    pub label: String,
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationEntry {
    pub version: i64,
    pub description: String,
    pub success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountEntry {
    pub id: String,
    pub provider: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: i64,
    pub enabled: bool,
    pub sync_interval_minutes: i64,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryStateEntry {
    pub account_id: String,
    pub failures: i64,
    pub suspended: bool,
    pub next_retry_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncRunEntry {
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub imported_count: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailLedgerEntry {
    pub processed_at: String,
    pub outcome: String,
    pub candidate_count: i64,
    pub imported_count: i64,
    pub failed_count: i64,
    pub marked_seen: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreferencesEntry {
    pub background_sync_enabled: bool,
    pub mark_processed_mail_seen: bool,
    pub export_directory: String,
    pub batch_directory_pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryEntry {
    pub label: String,
    pub path: String,
    pub exists: bool,
    pub file_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityEntry {
    pub items: i64,
    pub missing_originals: i64,
    pub missing_normalized: i64,
    pub confirmed_without_normalized: i64,
    pub missing_original_samples: Vec<String>,
    pub missing_normalized_samples: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsReport {
    pub generated_at: String,
    pub app_version: String,
    pub platform: String,
    pub counts: Vec<CountEntry>,
    pub migrations: Vec<MigrationEntry>,
    pub accounts: Vec<AccountEntry>,
    pub retry_states: Vec<RetryStateEntry>,
    pub preferences: PreferencesEntry,
    pub recent_sync_runs: Vec<SyncRunEntry>,
    pub recent_mail_ledger: Vec<MailLedgerEntry>,
    pub directories: Vec<DirectoryEntry>,
    pub integrity: IntegrityEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleOutcome {
    pub path: PathBuf,
    pub bytes: u64,
}

fn internal(message: impl Into<String>) -> AppError {
    AppError::Internal {
        message: message.into(),
    }
}

/// `zhang.san@qq.com` becomes `z***@qq.com`.
pub fn mask_email(email: &str) -> String {
    let trimmed = email.trim();
    let Some((local, domain)) = trimmed.split_once('@') else {
        return trimmed.to_owned();
    };
    if local.is_empty() || domain.is_empty() {
        return trimmed.to_owned();
    }
    let first = local.chars().next().unwrap_or('*');
    format!("{first}***@{domain}")
}

/// Keep a diagnostic string useful while refusing to carry anything secret.
///
/// Messages that talk about a password are replaced wholesale: an IMAP error is
/// worth reading, but a credential that slipped into one is not worth shipping.
pub fn sanitize_text(text: &str) -> String {
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("password") || text.contains("授权码") || lowered.contains("secret") {
        return "(内容已省略：可能包含凭据)".to_owned();
    }
    let masked = text
        .split_whitespace()
        .map(mask_token)
        .collect::<Vec<_>>()
        .join(" ");
    let mut cleaned = String::with_capacity(masked.len());
    for character in masked.chars() {
        if character.is_control() {
            cleaned.push(' ');
        } else {
            cleaned.push(character);
        }
    }
    let cleaned = cleaned.trim().to_owned();
    if cleaned.chars().count() > MAX_TEXT_CHARS {
        let truncated: String = cleaned.chars().take(MAX_TEXT_CHARS).collect();
        return format!("{truncated}…");
    }
    cleaned
}

/// Mask the address inside a token such as `(zhang.san@qq.com).`, keeping the
/// punctuation around it so the message still reads normally.
fn mask_token(word: &str) -> String {
    let Some(start) = word.find(|character: char| character.is_alphanumeric()) else {
        return word.to_owned();
    };
    let Some((end, last)) = word
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_alphanumeric())
    else {
        return word.to_owned();
    };
    let end = end + last.len_utf8();
    let (leading, rest) = word.split_at(start);
    let (core, trailing) = rest.split_at(end - start);
    if !core.contains('@') || !core.contains('.') {
        return word.to_owned();
    }
    format!("{leading}{}{trailing}", mask_email(core))
}

async fn count(pool: &SqlitePool, sql: &str) -> Result<i64, AppError> {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .map_err(|error| internal(format!("diagnostics count failed: {error}")))
}

fn optional_text(value: Option<String>) -> Option<String> {
    value.map(|text| sanitize_text(&text))
}

/// Collect the report a support bundle is built from.
pub async fn build_report(
    pool: &SqlitePool,
    paths: &AppPaths,
) -> Result<DiagnosticsReport, AppError> {
    let preferences = load_preferences(pool).await?;

    let counts = vec![
        CountEntry {
            label: "票据总数".to_owned(),
            value: count(pool, "SELECT COUNT(*) FROM items").await?,
        },
        CountEntry {
            label: "识别成功".to_owned(),
            value: count(
                pool,
                "SELECT COUNT(*) FROM items WHERE recognition_status = 'succeeded'",
            )
            .await?,
        },
        CountEntry {
            label: "识别失败".to_owned(),
            value: count(
                pool,
                "SELECT COUNT(*) FROM items WHERE recognition_status = 'failed'",
            )
            .await?,
        },
        CountEntry {
            label: "待确认".to_owned(),
            value: count(
                pool,
                "SELECT COUNT(*) FROM items WHERE confirmation_status = 'pending'",
            )
            .await?,
        },
        CountEntry {
            label: "疑似重复".to_owned(),
            value: count(
                pool,
                "SELECT COUNT(*) FROM items WHERE dedupe_status = 'suspected_duplicate'",
            )
            .await?,
        },
        CountEntry {
            label: "批次总数".to_owned(),
            value: count(pool, "SELECT COUNT(*) FROM batches").await?,
        },
        CountEntry {
            label: "邮件台账记录".to_owned(),
            value: count(pool, "SELECT COUNT(*) FROM mail_ledger").await?,
        },
        CountEntry {
            label: "同步记录".to_owned(),
            value: count(pool, "SELECT COUNT(*) FROM sync_runs").await?,
        },
    ];

    let migrations = sqlx::query_as::<_, (i64, String, i64)>(
        "SELECT version, description, success FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| internal(format!("diagnostics migrations failed: {error}")))?
    .into_iter()
    .map(|(version, description, success)| MigrationEntry {
        version,
        description,
        success: success != 0,
    })
    .collect();

    let accounts = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            i64,
            i64,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >(
        "SELECT id, provider, email, imap_host, imap_port, enabled, sync_interval_minutes, \
         last_synced_at, last_error, last_error_at FROM mailbox_accounts ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| internal(format!("diagnostics accounts failed: {error}")))?
    .into_iter()
    .map(
        |(
            id,
            provider,
            email,
            imap_host,
            imap_port,
            enabled,
            sync_interval_minutes,
            last_synced_at,
            last_error,
            last_error_at,
        )| AccountEntry {
            id,
            provider,
            email: mask_email(&email),
            imap_host,
            imap_port,
            enabled: enabled != 0,
            sync_interval_minutes,
            last_synced_at,
            last_error: optional_text(last_error),
            last_error_at,
        },
    )
    .collect();

    let retry_states = sqlx::query_as::<_, (String, i64, i64, Option<String>)>(
        "SELECT account_id, failures, suspended, next_retry_at FROM sync_retry_states",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| internal(format!("diagnostics retry states failed: {error}")))?
    .into_iter()
    .map(
        |(account_id, failures, suspended, next_retry_at)| RetryStateEntry {
            account_id,
            failures,
            suspended: suspended != 0,
            next_retry_at,
        },
    )
    .collect();

    let recent_sync_runs =
        sqlx::query_as::<_, (String, Option<String>, String, i64, Option<String>)>(
            "SELECT started_at, finished_at, status, imported_count, error_message FROM sync_runs \
         ORDER BY started_at DESC LIMIT ?",
        )
        .bind(RECENT_SYNC_RUNS)
        .fetch_all(pool)
        .await
        .map_err(|error| internal(format!("diagnostics sync runs failed: {error}")))?
        .into_iter()
        .map(
            |(started_at, finished_at, status, imported_count, error)| SyncRunEntry {
                started_at,
                finished_at,
                status,
                imported_count,
                error: optional_text(error),
            },
        )
        .collect();

    let recent_mail_ledger = sqlx::query_as::<
        _,
        (String, String, i64, i64, i64, i64, Option<String>),
    >(
        "SELECT processed_at, outcome, candidate_count, imported_count, failed_count, marked_seen, \
         reason FROM mail_ledger ORDER BY processed_at DESC LIMIT ?",
    )
    .bind(RECENT_MAIL_ROWS)
    .fetch_all(pool)
    .await
    .map_err(|error| internal(format!("diagnostics mail ledger failed: {error}")))?
    .into_iter()
    .map(
        |(
            processed_at,
            outcome,
            candidate_count,
            imported_count,
            failed_count,
            marked_seen,
            reason,
        )| MailLedgerEntry {
            processed_at,
            outcome,
            candidate_count,
            imported_count,
            failed_count,
            marked_seen: marked_seen != 0,
            reason: optional_text(reason),
        },
    )
    .collect();

    let integrity = integrity_entry(pool, paths).await?;

    Ok(DiagnosticsReport {
        generated_at: Utc::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        platform: format!(
            "{} {} ({})",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::consts::FAMILY
        ),
        counts,
        migrations,
        accounts,
        retry_states,
        preferences: PreferencesEntry {
            background_sync_enabled: preferences.background_sync_enabled,
            mark_processed_mail_seen: preferences.mark_processed_mail_seen,
            export_directory: preferences.export_directory.clone(),
            batch_directory_pattern: preferences.batch_directory_pattern.clone(),
        },
        recent_sync_runs,
        recent_mail_ledger,
        directories: vec![
            directory_entry("数据根目录", &paths.root),
            directory_entry("原件目录", &paths.originals),
            directory_entry("归一化 PDF 目录", &paths.normalized),
            directory_entry("导出目录", &paths.exports),
        ],
        integrity,
    })
}

fn directory_entry(label: &str, path: &Path) -> DirectoryEntry {
    let file_count = fs::read_dir(path)
        .map(|entries| entries.flatten().count() as i64)
        .unwrap_or(0);
    DirectoryEntry {
        label: label.to_owned(),
        path: path.to_string_lossy().into_owned(),
        exists: path.is_dir(),
        file_count,
    }
}

async fn integrity_entry(pool: &SqlitePool, paths: &AppPaths) -> Result<IntegrityEntry, AppError> {
    let rows = sqlx::query_as::<_, (String, Option<String>, String, String)>(
        "SELECT original_path, normalized_pdf_path, recognition_status, confirmation_status \
         FROM items",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| internal(format!("diagnostics integrity failed: {error}")))?;

    let mut missing_original_samples = Vec::new();
    let mut missing_normalized_samples = Vec::new();
    let mut missing_originals = 0_i64;
    let mut missing_normalized = 0_i64;
    let mut confirmed_without_normalized = 0_i64;

    for (original_path, normalized_path, recognition, confirmation) in &rows {
        if !file_exists(original_path) {
            missing_originals += 1;
            push_sample(&mut missing_original_samples, original_path);
        }
        match normalized_path {
            Some(path) if file_exists(path) => {}
            _ => {
                missing_normalized += 1;
                push_sample(
                    &mut missing_normalized_samples,
                    normalized_path.as_deref().unwrap_or(original_path),
                );
                if recognition == "succeeded" && confirmation == "confirmed" {
                    confirmed_without_normalized += 1;
                }
            }
        }
    }

    let _ = paths;
    Ok(IntegrityEntry {
        items: rows.len() as i64,
        missing_originals,
        missing_normalized,
        confirmed_without_normalized,
        missing_original_samples,
        missing_normalized_samples,
    })
}

fn file_exists(path: &str) -> bool {
    !path.is_empty() && Path::new(path).is_file()
}

fn push_sample(samples: &mut Vec<String>, path: &str) {
    if samples.len() >= MAX_SAMPLE_NAMES {
        return;
    }
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "(无文件名)".to_owned());
    if !samples.contains(&name) {
        samples.push(name);
    }
}

/// Human readable rendering of the same facts, for readers who skip JSON.
pub fn render_summary(report: &DiagnosticsReport) -> String {
    let mut out = String::new();
    out.push_str("发票报销 诊断概览\n================\n\n");
    out.push_str(&format!("生成时间: {}\n", report.generated_at));
    out.push_str(&format!("App 版本: {}\n", report.app_version));
    out.push_str(&format!("运行平台: {}\n\n", report.platform));
    out.push_str("计数\n----\n");
    for entry in &report.counts {
        out.push_str(&format!("{}: {}\n", entry.label, entry.value));
    }
    out.push_str("\n迁移版本\n--------\n");
    for entry in &report.migrations {
        out.push_str(&format!(
            "{} {} ({})\n",
            entry.version,
            entry.description,
            if entry.success { "ok" } else { "失败" }
        ));
    }
    out.push_str("\n账号\n----\n");
    if report.accounts.is_empty() {
        out.push_str("(未配置邮箱账号)\n");
    }
    for account in &report.accounts {
        out.push_str(&format!(
            "{} {} {}:{} 启用={} 间隔={}分钟 最近成功={} 最近错误={}\n",
            account.provider,
            account.email,
            account.imap_host,
            account.imap_port,
            account.enabled,
            account.sync_interval_minutes,
            account.last_synced_at.as_deref().unwrap_or("(无)"),
            account.last_error.as_deref().unwrap_or("(无)"),
        ));
    }
    out.push_str("\n运行设置\n--------\n");
    out.push_str(&format!(
        "后台自动同步={} 提取完成后标记已读={} 导出目录={} 批次命名={}\n",
        report.preferences.background_sync_enabled,
        report.preferences.mark_processed_mail_seen,
        report.preferences.export_directory,
        report.preferences.batch_directory_pattern,
    ));
    out.push_str("\n最近同步\n--------\n");
    if report.recent_sync_runs.is_empty() {
        out.push_str("(无同步记录)\n");
    }
    for run in &report.recent_sync_runs {
        out.push_str(&format!(
            "{} {} 导入={} {}\n",
            run.started_at,
            run.status,
            run.imported_count,
            run.error.as_deref().unwrap_or(""),
        ));
    }
    if !report.retry_states.is_empty() {
        out.push_str("\n重试状态\n--------\n");
        for state in &report.retry_states {
            out.push_str(&format!(
                "账号 {} 连续失败={} 暂停自动同步={} 下次重试={}\n",
                state.account_id,
                state.failures,
                state.suspended,
                state.next_retry_at.as_deref().unwrap_or("(无)"),
            ));
        }
    }
    out.push_str("\n最近邮件台账\n------------\n");
    if report.recent_mail_ledger.is_empty() {
        out.push_str("(暂无记录)\n");
    }
    for row in &report.recent_mail_ledger {
        out.push_str(&format!(
            "{} {} 候选={} 导入={} 失败={} 已读={} {}\n",
            row.processed_at,
            row.outcome,
            row.candidate_count,
            row.imported_count,
            row.failed_count,
            row.marked_seen,
            row.reason.as_deref().unwrap_or(""),
        ));
    }
    out.push_str("\n存储目录\n--------\n");
    for directory in &report.directories {
        out.push_str(&format!(
            "{}: {} (存在={} 条目数={})\n",
            directory.label, directory.path, directory.exists, directory.file_count
        ));
    }
    let integrity = &report.integrity;
    out.push_str("\n一致性\n------\n");
    out.push_str(&format!("票据总数: {}\n", integrity.items));
    out.push_str(&format!("原件缺失: {}\n", integrity.missing_originals));
    out.push_str(&format!(
        "归一化 PDF 缺失: {}\n",
        integrity.missing_normalized
    ));
    out.push_str(&format!(
        "已确认但无归一化 PDF: {}\n",
        integrity.confirmed_without_normalized
    ));
    for sample in &integrity.missing_original_samples {
        out.push_str(&format!("  原件缺失示例: {sample}\n"));
    }
    for sample in &integrity.missing_normalized_samples {
        out.push_str(&format!("  归一化缺失示例: {sample}\n"));
    }
    out
}

/// Write the bundle and return where it landed.
pub async fn write_bundle(
    pool: &SqlitePool,
    paths: &AppPaths,
    destination: Option<PathBuf>,
) -> Result<BundleOutcome, AppError> {
    let report = build_report(pool, paths).await?;
    let summary = render_summary(&report);
    let json = serde_json::to_vec_pretty(&report)
        .map_err(|error| internal(format!("diagnostics serialization failed: {error}")))?;

    let directory = destination.unwrap_or_else(|| paths.exports.clone());
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|error| internal(format!("failed to create diagnostics directory: {error}")))?;
    if !directory.is_dir() {
        return Err(AppError::validation(
            "destination",
            "诊断包保存位置不可用，请选择一个可写的目录",
        ));
    }

    let file_name = format!(
        "invoice-diagnostics-{}.zip",
        Utc::now().format("%Y%m%d-%H%M%S")
    );
    let path = directory.join(file_name);
    let file = fs::File::create(&path)
        .map_err(|error| internal(format!("failed to create diagnostics bundle: {error}")))?;
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o600);
    for (name, contents) in [
        ("README.txt", BUNDLE_README.as_bytes().to_vec()),
        ("summary.txt", summary.into_bytes()),
        ("report.json", json),
    ] {
        archive
            .start_file(name, options)
            .map_err(|error| internal(format!("failed to write bundle entry: {error}")))?;
        std::io::Write::write_all(&mut archive, &contents)
            .map_err(|error| internal(format!("failed to write bundle entry: {error}")))?;
    }
    archive
        .finish()
        .map_err(|error| internal(format!("failed to finalize diagnostics bundle: {error}")))?;

    let bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    Ok(BundleOutcome { path, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_keeps_its_domain_but_not_its_local_part() {
        assert_eq!(mask_email("zhang.san@qq.com"), "z***@qq.com");
        assert_eq!(mask_email("  a@b.cn "), "a***@b.cn");
    }

    #[test]
    fn a_value_that_is_not_an_address_is_left_alone() {
        assert_eq!(mask_email("imap.qq.com"), "imap.qq.com");
        assert_eq!(mask_email(""), "");
        assert_eq!(mask_email("@qq.com"), "@qq.com");
    }

    #[test]
    fn addresses_inside_an_error_are_masked() {
        let text = sanitize_text("login rejected for zhang.san@qq.com (a@b.cn).");
        assert!(text.contains("z***@qq.com"), "{text}");
        assert!(text.contains("a***@b.cn"), "{text}");
        assert!(!text.contains("zhang.san"), "{text}");

        // Punctuation around an address must survive the masking.
        assert_eq!(
            sanitize_text("login failed: (a@b.cn)."),
            "login failed: (a***@b.cn)."
        );
        assert_eq!(sanitize_text("see a@b.cn,"), "see a***@b.cn,");
    }

    #[test]
    fn anything_about_a_password_is_dropped_whole() {
        assert_eq!(
            sanitize_text("Password for user: hunter2"),
            "(内容已省略：可能包含凭据)"
        );
        assert_eq!(
            sanitize_text("授权码 1234 无效"),
            "(内容已省略：可能包含凭据)"
        );
    }

    #[test]
    fn control_characters_are_flattened_and_long_text_is_capped() {
        let text = sanitize_text("a\nb\tc");
        assert_eq!(text, "a b c");
        let long = sanitize_text(&"x".repeat(MAX_TEXT_CHARS + 50));
        assert_eq!(long.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[tokio::test]
    async fn a_fresh_installation_reports_its_own_state() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let root = std::env::temp_dir().join(format!("diag-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let paths = AppPaths {
            root: root.clone(),
            originals: root.join("originals"),
            normalized: root.join("normalized"),
            exports: root.join("exports"),
            staging: root.join("staging"),
        };
        fs::create_dir_all(&paths.originals).unwrap();
        fs::create_dir_all(&paths.normalized).unwrap();

        let report = build_report(&pool, &paths).await.unwrap();
        assert_eq!(report.app_version, env!("CARGO_PKG_VERSION"));
        assert!(report.accounts.is_empty());
        assert!(report.integrity.items == 0);
        assert!(
            report
                .migrations
                .iter()
                .any(|migration| migration.version == 13),
            "the bundle must name the applied migrations"
        );

        let summary = render_summary(&report);
        assert!(summary.contains("发票报销 诊断概览"));
        assert!(summary.contains("归一化 PDF 缺失: 0"));

        let outcome = write_bundle(&pool, &paths, Some(root.join("exports")))
            .await
            .unwrap();
        assert!(outcome.bytes > 0);
        assert!(outcome.path.is_file());
        let file = fs::File::open(&outcome.path).unwrap();
        let names = zip::ZipArchive::new(file)
            .unwrap()
            .file_names()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(names.contains(&"summary.txt".to_owned()), "{names:?}");
        assert!(names.contains(&"report.json".to_owned()), "{names:?}");
        fs::remove_dir_all(&root).ok();
    }
}
