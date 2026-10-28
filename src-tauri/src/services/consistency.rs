//! Startup consistency audit.
//!
//! Exports fail for reasons that are invisible until someone tries: an original
//! that was moved out of the data directory, a normalized PDF that was never
//! written, a batch that silently carries an item which can never be exported.
//! This module answers "is the library still consistent with its own records?"
//! so the dashboard can say so before an export does.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::domain::error::AppError;
use crate::infra::files::AppPaths;

const MAX_SAMPLES: usize = 5;
/// A runaway directory must not turn the audit into a hang.
const MAX_WALKED_ENTRIES: usize = 50_000;
const MAX_WALK_DEPTH: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsistencyIssue {
    pub key: String,
    pub label: String,
    pub count: i64,
    /// What the user can do about it.
    pub hint: String,
    pub samples: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsistencyReport {
    pub checked_at: String,
    pub items_checked: i64,
    pub batches_checked: i64,
    pub issues: Vec<ConsistencyIssue>,
}

fn internal(message: impl Into<String>) -> AppError {
    AppError::Internal {
        message: message.into(),
    }
}

/// Answer whether the library matches its records.
pub async fn audit(pool: &SqlitePool, paths: &AppPaths) -> Result<ConsistencyReport, AppError> {
    let items = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<String>,
            String,
            String,
            Option<String>,
        ),
    >(
        "SELECT original_name, original_path, normalized_pdf_path, recognition_status, \
         confirmation_status, batch_id FROM items",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| internal(format!("consistency audit failed to read items: {error}")))?;

    let mut referenced: HashSet<String> = HashSet::new();
    let mut missing_original_samples = Vec::new();
    let mut missing_normalized_samples = Vec::new();
    let mut missing_originals = 0_i64;
    let mut missing_normalized = 0_i64;
    let mut confirmed_without_normalized = 0_i64;
    let mut blocked_batch_items: HashMap<String, i64> = HashMap::new();

    for (original_name, original_path, normalized_path, recognition, confirmation, batch_id) in
        &items
    {
        referenced.insert(original_path.clone());
        if let Some(path) = normalized_path {
            referenced.insert(path.clone());
        }

        let original_missing = !file_exists(original_path);
        if original_missing {
            missing_originals += 1;
            push_sample(&mut missing_original_samples, original_name);
        }

        let normalized_missing = match normalized_path.as_deref() {
            Some(path) => !file_exists(path),
            None => true,
        };
        let is_export_candidate = recognition == "succeeded" && confirmation == "confirmed";
        if normalized_missing {
            missing_normalized += 1;
            push_sample(&mut missing_normalized_samples, original_name);
            if is_export_candidate {
                confirmed_without_normalized += 1;
            }
        }

        if (original_missing || (normalized_missing && is_export_candidate))
            && let Some(batch_id) = batch_id
        {
            *blocked_batch_items.entry(batch_id.clone()).or_default() += 1;
        }
    }

    let batches = sqlx::query_as::<_, (String, String)>("SELECT id, name FROM batches")
        .fetch_all(pool)
        .await
        .map_err(|error| internal(format!("consistency audit failed to read batches: {error}")))?;

    let mut blocked_batches = blocked_batch_items
        .into_iter()
        .filter_map(|(batch_id, affected)| {
            batches
                .iter()
                .find(|(id, _)| id == &batch_id)
                .map(|(_, name)| (name.clone(), affected))
        })
        .collect::<Vec<_>>();
    blocked_batches.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));

    let orphans = collect_orphans(paths, &referenced);

    let mut issues = Vec::new();
    if missing_originals != 0 {
        issues.push(ConsistencyIssue {
            key: "missing_original".to_owned(),
            label: "原件文件缺失".to_owned(),
            count: missing_originals,
            hint: "原始文件已不在数据目录中。请从备份恢复，或确认不再需要后删除这些票据记录。"
                .to_owned(),
            samples: missing_original_samples,
        });
    }
    if missing_normalized != 0 {
        issues.push(ConsistencyIssue {
            key: "missing_normalized_pdf".to_owned(),
            label: "缺少归一化 PDF".to_owned(),
            count: missing_normalized,
            hint: "PDF、JPG、PNG 原件可在票据详情点击“重新识别”补齐；其他格式请改用可报销的原件。"
                .to_owned(),
            samples: missing_normalized_samples,
        });
    }
    if confirmed_without_normalized != 0 {
        issues.push(ConsistencyIssue {
            key: "confirmed_without_normalized_pdf".to_owned(),
            label: "已确认但缺归一化 PDF".to_owned(),
            count: confirmed_without_normalized,
            hint: "这些票据会阻止所在批次导出。可点击“补齐归一化 PDF”，或把它们移出批次。"
                .to_owned(),
            samples: Vec::new(),
        });
    }
    if !blocked_batches.is_empty() {
        issues.push(ConsistencyIssue {
            key: "blocked_batch".to_owned(),
            label: "存在无法导出的批次".to_owned(),
            count: blocked_batches.len() as i64,
            hint: "这些批次里有缺失原件的票据。打开批次详情可以看到逐张处理入口。".to_owned(),
            samples: blocked_batches
                .iter()
                .take(MAX_SAMPLES)
                .map(|(name, affected)| format!("{name}（{affected} 张待处理）"))
                .collect(),
        });
    }
    if orphans != 0 {
        issues.push(ConsistencyIssue {
            key: "orphan_file".to_owned(),
            label: "存在未被引用的文件".to_owned(),
            count: orphans,
            hint: "存储目录里有票据记录没有引用的文件，通常是历史遗留。确认无误后可手动清理，App 不会自动删除。"
                .to_owned(),
            samples: Vec::new(),
        });
    }

    Ok(ConsistencyReport {
        checked_at: Utc::now().to_rfc3339(),
        items_checked: items.len() as i64,
        batches_checked: batches.len() as i64,
        issues,
    })
}

