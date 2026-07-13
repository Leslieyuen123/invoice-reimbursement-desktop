use std::sync::Arc;

use sqlx::SqlitePool;

use crate::infra::credentials::CredentialStore;
use crate::infra::files::AppPaths;

#[derive(Clone)]
pub struct AppState {
    pool: SqlitePool,
    paths: AppPaths,
    credentials: Arc<dyn CredentialStore>,
}

impl AppState {
    pub fn new(pool: SqlitePool, paths: AppPaths, credentials: Arc<dyn CredentialStore>) -> Self {
        Self {
            pool,
            paths,
            credentials,
        }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn credentials(&self) -> &dyn CredentialStore {
        self.credentials.as_ref()
    }
}
