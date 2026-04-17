//! Trading engine: wires order book fills to enclave margin checks.
//!
//! Flow: user submits order → orderbook matches → for each fill,
//! call enclave open_position for both maker and taker.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

use crate::orderbook::{Order, OrderBook, OrderType, TimeInForce, Trade};
use crate::p2p::{FillMessage, OrderBatch, OrderMessage};
use crate::perp_client::PerpClient;
use crate::types::{Side, FP8};

/// Trading engine: orderbook + enclave integration + P2P batch publishing.
pub struct TradingEngine {
    pub book: Mutex<OrderBook>,
    perp: PerpClient,
    /// Channel to send order batches to P2P layer (sequencer mode).
    batch_tx: Option<mpsc::Sender<OrderBatch>>,
    /// Monotonic batch sequence number.
    seq_num: AtomicU64,
    /// Our P2P peer ID (for sequencer_id in batches).
    peer_id: String,
}

/// Result of submitting an order.
#[derive(Debug)]
pub struct OrderResult {
    pub order: Order,
    pub trades: Vec<Trade>,
    pub failed_fills: Vec<FailedFill>,
    /// Resting orders cancelled by self-trade prevention (decrement-and-cancel).
    /// Each entry is (the maker order's current state, amount cancelled).
    pub stp_cancelled: Vec<(Order, i64)>,
}

/// A fill that was rejected by the enclave (margin insufficient).
#[derive(Debug)]
#[allow(dead_code)]
pub struct FailedFill {
    pub trade: Trade,
    pub maker_error: Option<String>,
    pub taker_error: Option<String>,
}

impl TradingEngine {
    pub fn new(market: &str, perp: PerpClient, peer_id: &str) -> Self {
        TradingEngine {
            book: Mutex::new(OrderBook::new(market)),
            perp,
            batch_tx: None,
            seq_num: AtomicU64::new(1),
            peer_id: peer_id.to_string(),
        }
    }

    /// Enable P2P batch publishing (sequencer mode).
    pub fn with_batch_publisher(mut self, tx: mpsc::Sender<OrderBatch>) -> Self {
        self.batch_tx = Some(tx);
        self
    }

    /// Submit an order: match on the book, then settle fills via enclave.
    #[allow(clippy::too_many_arguments)]
    pub async fn submit_order(
        &self,
        user_id: String,
        side: Side,
        order_type: OrderType,
        price: FP8,
        size: FP8,
        leverage: u32,
        time_in_force: TimeInForce,
        reduce_only: bool,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        self.submit_order_inner(
            user_id, side, order_type, price, size, leverage,
            time_in_force, reduce_only, client_order_id, None,
        ).await
    }