fn file_exists(path: &str) -> bool {
    !path.is_empty() && Path::new(path).is_file()
}

fn push_sample(samples: &mut Vec<String>, name: &str) {
    if samples.len() < MAX_SAMPLES && !samples.iter().any(|sample| sample == name) {
        samples.push(name.to_owned());
    }
}

fn collect_orphans(paths: &AppPaths, referenced: &HashSet<String>) -> i64 {
    let mut seen = 0_usize;
    let mut orphans = 0_i64;
    for directory in [&paths.originals, &paths.normalized] {
        walk(directory, referenced, &mut orphans, &mut seen, 0);
        if seen >= MAX_WALKED_ENTRIES {
            break;
        }
    }
    orphans
}

fn walk(
    directory: &Path,
    referenced: &HashSet<String>,
    orphans: &mut i64,
    seen: &mut usize,
    depth: usize,
) {
    if depth > MAX_WALK_DEPTH || *seen >= MAX_WALKED_ENTRIES {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if *seen >= MAX_WALKED_ENTRIES {
            return;
        }
        *seen += 1;
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            walk(&path, referenced, orphans, seen, depth + 1);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if is_ignored_artifact(&path) {
            continue;
        }
        if !referenced.contains(&path.to_string_lossy().into_owned()) {
            *orphans += 1;
        }
    }
}

/// Partial writes and export leftovers are not user data.
fn is_ignored_artifact(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    name.starts_with('.') || name.ends_with(".part")
}

