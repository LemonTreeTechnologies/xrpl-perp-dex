//! PostgreSQL persistence for trade history, funding payments, deposits, withdrawals.
//!
//! The enclave handles current state (balances, positions).
//! PostgreSQL handles historical data (audit trail, analytics).
//!
//! All writes are fire-and-forget — pg failure does not block trading.

use sqlx::postgres::PgPool;
use tracing::{error, info};

use crate::types::FP8;

/// Database connection pool.
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Connect to PostgreSQL. Returns None if connection fails (pg is optional).
    pub async fn connect(database_url: &str) -> Option<Self> {
        match PgPool::connect(database_url).await {
            Ok(pool) => {
                info!("PostgreSQL connected");
                Some(Db { pool })
            }
            Err(e) => {
                error!("PostgreSQL connection failed (history disabled): {}", e);
                None
            }
        }
    }

    /// Record a trade.
    ///
    /// Idempotent on `(trade_id, market)` via `ON CONFLICT DO NOTHING` so
    /// that both the sequencer (which inserts from `submit_order`) and any
    /// validator (which inserts from the P2P batch replay loop) can write
    /// the same row without producing duplicates. Required for passive
    /// replication across operators — see `docs/vault-design-followup.md`.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_trade(
        &self,
        trade_id: u64,
        market: &str,
        maker_order_id: u64,
        taker_order_id: u64,
        maker_user_id: &str,
        taker_user_id: &str,
        price: FP8,
        size: FP8,
        taker_side: &str,
        timestamp_ms: u64,
    ) {
        let r = sqlx::query(
            "INSERT INTO trades (trade_id, market, maker_order_id, taker_order_id, \
             maker_user_id, taker_user_id, price, size, taker_side, timestamp_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (trade_id, market) DO NOTHING",
        )
        .bind(trade_id as i64)
        .bind(market)
        .bind(maker_order_id as i64)
        .bind(taker_order_id as i64)
        .bind(maker_user_id)
        .bind(taker_user_id)
        .bind(price.raw())
        .bind(size.raw())
        .bind(taker_side)
        .bind(timestamp_ms as i64)
        .execute(&self.pool)
        .await;

        if let Err(e) = r {
            error!("pg insert_trade failed: {}", e);
        }
    }

    /// Record a deposit.
    pub async fn insert_deposit(
        &self,
        user_id: &str,
        amount: &str,
        xrpl_tx_hash: &str,
        ledger_index: u32,
    ) {
        let amount_raw = amount.parse::<FP8>().map(|f| f.raw()).unwrap_or(0);
        let r = sqlx::query(
            "INSERT INTO deposits (user_id, amount, xrpl_tx_hash, ledger_index) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (xrpl_tx_hash) DO NOTHING",
        )
        .bind(user_id)
        .bind(amount_raw)
        .bind(xrpl_tx_hash)
        .bind(ledger_index as i64)
        .execute(&self.pool)
        .await;

        if let Err(e) = r {
            error!("pg insert_deposit failed: {}", e);
        }
    }

    /// Record a withdrawal.
    #[allow(dead_code)]
    pub async fn insert_withdrawal(
        &self,
        user_id: &str,
        amount: &str,
        destination: &str,
        status: &str,
        xrpl_tx_hash: Option<&str>,
        message: &str,
    ) {
        let amount_raw = amount.parse::<FP8>().map(|f| f.raw()).unwrap_or(0);
        let r = sqlx::query(
            "INSERT INTO withdrawals (user_id, amount, destination, status, xrpl_tx_hash, message) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(user_id)
        .bind(amount_raw)
        .bind(destination)
        .bind(status)
        .bind(xrpl_tx_hash)
        .bind(message)
        .execute(&self.pool)
        .await;

        if let Err(e) = r {
            error!("pg insert_withdrawal failed: {}", e);
        }
    }

    /// Record a liquidation.
    ///
    /// Idempotent on `position_id` via `ON CONFLICT DO NOTHING`. All operators
    /// run the liquidation scan independently against their local enclave
    /// state, so every operator would otherwise insert the same liquidation
    /// row once the position falls below maintenance margin.
    pub async fn insert_liquidation(&self, position_id: u64, user_id: &str, close_price: f64) {
        let price_raw = FP8::from_f64(close_price).raw();
        let r = sqlx::query(
            "INSERT INTO liquidations (position_id, user_id, close_price) \
             VALUES ($1, $2, $3) ON CONFLICT (position_id) DO NOTHING",
        )
        .bind(position_id as i64)
        .bind(user_id)
        .bind(price_raw)
        .execute(&self.pool)
        .await;

        if let Err(e) = r {
            error!("pg insert_liquidation failed: {}", e);
        }
    }

    /// Query trade history for a user.
    pub async fn get_user_trades(
        &self,
        user_id: &str,
        limit: i64,
    ) -> Vec<serde_json::Value> {
        let rows = sqlx::query_as::<_, (i64, String, i64, i64, String, i64,)>(
            "SELECT trade_id, taker_side, price, size, market, timestamp_ms \
             FROM trades WHERE maker_user_id = $1 OR taker_user_id = $1 \
             ORDER BY timestamp_ms DESC LIMIT $2",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        rows.iter()
            .map(|(tid, side, price, size, market, ts)| {
                serde_json::json!({
                    "trade_id": tid,
                    "taker_side": side,
                    "price": FP8(*price).to_string(),
                    "size": FP8(*size).to_string(),
                    "market": market,
                    "timestamp_ms": ts,
                })
            })
            .collect()
    }

    /// Query funding payment history for a user.
    pub async fn get_user_funding(
        &self,
        user_id: &str,
        limit: i64,
    ) -> Vec<serde_json::Value> {
        let rows = sqlx::query_as::<_, (i64, i64, String, i64,)>(
            "SELECT payment, position_id, side, timestamp_epoch \
             FROM funding_payments WHERE user_id = $1 \
             ORDER BY timestamp_epoch DESC LIMIT $2",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        rows.iter()
            .map(|(payment, pos_id, side, ts)| {
                serde_json::json!({
                    "payment": FP8(*payment).to_string(),
                    "position_id": pos_id,
                    "side": side,
                    "timestamp": ts,
                })
            })
            .collect()
    }
}
