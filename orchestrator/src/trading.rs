//! Trading engine: wires order book fills to enclave margin checks.
//!
//! Flow: user submits order → orderbook matches → for each fill,
//! call enclave open_position for both maker and taker.

use anyhow::{bail, Result};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::orderbook::{Order, OrderBook, OrderStatus, OrderType, TimeInForce, Trade};
use crate::perp_client::PerpClient;
use crate::types::{FP8, Side};

/// Trading engine: orderbook + enclave integration.
pub struct TradingEngine {
    pub book: Mutex<OrderBook>,
    perp: PerpClient,
}

/// Result of submitting an order.
#[derive(Debug)]
pub struct OrderResult {
    pub order: Order,
    pub trades: Vec<Trade>,
    pub failed_fills: Vec<FailedFill>,
}

/// A fill that was rejected by the enclave (margin insufficient).
#[derive(Debug)]
pub struct FailedFill {
    pub trade: Trade,
    pub maker_error: Option<String>,
    pub taker_error: Option<String>,
}

impl TradingEngine {
    pub fn new(market: &str, perp: PerpClient) -> Self {
        TradingEngine {
            book: Mutex::new(OrderBook::new(market)),
            perp,
        }
    }

    /// Submit an order: match on the book, then settle fills via enclave.
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
        // Step 1: Match on the order book
        let (order, trades) = {
            let mut book = self.book.lock().await;
            book.submit_order(
                user_id, side, order_type, price, size, leverage,
                time_in_force, reduce_only, client_order_id,
            )?
        };

        if trades.is_empty() {
            return Ok(OrderResult {
                order,
                trades,
                failed_fills: Vec::new(),
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

            // Open position for taker
            let taker_result = self.perp.open_position(
                &trade.taker_user_id,
                taker_side,
                &fill_size,
                &fill_price,
                leverage,
            ).await;

            let taker_err = match &taker_result {
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
                Err(e) => {
                    let msg = format!("{}", e);
                    error!(trade_id = trade.trade_id, user = %trade.taker_user_id, "taker position error: {}", msg);
                    Some(msg)
                }
            };

            // Open position for maker
            let maker_result = self.perp.open_position(
                &trade.maker_user_id,
                maker_side,
                &fill_size,
                &fill_price,
                leverage,
            ).await;

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
                failed_fills.push(FailedFill {
                    trade: trade.clone(),
                    maker_error: maker_err,
                    taker_error: taker_err,
                });
            }
        }

        Ok(OrderResult {
            order,
            trades,
            failed_fills,
        })
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
