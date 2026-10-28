//! Live progress and cancellation for a running mailbox sync.
//!
//! A first range scan can walk thousands of mails, and until now the only
//! feedback was a run row that appeared when it finished. The monitor keeps a
//! small in-memory snapshot per syncing account so the UI can say which mail is
//! being processed and how much has come in, and so the user can stop a scan
//! that is going to take another ten minutes.
//!
//! Progress is deliberately in memory only: it describes work happening right
//! now, and a restart with no syncs running should show nothing rather than a
//! stale row that never ends.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use uuid::Uuid;

/// An entry whose sync never reported back (crashed thread, killed process) is
/// dropped from snapshots after this long.
const STALE_AFTER_MINUTES: i64 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncProgressSnapshot {
    pub account_id: String,
    /// Mailbox of the mail being processed, when the server reports one.
    pub mailbox: Option<String>,
    /// Mails this run has finished processing.
    pub processed: u64,
    pub imported: u64,
    pub failed: u64,
    pub started_at: String,
}

#[derive(Debug, Clone)]
struct Entry {
    mailbox: Option<String>,
    processed: u64,
    imported: u64,
    failed: u64,
    started_at: DateTime<Utc>,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
pub struct SyncMonitor {
    entries: RwLock<HashMap<Uuid, Entry>>,
}

static MONITOR: OnceLock<SyncMonitor> = OnceLock::new();

/// The process-wide monitor.
///
/// A global keeps every sync path (scheduler, manual sync, batch automation)
/// reporting into the same place without threading a handle through the
/// service constructors.
pub fn monitor() -> &'static SyncMonitor {
    MONITOR.get_or_init(SyncMonitor::default)
}

/// Clears the account's entry when the sync finishes, however it ends.
///
/// The guard borrows the monitor it came from, so a test that uses its own
/// monitor also cleans up its own entry.
pub struct SyncRunGuard<'a> {
    monitor: &'a SyncMonitor,
    account_id: Uuid,
}

impl Drop for SyncRunGuard<'_> {
    fn drop(&mut self) {
        self.monitor.finish(self.account_id);
    }
}

impl SyncMonitor {
    /// Start reporting progress for an account. Re-entering keeps the counters
    /// of a sync that is already running, so paged scans stay cumulative.
    pub fn begin(&self, account_id: Uuid) -> SyncRunGuard<'_> {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(|error| error.into_inner());
        entries.entry(account_id).or_insert_with(|| Entry {
            mailbox: None,
            processed: 0,
            imported: 0,
            failed: 0,
            started_at: Utc::now(),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        SyncRunGuard {
            monitor: self,
            account_id,
        }
    }

    /// Record one processed mail and what it produced.
    pub fn tick(&self, account_id: Uuid, mailbox: Option<&str>, imported: u64, failed: u64) {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let entry = entries.entry(account_id).or_insert_with(|| Entry {
            mailbox: None,
            processed: 0,
            imported: 0,
            failed: 0,
            started_at: Utc::now(),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        entry.processed += 1;
        entry.imported += imported;
        entry.failed += failed;
        if let Some(mailbox) = mailbox {
            entry.mailbox = Some(mailbox.to_owned());
        }
    }

    /// Ask a running sync to stop. Returns whether one was running.
    pub fn cancel(&self, account_id: Uuid) -> bool {
        let entries = self
            .entries
            .read()
            .unwrap_or_else(|error| error.into_inner());
        match entries.get(&account_id) {
            Some(entry) => {
                entry.cancel.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// Whether this account's sync has been asked to stop.
    pub fn is_cancelled(&self, account_id: Uuid) -> bool {
        let entries = self
            .entries
            .read()
            .unwrap_or_else(|error| error.into_inner());
        entries
            .get(&account_id)
            .is_some_and(|entry| entry.cancel.load(Ordering::SeqCst))
    }

    pub fn finish(&self, account_id: Uuid) {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(|error| error.into_inner());
        entries.remove(&account_id);
    }

    /// Progress for every account that is syncing right now.
    pub fn snapshot(&self) -> Vec<SyncProgressSnapshot> {
        let now = Utc::now();
        let cutoff = now - Duration::minutes(STALE_AFTER_MINUTES);
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(|error| error.into_inner());
        entries.retain(|_, entry| entry.started_at >= cutoff);
        let mut snapshots = entries
            .iter()
            .map(|(account_id, entry)| SyncProgressSnapshot {
                account_id: account_id.to_string(),
                mailbox: entry.mailbox.clone(),
                processed: entry.processed,
                imported: entry.imported,
                failed: entry.failed,
                started_at: entry.started_at.to_rfc3339(),
            })
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| left.started_at.cmp(&right.started_at));
        snapshots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_monitor_reports_nothing() {
        let monitor = SyncMonitor::default();
        assert!(monitor.snapshot().is_empty());
        assert!(!monitor.is_cancelled(Uuid::new_v4()));
    }

    #[test]
    fn ticks_accumulate_into_one_snapshot_per_account() {
        let monitor = SyncMonitor::default();
        let account = Uuid::new_v4();
        let guard = monitor.begin(account);
        monitor.tick(account, Some("INBOX"), 2, 0);
        monitor.tick(account, Some("INBOX"), 0, 1);

        let snapshots = monitor.snapshot();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].account_id, account.to_string());
        assert_eq!(snapshots[0].mailbox.as_deref(), Some("INBOX"));
        assert_eq!(snapshots[0].processed, 2);
        assert_eq!(snapshots[0].imported, 2);
        assert_eq!(snapshots[0].failed, 1);

        drop(guard);
        assert!(
            monitor.snapshot().is_empty(),
            "a finished sync must stop being reported"
        );
    }

    #[test]
    fn a_paged_scan_keeps_counting_instead_of_restarting() {
        let monitor = SyncMonitor::default();
        let account = Uuid::new_v4();
        let guard = monitor.begin(account);
        monitor.tick(account, None, 1, 0);
        // The next page enters the same account again.
        let second = monitor.begin(account);
        monitor.tick(account, None, 1, 0);
        assert_eq!(monitor.snapshot()[0].processed, 2);
        drop(second);
        drop(guard);
        assert!(monitor.snapshot().is_empty());
    }

    #[test]
    fn cancelling_only_affects_the_account_that_was_asked() {
        let monitor = SyncMonitor::default();
        let target = Uuid::new_v4();
        let other = Uuid::new_v4();
        let _target_guard = monitor.begin(target);
        let _other_guard = monitor.begin(other);

        assert!(monitor.cancel(target));
        assert!(monitor.is_cancelled(target));
        assert!(!monitor.is_cancelled(other));
        assert!(
            !monitor.cancel(Uuid::new_v4()),
            "cancelling an idle account must be a no-op"
        );
    }

    #[test]
    fn a_stale_entry_is_not_reported_as_running() {
        let monitor = SyncMonitor::default();
        let account = Uuid::new_v4();
        let _guard = monitor.begin(account);
        {
            let mut entries = monitor.entries.write().unwrap();
            let entry = entries.get_mut(&account).unwrap();
            entry.started_at = Utc::now() - Duration::minutes(STALE_AFTER_MINUTES + 1);
        }
        assert!(
            monitor.snapshot().is_empty(),
            "a sync that never reported back must not look like it is still running"
        );
    }
}
