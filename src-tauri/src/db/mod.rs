use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::domain::error::AppError;

pub mod items;

pub async fn connect(database_url: &str) -> Result<SqlitePool, AppError> {
    let options = SqliteConnectOptions::from_str(database_url)
        .map_err(|error| internal_error("failed to parse database URL", error))?
        .create_if_missing(true)
        .foreign_keys(true);

    let max_connections = if database_url == "sqlite::memory:" {
        1
    } else {
        5
    };

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

fn internal_error(context: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Internal {
        message: format!("{context}: {error}"),
    }
}
