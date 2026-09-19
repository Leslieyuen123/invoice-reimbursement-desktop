use std::{str::FromStr, time::Duration};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use crate::domain::error::AppError;

pub mod accounts;
pub mod batches;
pub mod items;
pub mod mail_ledger;

pub async fn connect(database_url: &str) -> Result<SqlitePool, AppError> {
    let is_memory = is_memory_database_url(database_url);
    let mut options = SqliteConnectOptions::from_str(database_url)
        .map_err(|error| internal_error("failed to parse database URL", error))?
        .create_if_missing(true)
        .foreign_keys(true);
    if !is_memory {
        options = options
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
    }

    let max_connections = if is_memory { 1 } else { 5 };

    let pool = SqlitePoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options)
        .await
        .map_err(|error| internal_error("failed to connect to database", error))?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|error| internal_error("failed to run database migrations", error))?;

    Ok(pool)
}

fn is_memory_database_url(mut database_url: &str) -> bool {
    database_url = database_url
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:");

    let mut database_and_query = database_url.splitn(2, '?');
    let database = database_and_query.next().unwrap_or_default();
    let query = database_and_query.next();

    database == ":memory:"
        || query.is_some_and(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .any(|(key, value)| key == "mode" && value == "memory")
        })
}

fn internal_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Internal {
        message: format!("{context}: {error}"),
    }
}
