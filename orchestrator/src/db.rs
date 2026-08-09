//! PostgreSQL persistence for trade history, funding payments, deposits, withdrawals.
//!
//! The enclave handles current state (balances, positions).
//! PostgreSQL handles historical data (audit trail, analytics).
//!
//! All writes are fire-and-forget — pg failure does not block trading.

use sqlx::postgres::PgPool;
use tracing::{error, info, warn};

use crate::types::FP8;

/// Database connection pool.
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

/// REQ-β3.2c-impl (D-1/D-2): the retained current-epoch membership tuple a
/// `source` node needs to serve a joining `newcomer` (X-β3.2-3). All hex is
/// lowercase, no `0x`. Signer sets are packed EXACTLY as the enclave/Deliver
/// wire — `count * 24` bytes, each entry `account_id[20] || weight_be[4]` — so
/// the source handler can hand these straight to `bootstrap-bundle-export` and
/// into `BootstrapMessage::Deliver` without re-encoding.
///
/// `authority_*` is the set the current epoch installed (M — the enclave's
/// sealed set). `attesting_*` is the OUTGOING (M-1) set whose quorum signed the
/// M transition (at genesis attesting == authority — self-authorising). This is
/// a replica of enclave-verified state: the newcomer re-verifies the M-1 quorum
/// and the escrow-bound hash in-enclave on import, so a tampered row can only
/// fail an export, never forge a membership.
#[derive(Debug, Clone)]
pub struct CurrentMembershipBundle {
    pub escrow_hex: String,
    pub authority_epoch: u64,
    pub confirmed_epoch: u64,
    pub prev_epoch_hash_hex: String,
    pub authority_signers_hex: String,
    pub authority_signer_count: u32,
    pub authority_quorum: u32,
    pub attesting_signers_hex: String,
    pub attesting_signer_count: u32,
    pub attesting_quorum: u32,
    pub quorum_bundle_hex: String,
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