/// Files the audit walks, exposed for tests that need the same root set.
pub fn audited_directories(paths: &AppPaths) -> Vec<PathBuf> {
    vec![paths.originals.clone(), paths.normalized.clone()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temporary_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("consistency-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn paths_for(root: &Path) -> AppPaths {
        let paths = AppPaths {
            root: root.to_path_buf(),
            originals: root.join("originals"),
            normalized: root.join("normalized"),
            exports: root.join("exports"),
            staging: root.join("staging"),
        };
        for directory in audited_directories(&paths) {
            fs::create_dir_all(directory).unwrap();
        }
        paths
    }

    async fn insert_item(
        pool: &SqlitePool,
        original_path: &str,
        normalized_pdf_path: Option<&str>,
        recognition: &str,
        confirmation: &str,
        batch_id: Option<&str>,
    ) {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO items (id, original_name, original_path, normalized_pdf_path, sha256, \
             mime_type, source_type, fetched_at, recognition_status, confirmation_status, \
             batch_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 'sha', 'application/pdf', 'manual_upload', ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(
            Path::new(original_path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "invoice.pdf".to_owned()),
        )
        .bind(original_path)
        .bind(normalized_pdf_path)
        .bind(&now)
        .bind(recognition)
        .bind(confirmation)
        .bind(batch_id)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_batch(pool: &SqlitePool, id: &str, name: &str) {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO batches (id, name, start_date, end_date, status, created_at, updated_at) \
             VALUES (?, ?, '2026-08-01', '2026-08-31', 'draft', ?, ?)",
        )
        .bind(id)
        .bind(name)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_empty_library_is_consistent() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let root = temporary_root();
        let paths = paths_for(&root);
        let report = audit(&pool, &paths).await.unwrap();
        assert_eq!(report.items_checked, 0);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_missing_original_and_a_missing_pdf_are_both_reported() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let root = temporary_root();
        let paths = paths_for(&root);
        insert_item(
            &pool,
            "/nowhere/moved-away.pdf",
            None,
            "succeeded",
            "confirmed",
            None,
        )
        .await;

        let report = audit(&pool, &paths).await.unwrap();
        let keys = report
            .issues
            .iter()
            .map(|issue| issue.key.as_str())
            .collect::<Vec<_>>();
        assert!(keys.contains(&"missing_original"), "{keys:?}");
        assert!(keys.contains(&"missing_normalized_pdf"), "{keys:?}");
        assert!(
            keys.contains(&"confirmed_without_normalized_pdf"),
            "{keys:?}"
        );
        let original = report
            .issues
            .iter()
            .find(|issue| issue.key == "missing_original")
            .unwrap();
        assert_eq!(original.count, 1);
        assert_eq!(original.samples, vec!["moved-away.pdf".to_owned()]);
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_batch_holding_an_unexportable_item_is_named() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let root = temporary_root();
        let paths = paths_for(&root);
        insert_batch(&pool, "batch-1", "2026 年 8 月报销").await;
        insert_item(
            &pool,
            "/nowhere/gone.pdf",
            None,
            "succeeded",
            "confirmed",
            Some("batch-1"),
        )
        .await;

        let report = audit(&pool, &paths).await.unwrap();
        let blocked = report
            .issues
            .iter()
            .find(|issue| issue.key == "blocked_batch")
            .expect("a batch that cannot export must be reported");
        assert_eq!(blocked.count, 1);
        assert!(
            blocked.samples[0].contains("2026 年 8 月报销"),
            "{:?}",
            blocked.samples
        );
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn files_nobody_references_are_reported_without_failing_the_audit() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let root = temporary_root();
        let paths = paths_for(&root);
        let referenced = paths.originals.join("2026-08").join("kept.pdf");
        fs::create_dir_all(referenced.parent().unwrap()).unwrap();
        fs::write(&referenced, b"%PDF-1.4").unwrap();
        fs::write(
            paths.originals.join("2026-08").join("left-behind.pdf"),
            b"%PDF-1.4",
        )
        .unwrap();
        // Partial writes are not user data.
        fs::write(paths.originals.join("2026-08").join(".kept.pdf.part"), b"x").unwrap();
        insert_item(
            &pool,
            &referenced.to_string_lossy(),
            Some(&referenced.to_string_lossy()),
            "succeeded",
            "confirmed",
            None,
        )
        .await;

        let report = audit(&pool, &paths).await.unwrap();
        let orphan = report
            .issues
            .iter()
            .find(|issue| issue.key == "orphan_file")
            .expect("an unreferenced file must be reported");
        assert_eq!(orphan.count, 1, "{orphan:?}");
        assert!(
            !report
                .issues
                .iter()
                .any(|issue| issue.key == "missing_original"),
            "a referenced file that exists is not an issue: {:?}",
            report.issues
        );
        fs::remove_dir_all(&root).ok();
    }
}
