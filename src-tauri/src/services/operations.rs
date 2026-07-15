use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;

use crate::domain::error::AppError;

#[derive(Clone, Default)]
pub(crate) struct AccountOperationCoordinator {
    locks: Arc<DashMap<Uuid, Arc<Mutex<()>>>>,
}

impl AccountOperationCoordinator {
    pub(crate) fn try_lock(&self, account_id: Uuid) -> Result<OwnedMutexGuard<()>, AppError> {
        self.locks
            .entry(account_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
            .try_lock_owned()
            .map_err(|_| AppError::Conflict {
                message: "mailbox account operation is already running".to_owned(),
            })
    }
}