    /// REQ-20-impl R2 commit 3: mirror a deposit binding into the DB
    /// after the enclave returned DB_OK from
    /// ecall_perp_register_deposit_binding. The enclave is the source
    /// of truth; this row is for operator dashboards / audit cross-check.
    /// ON CONFLICT DO NOTHING handles DB_IDEMPOTENT_OK and replication
    /// races — never overwrite an existing binding.
    pub async fn insert_deposit_binding(
        &self,
        user_id: &str,
        sender_addr: &str,
        dest_tag: u32,
        bound_at_ms: u64,
        bound_via_probe_tx_hash: &str,
    ) {
        let r = sqlx::query(
            "INSERT INTO deposit_bindings \
             (user_id, sender_addr, dest_tag, bound_at_ms, bound_via_probe_tx_hash) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (sender_addr, dest_tag) DO NOTHING",
        )
        .bind(user_id)
        .bind(sender_addr)
        .bind(dest_tag as i64)
        .bind(bound_at_ms as i64)
        .bind(bound_via_probe_tx_hash)
        .execute(&self.pool)
        .await;

        if let Err(e) = r {
            error!("pg insert_deposit_binding failed: {}", e);
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

    // ── Funding events ────────────────────────────────────────────

    /// Record per-position funding payment (called once per open position per funding tick).
    #[allow(clippy::too_many_arguments)] // mirrors the funding_payments table schema
    pub async fn insert_funding_payment(
        &self,
        user_id: &str,
        position_id: i64,
        side: &str,
        payment: i64,
        funding_rate: i64,
        mark_price: i64,
        timestamp_epoch: u64,
    ) {
        let r = sqlx::query(
            "INSERT INTO funding_payments \
             (user_id, position_id, side, payment, funding_rate, mark_price, timestamp_epoch) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(user_id)
        .bind(position_id)
        .bind(side)
        .bind(payment)
        .bind(funding_rate)
        .bind(mark_price)
        .bind(timestamp_epoch as i64)
        .execute(&self.pool)
        .await;
        if let Err(e) = r {
            error!("pg insert_funding_payment failed: {}", e);
        }
    }

    /// Record a funding application event (aggregate).
    pub async fn insert_funding_event(
        &self,
        funding_rate: i64,
        mark_price: i64,
        index_price: i64,
        timestamp_epoch: u64,
    ) {
        let r = sqlx::query(
            "INSERT INTO funding_events (funding_rate, mark_price, index_price, timestamp_epoch) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (timestamp_epoch) DO NOTHING",
        )
        .bind(funding_rate)
        .bind(mark_price)
        .bind(index_price)
        .bind(timestamp_epoch as i64)
        .execute(&self.pool)
        .await;
        if let Err(e) = r {
            error!("pg insert_funding_event failed: {}", e);
        }
    }

    // ── Resting orders (C5.1 orderbook persistence for failover) ──

    /// O-H4: insert a fresh resting order, carrying the user's XRPL
    /// signature binding so the row can be re-verified on failover reload.
    /// Idempotent on (order_id): if the row already exists we assume it
    /// was inserted by the sequencer and just touch `filled` — signature
    /// columns are never overwritten, preserving the original binding.
    pub async fn insert_resting_order(
        &self,
        o: &crate::orderbook::Order,
        binding: &crate::auth::OrderSignatureBinding,
    ) {
        let r = sqlx::query(
            "INSERT INTO resting_orders \
             (order_id, user_id, market, side, price, size, filled, leverage, reduce_only, timestamp_ms, client_order_id, \
              signed_body_hex, signature_hex, signer_timestamp, signer_address, signer_pubkey_hex) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) \
             ON CONFLICT (order_id) DO UPDATE SET filled = $7",
        )
        .bind(o.id as i64)
        .bind(&o.user_id)
        .bind(&o.market)
        .bind(format!("{}", o.side))
        .bind(o.price.raw())
        .bind(o.size.raw())
        .bind(o.filled.raw())
        .bind(o.leverage as i32)
        .bind(o.reduce_only)
        .bind(o.timestamp_ms as i64)
        .bind(&o.client_order_id)
        .bind(&binding.signed_body_hex)
        .bind(&binding.signature_hex)
        .bind(&binding.timestamp)
        .bind(&binding.signer_address)
        .bind(&binding.signer_pubkey_hex)
        .execute(&self.pool)
        .await;
        if let Err(e) = r {
            error!("pg insert_resting_order failed: {}", e);
        }
    }

    /// Update only the `filled` column on an existing resting order.
    /// Used for subsequent partial-fill updates where the original binding
    /// is not available in memory (the engine doesn't carry it).
    pub async fn update_resting_order_filled(&self, order_id: u64, filled: FP8) {
        let r = sqlx::query("UPDATE resting_orders SET filled = $2 WHERE order_id = $1")
            .bind(order_id as i64)
            .bind(filled.raw())
            .execute(&self.pool)
            .await;
        if let Err(e) = r {
            error!("pg update_resting_order_filled failed: {}", e);
        }
    }

    /// Remove a resting order (filled or cancelled).
    pub async fn delete_resting_order(&self, order_id: u64) {
        let r = sqlx::query("DELETE FROM resting_orders WHERE order_id = $1")
            .bind(order_id as i64)
            .execute(&self.pool)
            .await;
        if let Err(e) = r {
            error!("pg delete_resting_order failed: {}", e);
        }
    }

    /// O-H4: load all resting orders from PG, re-verifying each row's
    /// signature binding before returning it. Rows whose signature doesn't
    /// validate, or whose `user_id` doesn't match the signer's address, are
    /// dropped and logged — a compromised PG cannot poison the reloaded
    /// book with forged orders.
    pub async fn load_resting_orders(&self) -> Vec<crate::orderbook::Order> {
        #[allow(clippy::type_complexity)]
        let rows = sqlx::query_as::<
            _,
            (
                i64,
                String,
                String,
                String,
                i64,
                i64,
                i64,
                i32,
                bool,
                i64,
                Option<String>,
                String,
                String,
                String,
                String,
                String,
            ),
        >(
            "SELECT order_id, user_id, market, side, price, size, filled, leverage, reduce_only, timestamp_ms, client_order_id, \
                    signed_body_hex, signature_hex, signer_timestamp, signer_address, signer_pubkey_hex \
             FROM resting_orders ORDER BY order_id",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let mut out = Vec::with_capacity(rows.len());
        let mut rejected = 0usize;
        for (
            id,
            user_id,
            market,
            side,
            price,
            size,
            filled,
            leverage,
            reduce_only,
            ts,
            coid,
            signed_body_hex,
            signature_hex,
            signer_timestamp,
            signer_address,
            signer_pubkey_hex,
        ) in rows
        {
            let binding = crate::auth::OrderSignatureBinding {
                signed_body_hex,
                signature_hex,
                timestamp: signer_timestamp,
                signer_address: signer_address.clone(),
                signer_pubkey_hex,
            };
            if let Err(e) = crate::auth::verify_signature_only(&binding) {
                warn!(order_id = id, %user_id, "resting order rejected on reload: {}", e);
                rejected += 1;
                continue;
            }
            if signer_address != user_id {
                warn!(
                    order_id = id,
                    %user_id,
                    stored_signer = %signer_address,
                    "resting order rejected on reload: signer_address != user_id"
                );
                rejected += 1;
                continue;
            }
            let side_enum = match side.as_str() {
                "long" | "buy" => crate::types::Side::Long,
                _ => crate::types::Side::Short,
            };
            out.push(crate::orderbook::Order {
                id: id as u64,
                user_id,
                market,
                side: side_enum,
                order_type: crate::orderbook::OrderType::Limit,
                price: FP8(price),
                size: FP8(size),
                filled: FP8(filled),
                leverage: leverage as u32,
                status: crate::orderbook::OrderStatus::Open,
                time_in_force: crate::orderbook::TimeInForce::Gtc,
                reduce_only,
                timestamp_ms: ts as u64,
                client_order_id: coid,
                close_position_id: None,
            });
        }
        if rejected > 0 {
            warn!(
                rejected,
                accepted = out.len(),
                "load_resting_orders: some rows failed signature re-verification"
            );
        }
        out
    }

    /// Query trade history for a user.
    pub async fn get_user_trades(&self, user_id: &str, limit: i64) -> Vec<serde_json::Value> {
        let rows = sqlx::query_as::<_, (i64, String, i64, i64, String, i64)>(
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

    /// REQ-β3.2c-impl (D-1 PERSIST): replace the single retained current-epoch
    /// membership tuple. Called at each successful `Seal`/`Bootstrap` apply so
    /// that any in-sync node — including one restarted after the change — can
    /// serve a joining newcomer (`bootstrap-bundle-export`). Fire-and-forget +
    /// log, like every other replica write here: a persist failure must never
    /// block the seal (the enclave's sealed state stays canonical); it only
    /// means this node cannot SERVE a join until the next change re-populates.
    pub async fn upsert_current_membership_bundle(&self, b: &CurrentMembershipBundle) {
        let r = sqlx::query(
            "INSERT INTO current_membership_bundle \
             (id, escrow_hex, authority_epoch, confirmed_epoch, prev_epoch_hash_hex, \
              authority_signers_hex, authority_signer_count, authority_quorum, \
              attesting_signers_hex, attesting_signer_count, attesting_quorum, \
              quorum_bundle_hex, updated_at) \
             VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW()) \
             ON CONFLICT (id) DO UPDATE SET \
              escrow_hex = EXCLUDED.escrow_hex, \
              authority_epoch = EXCLUDED.authority_epoch, \
              confirmed_epoch = EXCLUDED.confirmed_epoch, \
              prev_epoch_hash_hex = EXCLUDED.prev_epoch_hash_hex, \
              authority_signers_hex = EXCLUDED.authority_signers_hex, \
              authority_signer_count = EXCLUDED.authority_signer_count, \
              authority_quorum = EXCLUDED.authority_quorum, \
              attesting_signers_hex = EXCLUDED.attesting_signers_hex, \
              attesting_signer_count = EXCLUDED.attesting_signer_count, \
              attesting_quorum = EXCLUDED.attesting_quorum, \
              quorum_bundle_hex = EXCLUDED.quorum_bundle_hex, \
              updated_at = NOW()",
        )
        .bind(&b.escrow_hex)
        .bind(b.authority_epoch as i64)
        .bind(b.confirmed_epoch as i64)
        .bind(&b.prev_epoch_hash_hex)
        .bind(&b.authority_signers_hex)
        .bind(b.authority_signer_count as i32)
        .bind(b.authority_quorum as i32)
        .bind(&b.attesting_signers_hex)
        .bind(b.attesting_signer_count as i32)
        .bind(b.attesting_quorum as i32)
        .bind(&b.quorum_bundle_hex)
        .execute(&self.pool)
        .await;

        match r {
            Ok(_) => info!(
                epoch = b.authority_epoch,
                "retained current membership bundle (epoch {})", b.authority_epoch
            ),
            Err(e) => error!("pg upsert_current_membership_bundle failed: {}", e),
        }
    }

    /// REQ-β3.2c-impl: load the single retained current-epoch membership tuple,
    /// or `None` if this node has never persisted one (fresh DB) or on any read
    /// error. The `source` handler uses this to decide whether it can serve a
    /// join; `warn` (not `error`) on a read failure — absence is a normal state.
    pub async fn load_current_membership_bundle(&self) -> Option<CurrentMembershipBundle> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                i64,
                i64,
                String,
                String,
                i32,
                i32,
                String,
                i32,
                i32,
                String,
            ),
        >(
            "SELECT escrow_hex, authority_epoch, confirmed_epoch, prev_epoch_hash_hex, \
              authority_signers_hex, authority_signer_count, authority_quorum, \
              attesting_signers_hex, attesting_signer_count, attesting_quorum, \
              quorum_bundle_hex \
             FROM current_membership_bundle WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await;

        match row {
            Ok(Some((
                escrow_hex,
                authority_epoch,
                confirmed_epoch,
                prev_epoch_hash_hex,
                authority_signers_hex,
                authority_signer_count,
                authority_quorum,
                attesting_signers_hex,
                attesting_signer_count,
                attesting_quorum,
                quorum_bundle_hex,
            ))) => Some(CurrentMembershipBundle {
                escrow_hex,
                authority_epoch: authority_epoch as u64,
                confirmed_epoch: confirmed_epoch as u64,
                prev_epoch_hash_hex,
                authority_signers_hex,
                authority_signer_count: authority_signer_count as u32,
                authority_quorum: authority_quorum as u32,
                attesting_signers_hex,
                attesting_signer_count: attesting_signer_count as u32,
                attesting_quorum: attesting_quorum as u32,
                quorum_bundle_hex,
            }),
            Ok(None) => None,
            Err(e) => {
                warn!("pg load_current_membership_bundle failed: {}", e);
                None
            }
        }
    }

    /// Query funding payment history for a user.
    pub async fn get_user_funding(&self, user_id: &str, limit: i64) -> Vec<serde_json::Value> {
        let rows = sqlx::query_as::<_, (i64, i64, String, i64)>(
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
