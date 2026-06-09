//! Postgres ledger backend (feature `ledger-postgres`).

use async_trait::async_trait;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::ledger::{LedgerError, LedgerStore, UsageEntry};

pub struct PostgresLedger {
    pool: PgPool,
}

impl PostgresLedger {
    pub async fn connect(dsn: &str) -> Result<Self, LedgerError> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(dsn)
            .await
            .map_err(|e| LedgerError::Backend(e.to_string()))?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS usage_events (\
             id BIGSERIAL PRIMARY KEY, ts TIMESTAMPTZ NOT NULL, tenant TEXT NOT NULL, workspace TEXT, \
             route TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL, lane TEXT NOT NULL, \
             input_tokens BIGINT NOT NULL, output_tokens BIGINT NOT NULL, cost_usd DOUBLE PRECISION NOT NULL, \
             request_id TEXT NOT NULL, status TEXT NOT NULL)",
        )
        .execute(&pool)
        .await
        .map_err(|e| LedgerError::Backend(e.to_string()))?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl LedgerStore for PostgresLedger {
    async fn record(&self, e: &UsageEntry) -> Result<(), LedgerError> {
        sqlx::query(
            "INSERT INTO usage_events \
             (ts, tenant, workspace, route, provider, model, lane, input_tokens, output_tokens, cost_usd, request_id, status) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
        )
        .bind(e.ts)
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
