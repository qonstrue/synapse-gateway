//! SQLite ledger backend (feature `ledger-sqlite`).

use std::str::FromStr;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

use crate::ledger::{LedgerError, LedgerStore, UsageEntry};

pub struct SqliteLedger {
    pool: SqlitePool,
}

impl SqliteLedger {
    /// Connect (DSN like `sqlite://synapse.db?mode=rwc` or `sqlite::memory:`)
    /// and create the table if absent.
    ///
    /// Uses `max_connections(1)` so that both file-backed and in-memory databases
    /// work correctly: with `sqlite::memory:` every connection gets its own
    /// isolated database, so a single connection ensures the migration and all
    /// subsequent writes share the same in-memory DB.
    pub async fn connect(dsn: &str) -> Result<Self, LedgerError> {
        let opts = SqliteConnectOptions::from_str(dsn)
            .map_err(|e| LedgerError::Backend(e.to_string()))?
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .map_err(|e| LedgerError::Backend(e.to_string()))?;

        // Run the multi-statement migration via raw_sql which supports
        // multiple `;`-separated statements in a single call.
        sqlx::raw_sql(include_str!("../../migrations/0001_usage_events.sql"))
            .execute(&pool)
            .await
            .map_err(|e| LedgerError::Backend(e.to_string()))?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl LedgerStore for SqliteLedger {
    async fn record(&self, e: &UsageEntry) -> Result<(), LedgerError> {
        sqlx::query(
            "INSERT INTO usage_events \
             (ts, tenant, workspace, route, provider, model, lane, \
              input_tokens, output_tokens, cost_usd, request_id, status) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(e.ts.to_rfc3339())
        .bind(&e.tenant)
        .bind(&e.workspace)
        .bind(&e.route)
        .bind(&e.provider)
        .bind(&e.model)
        .bind(&e.lane)
        .bind(e.input_tokens as i64)
        .bind(e.output_tokens as i64)
        .bind(e.cost_usd)
        .bind(&e.request_id)
        .bind(&e.status)
        .execute(&self.pool)
        .await
        .map_err(|e| LedgerError::Backend(e.to_string()))?;
        Ok(())
    }
}