    /// Submit a close order that routes through the CLOB.
    /// The taker side of fills will call close_position instead of open_position.
    #[allow(clippy::too_many_arguments)]
    pub async fn submit_close_order(
        &self,
        user_id: String,
        close_side: Side,
        size: FP8,
        leverage: u32,
        close_position_id: u32,
    ) -> Result<OrderResult> {
        self.submit_order_inner(
            user_id, close_side, OrderType::Market, FP8::ZERO, size, leverage,
            TimeInForce::Ioc, true, None, Some(close_position_id),
        ).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit_order_inner(
        &self,
        user_id: String,
        side: Side,
        order_type: OrderType,
        price: FP8,
        size: FP8,
        leverage: u32,
        time_in_force: TimeInForce,
        reduce_only: bool,
        client_order_id: Option<String>,
        close_position_id: Option<u32>,
    ) -> Result<OrderResult> {
        // Step 0: Pre-check margin in enclave before matching (skip for close orders)
        if close_position_id.is_none() {
            let balance = self.perp.get_balance(&user_id).await;
            if let Ok(bal) = &balance {
                if let Some(avail_str) = bal["data"]["available_margin"].as_str() {
                    if let Ok(avail) = avail_str.parse::<FP8>() {
                        let est_price = if price.raw() > 0 {
                            price
                        } else {
                            FP8::from_f64(1.0)
                        };
                        let notional = size * est_price;
                        let est_margin = FP8(notional.raw() / leverage as i64);
                        if avail.raw() < est_margin.raw() {
                            anyhow::bail!(
                                "insufficient margin: available={}, required~={}",
                                avail,
                                est_margin
                            );
                        }
                    }
                }
            }
        }

        // Step 1: Match on the order book
        let (mut order, trades, stp_cancelled) = {
            let mut book = self.book.lock().await;
            book.submit_order(
                user_id,
                side,
                order_type,
                price,
                size,
                leverage,
                time_in_force,
                reduce_only,
                client_order_id,
            )?
        };
        order.close_position_id = close_position_id;

        if trades.is_empty() {
            return Ok(OrderResult {
                order,
                trades,
                failed_fills: Vec::new(),
                stp_cancelled,
            });
        }

        // Step 2: For each fill, open positions in enclave
        let mut failed_fills = Vec::new();

        for trade in &trades {
            let fill_price = trade.price.to_string();
            let fill_size = trade.size.to_string();

            // Determine sides
            let (maker_side, taker_side) = match trade.taker_side {
                Side::Long => ("short", "long"),
                Side::Short => ("long", "short"),
            };

            // Settle taker: close_position if this is a close order, else open_position
            let taker_err = if let Some(pos_id) = close_position_id {
                let result = self
                    .perp
                    .close_position(&trade.taker_user_id, pos_id as u64, &fill_price)
                    .await;
                match &result {
                    Ok(v) => {
                        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
                        if status == "success" {
                            info!(
                                trade_id = trade.trade_id,
                                user = %trade.taker_user_id,
                                position_id = pos_id,
                                price = %trade.price,
                                "position closed via CLOB"
                            );
                            None
                        } else {
                            let msg = format!("enclave close returned: {}", v);
                            warn!(trade_id = trade.trade_id, "close_position failed: {}", msg);
                            Some(msg)
                        }
                    }
                    Err(e) => Some(format!("{}", e)),
                }
            } else {
                let result = self
                    .perp
                    .open_position(
                        &trade.taker_user_id,
                        taker_side,
                        &fill_size,
                        &fill_price,
                        leverage,
                    )
                    .await;
                match &result {
                    Ok(v) => {
                        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
                        if status == "success" {
                            info!(
                                trade_id = trade.trade_id,
                                user = %trade.taker_user_id,
                                side = taker_side,
                                size = %trade.size,
                                price = %trade.price,
                                "taker position opened"
                            );
                            None
                        } else {
                            let msg = format!("enclave returned: {}", v);
                            warn!(trade_id = trade.trade_id, user = %trade.taker_user_id, "taker position failed: {}", msg);
                            Some(msg)
                        }
                    }
                    Err(e) => Some(format!("{}", e)),
                }
            };

            // Open position for maker
            let maker_result = self
                .perp
                .open_position(
                    &trade.maker_user_id,
                    maker_side,
                    &fill_size,
                    &fill_price,
                    leverage,
                )
                .await;

            let maker_err = match &maker_result {
                Ok(v) => {
                    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
                    if status == "success" {
                        info!(
                            trade_id = trade.trade_id,
                            user = %trade.maker_user_id,
                            side = maker_side,
                            size = %trade.size,
                            price = %trade.price,
                            "maker position opened"
                        );
                        None
                    } else {
                        let msg = format!("enclave returned: {}", v);
                        warn!(trade_id = trade.trade_id, user = %trade.maker_user_id, "maker position failed: {}", msg);
                        Some(msg)
                    }
                }
                Err(e) => {
                    let msg = format!("{}", e);
                    error!(trade_id = trade.trade_id, user = %trade.maker_user_id, "maker position error: {}", msg);
                    Some(msg)
                }
            };

            if taker_err.is_some() || maker_err.is_some() {
                // WARNING: orderbook has consumed maker liquidity but enclave rejected position.
                // This is a known limitation — proper fix requires tentative matching with rollback.
                // For now, log the failure prominently so it can be investigated.
                error!(
                    trade_id = trade.trade_id,
                    price = %trade.price,
                    size = %trade.size,
                    "FILL REJECTED BY ENCLAVE — orderbook state inconsistent"
                );
                failed_fills.push(FailedFill {
                    trade: trade.clone(),
                    maker_error: maker_err,
                    taker_error: taker_err,
                });
            }
        }

        // Publish batch via P2P (sequencer mode)
        if !trades.is_empty() {
            if let Some(tx) = &self.batch_tx {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                let batch_seq = self.seq_num.fetch_add(1, Ordering::SeqCst);
                let batch = OrderBatch {
                    seq_num: batch_seq,
                    orders: vec![OrderMessage {
                        order_id: order.id,
                        user_id: order.user_id.clone(),
                        side: format!("{}", order.side),
                        order_type: format!("{:?}", order.order_type),
                        price: order.price.to_string(),
                        size: order.size.to_string(),
                        leverage: order.leverage,
                        status: format!("{:?}", order.status),
                        fills: trades
                            .iter()
                            .map(|t| FillMessage {
                                trade_id: t.trade_id,
                                maker_order_id: t.maker_order_id,
                                taker_order_id: t.taker_order_id,
                                maker_user_id: t.maker_user_id.clone(),
                                price: t.price.to_string(),
                                size: t.size.to_string(),
                                taker_side: format!("{}", t.taker_side),
                            })
                            .collect(),
                    }],
                    state_hash: {
                        // State hash = SHA-256 of (seq_num || trades || timestamp)
                        use sha2::{Digest, Sha256};
                        let mut hasher = Sha256::new();
                        hasher.update(batch_seq.to_le_bytes());
                        for t in &trades {
                            hasher.update(t.trade_id.to_le_bytes());
                            hasher.update(t.price.raw().to_le_bytes());
                            hasher.update(t.size.raw().to_le_bytes());
                        }
                        hasher.update(now.to_le_bytes());
                        hex::encode(hasher.finalize())
                    },
                    timestamp: now,
                    sequencer_id: self.peer_id.clone(),
                };

                if let Err(e) = tx.send(batch).await {
                    warn!("failed to send batch to P2P: {}", e);
                }
            }
        }

        Ok(OrderResult {
            order,
            trades,
            failed_fills,
            stp_cancelled,
        })
    }

    /// Look up order owner without modifying the book.
    pub async fn order_owner(&self, order_id: u64) -> Option<String> {
        let book = self.book.lock().await;
        // Search bids and asks for the order
        for level in book.bids.values() {
            for o in &level.orders {
                if o.id == order_id {
                    return Some(o.user_id.clone());
                }
            }
        }
        for level in book.asks.values() {
            for o in &level.orders {
                if o.id == order_id {
                    return Some(o.user_id.clone());
                }
            }
        }
        None
    }

    /// Get a resting order by ID (for PG sync after matching).
    pub async fn get_order(&self, order_id: u64) -> Option<Order> {
        let book = self.book.lock().await;
        book.get_order(order_id).cloned()
    }

    /// Load resting orders into the book (failover rebuild from PG).
    pub async fn load_orders(&self, orders: Vec<Order>) {
        let mut book = self.book.lock().await;
        book.load_orders(orders);
    }

    /// Cancel an order.
    pub async fn cancel_order(&self, order_id: u64) -> Result<Order> {
        let mut book = self.book.lock().await;
        book.cancel_order(order_id)
    }

    /// Cancel all orders for a user.
    pub async fn cancel_all(&self, user_id: &str) -> Vec<Order> {
        let mut book = self.book.lock().await;
        book.cancel_all(user_id)
    }

    /// Get order book depth.
    pub async fn depth(&self, levels: usize) -> (Vec<(FP8, FP8)>, Vec<(FP8, FP8)>) {
        let book = self.book.lock().await;
        book.depth(levels)
    }

    /// Get user's open orders.
    pub async fn user_orders(&self, user_id: &str) -> Vec<Order> {
        let book = self.book.lock().await;
        book.user_orders(user_id).into_iter().cloned().collect()
    }

    /// Get recent trades.
    pub async fn recent_trades(&self) -> Vec<Trade> {
        let book = self.book.lock().await;
        book.recent_trades.clone()
    }

    /// Get best bid/ask.
    pub async fn ticker(&self) -> (Option<FP8>, Option<FP8>, Option<FP8>) {
        let book = self.book.lock().await;
        (book.best_bid(), book.best_ask(), book.mid_price())
    }
}
