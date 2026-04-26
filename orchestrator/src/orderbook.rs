//! Central Limit Order Book (CLOB) with price-time priority matching.
//!
//! Orders live in the orchestrator (not in the enclave).
//! When a match occurs, the orchestrator calls enclave `open_position`
//! for both maker and taker to validate margin.

use std::collections::BTreeMap;
use std::time::SystemTime;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::types::{Side, FP8};

// ── Order types ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    /// Good-Till-Cancelled
    Gtc,
    /// Immediate-Or-Cancel
    Ioc,
    /// Fill-Or-Kill
    Fok,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
}

// ── Order ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: u64,
    pub user_id: String,
    pub market: String,
    pub side: Side,
    pub order_type: OrderType,
    pub price: FP8,
    pub size: FP8,
    pub filled: FP8,
    pub leverage: u32,
    pub status: OrderStatus,
    pub time_in_force: TimeInForce,
    pub reduce_only: bool,
    pub timestamp_ms: u64,
    pub client_order_id: Option<String>,
    /// When set, fills on this order call close_position instead of open_position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_position_id: Option<u32>,
}

impl Order {
    pub fn remaining(&self) -> FP8 {
        self.size - self.filled
    }
}

// ── Trade (fill record) ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub trade_id: u64,
    pub market: String,
    pub maker_order_id: u64,
    pub taker_order_id: u64,
    pub maker_user_id: String,
    pub taker_user_id: String,
    pub price: FP8,
    pub size: FP8,
    pub taker_side: Side,
    pub timestamp_ms: u64,
}

// ── Price level: orders at the same price ───────────────────────

#[derive(Debug, Clone)]
pub(crate) struct PriceLevel {
    pub(crate) orders: Vec<Order>,
}

impl PriceLevel {
    fn new() -> Self {
        PriceLevel { orders: Vec::new() }
    }

    fn add(&mut self, order: Order) {
        self.orders.push(order);
    }

    fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    fn total_size(&self) -> FP8 {
        self.orders
            .iter()
            .map(|o| o.remaining())
            .fold(FP8::ZERO, |a, b| a + b)
    }
}

// ── Order Book ──────────────────────────────────────────────────

/// CLOB for a single market (e.g., XRP-USD-PERP).
pub struct OrderBook {
    pub market: String,

    /// Bids: price → orders (BTreeMap = sorted ascending, we iterate from highest)
    pub(crate) bids: BTreeMap<i64, PriceLevel>,

    /// Asks: price → orders (BTreeMap = sorted ascending, we iterate from lowest)
    pub(crate) asks: BTreeMap<i64, PriceLevel>,

    next_order_id: u64,
    next_trade_id: u64,

    /// Recent trades (last N)
    pub recent_trades: Vec<Trade>,
    max_recent_trades: usize,

    /// O-M4: per-user rate limit on Self-Trade-Prevention events.
    /// STP is "decrement-and-cancel" — cheap per-event but a grief
    /// attacker bouncing self-crosses through their own resting
    /// liquidity (e.g. through the vault MM) amplifies cancel-and-
    /// replace pressure on PG and gossipsub. Cap STP triggers at
    /// 30/min/user. Records ONLY when an STP event actually fires;
    /// non-self-crossing orders are not counted.
    stp_rate: crate::rate_limit::RateLimiter,
}

impl OrderBook {
    pub fn new(market: &str) -> Self {
        OrderBook {
            market: market.to_string(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            next_order_id: 1,
            next_trade_id: 1,
            recent_trades: Vec::new(),
            max_recent_trades: 1000,
            stp_rate: crate::rate_limit::RateLimiter::new(std::time::Duration::from_secs(60), 30),
        }
    }

    /// Best bid price (highest buy order).
    pub fn best_bid(&self) -> Option<FP8> {
        self.bids.keys().next_back().map(|&p| FP8(p))
    }

    /// Best ask price (lowest sell order).
    pub fn best_ask(&self) -> Option<FP8> {
        self.asks.keys().next().map(|&p| FP8(p))
    }

    /// Mid price.
    pub fn mid_price(&self) -> Option<FP8> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some(FP8((bid.0 + ask.0) / 2)),
            _ => None,
        }
    }

    /// Spread.
    #[allow(dead_code)]
    pub fn spread(&self) -> Option<FP8> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) if ask.0 > bid.0 => Some(ask - bid),
            _ => None,
        }
    }

    /// Submit a new order. Returns (order, fills, stp_cancelled).
    /// Fills are produced immediately if the order crosses the book.
    /// `stp_cancelled` lists any of the user's resting orders that were
    /// cancelled by self-trade prevention to make way for this new order.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn submit_order(
        &mut self,
        user_id: String,
        side: Side,
        order_type: OrderType,
        price: FP8,
        size: FP8,
        leverage: u32,
        time_in_force: TimeInForce,
        reduce_only: bool,
        client_order_id: Option<String>,
    ) -> Result<(Order, Vec<Trade>, Vec<(Order, i64)>)> {
        if size.0 <= 0 {
            bail!("order size must be positive");
        }
        if order_type == OrderType::Limit && price.0 <= 0 {
            bail!("limit order price must be positive");
        }
        // O-M4: gate before matching. If the user has already triggered
        // 30 STP events in the trailing minute, refuse the new order
        // entirely — the next match would either fill normally or
        // trigger another STP, but we want to bound the latter.
        if !self.stp_rate.peek(&user_id) {
            bail!("self-trade rate exceeded: too many self-crossing orders in the last minute");
        }

        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let order_id = self.next_order_id;
        self.next_order_id += 1;

        let mut order = Order {
            id: order_id,
            user_id,
            market: self.market.clone(),
            side,
            order_type,
            price,
            size,
            filled: FP8::ZERO,
            leverage,
            status: OrderStatus::Open,
            time_in_force,
            reduce_only,
            timestamp_ms: now_ms,
            client_order_id,
            close_position_id: None,
        };

        // FOK pre-check: verify sufficient liquidity before matching
        if time_in_force == TimeInForce::Fok {
            let available = self.available_liquidity(&order);
            if available < size.0 {
                order.status = OrderStatus::Cancelled;
                return Ok((order, Vec::new(), Vec::new()));
            }
        }

        // Match against resting orders
        let (trades, stp_cancelled) = self.match_order(&mut order);

        // O-M4: record an STP event if any self-crosses fired. We
        // record AFTER the match so non-crossing orders never consume
        // budget — only repeated bouncing through own resting orders
        // does. This keeps the cap functional for grief patterns
        // without penalising legit traffic.
        if !stp_cancelled.is_empty() {
            self.stp_rate.record(&order.user_id);
        }

        // If STP shrank the taker size to zero, the order is fully self-cancelled.
        if order.size.0 == 0 {
            order.status = OrderStatus::Cancelled;
        } else {
            // Handle remaining quantity
            match order_type {
                OrderType::Market => {
                    // Market order: cancel unfilled remainder
                    if order.remaining().0 > 0 {
                        order.status = if order.filled.0 > 0 {
                            OrderStatus::PartiallyFilled
                        } else {
                            OrderStatus::Cancelled
                        };
                    }
                }
                OrderType::Limit => match time_in_force {
                    TimeInForce::Ioc => {
                        // IOC: cancel unfilled remainder
                        if order.remaining().0 > 0 && order.filled.0 > 0 {
                            order.status = OrderStatus::PartiallyFilled;
                        } else if order.filled.0 == 0 {
                            order.status = OrderStatus::Cancelled;
                        }
                    }
                    TimeInForce::Fok => {
                        // Already pre-checked — if we get here, it was fully filled
                    }
                    TimeInForce::Gtc => {
                        // GTC: rest unfilled on the book
                        if order.remaining().0 > 0 && order.status == OrderStatus::Open {
                            self.add_to_book(order.clone());
                        }
                    }
                },
            }

            if order.filled == order.size {
                order.status = OrderStatus::Filled;
            }
        }

        if !trades.is_empty() {
            info!(
                order_id,
                fills = trades.len(),
                filled = %order.filled,
                remaining = %order.remaining(),
                "order matched"
            );
        }

        if !stp_cancelled.is_empty() {
            info!(
                order_id,
                cancelled = stp_cancelled.len(),
                "self-trade prevention: cancelled crossing resting orders"
            );
        }

        Ok((order, trades, stp_cancelled))
    }

    /// Cancel an order by ID. Returns the cancelled order or error.
    pub fn cancel_order(&mut self, order_id: u64) -> Result<Order> {
        // Search bids
        for level in self.bids.values_mut() {
            if let Some(pos) = level.orders.iter().position(|o| o.id == order_id) {
                let mut order = level.orders.remove(pos);
                order.status = OrderStatus::Cancelled;
                self.bids.retain(|_, l| !l.is_empty());
                return Ok(order);
            }
        }
        // Search asks
        for level in self.asks.values_mut() {
            if let Some(pos) = level.orders.iter().position(|o| o.id == order_id) {
                let mut order = level.orders.remove(pos);
                order.status = OrderStatus::Cancelled;
                self.asks.retain(|_, l| !l.is_empty());
                return Ok(order);
            }
        }
        bail!("order {order_id} not found")
    }

    /// Cancel all orders for a user.
    pub fn cancel_all(&mut self, user_id: &str) -> Vec<Order> {
        let mut cancelled = Vec::new();
        for level in self.bids.values_mut() {
            level.orders.retain(|o| {
                if o.user_id == user_id {
                    let mut c = o.clone();
                    c.status = OrderStatus::Cancelled;
                    cancelled.push(c);
                    false
                } else {
                    true
                }
            });
        }
        for level in self.asks.values_mut() {
            level.orders.retain(|o| {
                if o.user_id == user_id {
                    let mut c = o.clone();
                    c.status = OrderStatus::Cancelled;
                    cancelled.push(c);
                    false
                } else {
                    true
                }
            });
        }
        self.bids.retain(|_, l| !l.is_empty());
        self.asks.retain(|_, l| !l.is_empty());
        cancelled
    }

    /// Get all open orders for a user.
    pub fn user_orders(&self, user_id: &str) -> Vec<&Order> {
        let mut orders = Vec::new();
        for level in self.bids.values() {
            for o in &level.orders {
                if o.user_id == user_id {
                    orders.push(o);
                }
            }
        }
        for level in self.asks.values() {
            for o in &level.orders {
                if o.user_id == user_id {
                    orders.push(o);
                }
            }
        }
        orders
    }

    /// Get an order by ID from either side of the book.
    pub fn get_order(&self, order_id: u64) -> Option<&Order> {
        for level in self.bids.values() {
            if let Some(o) = level.orders.iter().find(|o| o.id == order_id) {
                return Some(o);
            }
        }
        for level in self.asks.values() {
            if let Some(o) = level.orders.iter().find(|o| o.id == order_id) {
                return Some(o);
            }
        }
        None
    }

    /// Load resting orders into the book (failover rebuild from PG).
    /// Also updates next_order_id and next_trade_id to avoid collisions.
    pub fn load_orders(&mut self, orders: Vec<Order>) {
        for order in orders {
            if order.id >= self.next_order_id {
                self.next_order_id = order.id + 1;
            }
            self.add_to_book(order);
        }
        info!(
            bids = self.bids.values().map(|l| l.orders.len()).sum::<usize>(),
            asks = self.asks.values().map(|l| l.orders.len()).sum::<usize>(),
            next_order_id = self.next_order_id,
            "rebuilt orderbook from persisted resting orders"
        );
    }

    /// Get top N levels of the order book.
    #[allow(clippy::type_complexity)]
    pub fn depth(&self, levels: usize) -> (Vec<(FP8, FP8)>, Vec<(FP8, FP8)>) {
        let bids: Vec<(FP8, FP8)> = self
            .bids
            .iter()
            .rev()
            .take(levels)
            .map(|(&price, level)| (FP8(price), level.total_size()))
            .collect();

        let asks: Vec<(FP8, FP8)> = self
            .asks
            .iter()
            .take(levels)
            .map(|(&price, level)| (FP8(price), level.total_size()))
            .collect();

        (bids, asks)
    }

    // ── Internal ────────────────────────────────────────────────

    /// Check available liquidity for an order (for FOK pre-check).
    fn available_liquidity(&self, order: &Order) -> i64 {
        let opposite_book = match order.side {
            Side::Long => &self.asks,
            Side::Short => &self.bids,
        };
        let mut total = 0i64;
        for (&price_key, level) in opposite_book.iter() {
            let matches = match order.side {
                Side::Long => order.order_type == OrderType::Market || price_key <= order.price.0,
                Side::Short => order.order_type == OrderType::Market || price_key >= order.price.0,
            };
            if !matches {
                break;
            }
            for o in &level.orders {
                if o.user_id != order.user_id {
                    total += o.remaining().0;
                }
            }
        }
        total
    }

    fn add_to_book(&mut self, order: Order) {
        let book = match order.side {
            Side::Long => &mut self.bids,
            Side::Short => &mut self.asks,
        };
        book.entry(order.price.0)
            .or_insert_with(PriceLevel::new)
            .add(order);
    }

    fn match_order(&mut self, taker: &mut Order) -> (Vec<Trade>, Vec<(Order, i64)>) {
        let mut trades = Vec::new();
        // Self-trade prevention (Decrement-and-Cancel mode):
        // When a new taker order would cross with a resting maker from the
        // same user, both sides are decremented by the cross amount and NO
        // trade is generated. The (Order, i64) tuple is the maker order's
        // current state and the amount that was cancelled.
        let mut stp_cancelled: Vec<(Order, i64)> = Vec::new();

        let opposite_book = match taker.side {
            Side::Long => &mut self.asks,  // buyer matches against asks
            Side::Short => &mut self.bids, // seller matches against bids
        };

        // Collect matching price levels
        let matching_prices: Vec<i64> = match taker.side {
            Side::Long => {
                // Buy: match asks from lowest up to taker.price
                opposite_book
                    .keys()
                    .copied()
                    .take_while(|&ask_price| {
                        taker.order_type == OrderType::Market || ask_price <= taker.price.0
                    })
                    .collect()
            }
            Side::Short => {
                // Sell: match bids from highest down to taker.price
                opposite_book
                    .keys()
                    .rev()
                    .copied()
                    .take_while(|&bid_price| {
                        taker.order_type == OrderType::Market || bid_price >= taker.price.0
                    })
                    .collect()
            }
        };

        for price_key in matching_prices {
            if taker.remaining().0 <= 0 {
                break;
            }

            if let Some(level) = opposite_book.get_mut(&price_key) {
                let mut i = 0;
                while i < level.orders.len() && taker.remaining().0 > 0 {
                    let maker = &mut level.orders[i];

                    // Self-trade prevention: Decrement-and-Cancel
                    // Cancel ONLY the crossed amount on both sides (no trade).
                    if maker.user_id == taker.user_id {
                        let cross = std::cmp::min(taker.remaining().0, maker.remaining().0);
                        if cross <= 0 {
                            i += 1;
                            continue;
                        }
                        // Shrink taker by cross amount (size, not filled — no trade happened)
                        taker.size = FP8(taker.size.0 - cross);
                        // Bump maker.filled to "consume" cross from the resting order
                        maker.filled = FP8(maker.filled.0 + cross);

                        // Snapshot maker for caller notification
                        let snapshot = maker.clone();
                        if maker.filled.0 >= maker.size.0 {
                            // Fully consumed — remove from book, mark Cancelled
                            let mut removed = level.orders.remove(i);
                            removed.status = OrderStatus::Cancelled;
                            stp_cancelled.push((removed, cross));
                            // do not increment i (vec shifted)
                        } else {
                            // Partially consumed — stays in book with reduced remaining
                            stp_cancelled.push((snapshot, cross));
                            i += 1;
                        }
                        continue;
                    }

                    let fill_size = std::cmp::min(taker.remaining().0, maker.remaining().0);
                    let fill = FP8(fill_size);
                    let fill_price = FP8(price_key); // maker's price

                    // Update quantities
                    taker.filled = taker.filled + fill;
                    maker.filled = maker.filled + fill;

                    if maker.filled == maker.size {
                        maker.status = OrderStatus::Filled;
                    } else {
                        maker.status = OrderStatus::PartiallyFilled;
                    }

                    // Record trade
                    let trade_id = self.next_trade_id;
                    self.next_trade_id += 1;

                    let now_ms = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;

                    let trade = Trade {
                        trade_id,
                        market: self.market.clone(),
                        maker_order_id: maker.id,
                        taker_order_id: taker.id,
                        maker_user_id: maker.user_id.clone(),
                        taker_user_id: taker.user_id.clone(),
                        price: fill_price,
                        size: fill,
                        taker_side: taker.side,
                        timestamp_ms: now_ms,
                    };
                    trades.push(trade.clone());

                    // Add to recent trades
                    self.recent_trades.push(trade);
                    if self.recent_trades.len() > self.max_recent_trades {
                        self.recent_trades.remove(0);
                    }

                    // Remove fully filled maker
                    if maker.status == OrderStatus::Filled {
                        level.orders.remove(i);
                    } else {
                        i += 1;
                    }
                }
            }
        }

        // Clean empty levels
        opposite_book.retain(|_, l| !l.is_empty());

        (trades, stp_cancelled)
    }
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> OrderBook {
        OrderBook::new("XRP-USD-PERP")
    }

    #[test]
    fn limit_order_rests_on_book() {
        let mut ob = book();
        let (order, trades, _stp) = ob
            .submit_order(
                "alice".into(),
                Side::Long,
                OrderType::Limit,
                FP8::from_f64(0.55),
                FP8::from_f64(100.0),
                5,
                TimeInForce::Gtc,
                false,
                None,
            )
            .unwrap();

        assert!(trades.is_empty());
        assert_eq!(order.status, OrderStatus::Open);
        assert_eq!(ob.best_bid(), Some(FP8::from_f64(0.55)));
        assert_eq!(ob.best_ask(), None);
    }

    #[test]
    fn matching_produces_trade() {
        let mut ob = book();

        // Alice places buy at 0.55
        ob.submit_order(
            "alice".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.55),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();

        // Bob places sell at 0.55 → should match
        let (order, trades, _stp) = ob
            .submit_order(
                "bob".into(),
                Side::Short,
                OrderType::Limit,
                FP8::from_f64(0.55),
                FP8::from_f64(50.0),
                5,
                TimeInForce::Gtc,
                false,
                None,
            )
            .unwrap();

        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].size, FP8::from_f64(50.0));
        assert_eq!(trades[0].price, FP8::from_f64(0.55));
        assert_eq!(order.status, OrderStatus::Filled);

        // Alice's order partially filled, 50 remaining
        assert_eq!(ob.best_bid(), Some(FP8::from_f64(0.55)));
    }

    #[test]
    fn market_order_sweeps() {
        let mut ob = book();

        // Place 3 sell orders at different prices
        ob.submit_order(
            "s1".into(),
            Side::Short,
            OrderType::Limit,
            FP8::from_f64(0.55),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();
        ob.submit_order(
            "s2".into(),
            Side::Short,
            OrderType::Limit,
            FP8::from_f64(0.56),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();
        ob.submit_order(
            "s3".into(),
            Side::Short,
            OrderType::Limit,
            FP8::from_f64(0.57),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();

        // Market buy 250 → should fill 100@0.55 + 100@0.56 + 50@0.57
        let (order, trades, _stp) = ob
            .submit_order(
                "buyer".into(),
                Side::Long,
                OrderType::Market,
                FP8::ZERO,
                FP8::from_f64(250.0),
                5,
                TimeInForce::Ioc,
                false,
                None,
            )
            .unwrap();

        assert_eq!(trades.len(), 3);
        assert_eq!(trades[0].price, FP8::from_f64(0.55));
        assert_eq!(trades[0].size, FP8::from_f64(100.0));
        assert_eq!(trades[1].price, FP8::from_f64(0.56));
        assert_eq!(trades[2].price, FP8::from_f64(0.57));
        assert_eq!(trades[2].size, FP8::from_f64(50.0));
        assert_eq!(order.filled, FP8::from_f64(250.0));
    }

    #[test]
    fn cancel_order() {
        let mut ob = book();
        let (order, _, _) = ob
            .submit_order(
                "alice".into(),
                Side::Long,
                OrderType::Limit,
                FP8::from_f64(0.55),
                FP8::from_f64(100.0),
                5,
                TimeInForce::Gtc,
                false,
                None,
            )
            .unwrap();

        let cancelled = ob.cancel_order(order.id).unwrap();
        assert_eq!(cancelled.status, OrderStatus::Cancelled);
        assert_eq!(ob.best_bid(), None);
    }

    #[test]
    fn stp_decrement_and_cancel_partial() {
        // Tom's case: 1 buy resting + 1 sell crossing for 1.0 — both fully cancelled.
        let mut ob = book();

        ob.submit_order(
            "alice".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(1.3898),
            FP8::from_f64(1.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();

        let (taker, trades, stp) = ob
            .submit_order(
                "alice".into(),
                Side::Short,
                OrderType::Limit,
                FP8::from_f64(1.3890),
                FP8::from_f64(1.0),
                5,
                TimeInForce::Gtc,
                false,
                None,
            )
            .unwrap();

        // No actual trade should happen (STP)
        assert!(trades.is_empty());
        // Taker fully self-cancelled — should be marked Cancelled, size shrunk to 0
        assert_eq!(taker.size, FP8::ZERO);
        assert_eq!(taker.status, OrderStatus::Cancelled);
        // Maker fully consumed by STP — book should be empty
        assert_eq!(stp.len(), 1);
        assert_eq!(stp[0].0.status, OrderStatus::Cancelled);
        assert_eq!(stp[0].1, FP8::from_f64(1.0).0);
        assert_eq!(ob.best_bid(), None);
    }

    #[test]
    fn stp_decrement_and_cancel_only_crossed_amount() {
        // Tom's example: "1 buy and sell which crosses for 0.5 — buy should update to 0.5"
        let mut ob = book();

        // Alice rests 1.0 buy at 1.50
        ob.submit_order(
            "alice".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(1.50),
            FP8::from_f64(1.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();

        // Alice sends 0.5 sell at 1.50 — STP decrements her buy to 0.5
        let (taker, trades, stp) = ob
            .submit_order(
                "alice".into(),
                Side::Short,
                OrderType::Limit,
                FP8::from_f64(1.50),
                FP8::from_f64(0.5),
                5,
                TimeInForce::Gtc,
                false,
                None,
            )
            .unwrap();

        // No trades produced
        assert!(trades.is_empty());
        // Taker (the 0.5 sell) is fully self-cancelled
        assert_eq!(taker.size, FP8::ZERO);
        assert_eq!(taker.status, OrderStatus::Cancelled);
        // Maker survives with 0.5 remaining
        assert_eq!(stp.len(), 1);
        assert_eq!(stp[0].1, FP8::from_f64(0.5).0); // cancelled portion
        assert_eq!(ob.best_bid(), Some(FP8::from_f64(1.50)));

        // Confirm Alice's resting buy is now 0.5
        let alices = ob.user_orders("alice");
        assert_eq!(alices.len(), 1);
        assert_eq!(alices[0].remaining(), FP8::from_f64(0.5));
    }

    #[test]
    fn no_self_trade() {
        let mut ob = book();

        ob.submit_order(
            "alice".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.55),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();

        // Alice sells — should NOT match her own buy
        let (_, trades, _stp) = ob
            .submit_order(
                "alice".into(),
                Side::Short,
                OrderType::Limit,
                FP8::from_f64(0.55),
                FP8::from_f64(50.0),
                5,
                TimeInForce::Gtc,
                false,
                None,
            )
            .unwrap();

        assert!(trades.is_empty());
    }

    #[test]
    fn depth_snapshot() {
        let mut ob = book();
        ob.submit_order(
            "a".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.54),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();
        ob.submit_order(
            "b".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.55),
            FP8::from_f64(200.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();
        ob.submit_order(
            "c".into(),
            Side::Short,
            OrderType::Limit,
            FP8::from_f64(0.56),
            FP8::from_f64(150.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();

        let (bids, asks) = ob.depth(10);
        assert_eq!(bids.len(), 2);
        assert_eq!(bids[0].0, FP8::from_f64(0.55)); // highest bid first
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].0, FP8::from_f64(0.56));
    }

    #[test]
    fn price_time_priority() {
        let mut ob = book();
        // Two buys at same price — first should fill first
        ob.submit_order(
            "alice".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.55),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();
        ob.submit_order(
            "bob".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.55),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();

        // Sell 50 — should match Alice (first in queue)
        let (_, trades, _stp) = ob
            .submit_order(
                "charlie".into(),
                Side::Short,
                OrderType::Limit,
                FP8::from_f64(0.55),
                FP8::from_f64(50.0),
                5,
                TimeInForce::Gtc,
                false,
                None,
            )
            .unwrap();

        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].maker_user_id, "alice");
    }

    #[test]
    fn partial_fill_tracking() {
        let mut ob = book();
        ob.submit_order(
            "alice".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.55),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();

        // Partial fill: sell 30
        let (_, trades, _stp) = ob
            .submit_order(
                "bob".into(),
                Side::Short,
                OrderType::Limit,
                FP8::from_f64(0.55),
                FP8::from_f64(30.0),
                5,
                TimeInForce::Gtc,
                false,
                None,
            )
            .unwrap();
        assert_eq!(trades.len(), 1);

        // Alice's remaining: 70
        let orders = ob.user_orders("alice");
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].remaining(), FP8::from_f64(70.0));
        assert_eq!(orders[0].status, OrderStatus::PartiallyFilled);
    }

    #[test]
    fn cancel_all_for_user() {
        let mut ob = book();
        ob.submit_order(
            "alice".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.54),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();
        ob.submit_order(
            "alice".into(),
            Side::Short,
            OrderType::Limit,
            FP8::from_f64(0.56),
            FP8::from_f64(50.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();
        ob.submit_order(
            "bob".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.53),
            FP8::from_f64(200.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();

        let cancelled = ob.cancel_all("alice");
        assert_eq!(cancelled.len(), 2);
        assert_eq!(ob.user_orders("alice").len(), 0);
        assert_eq!(ob.user_orders("bob").len(), 1); // Bob's order untouched
    }

    #[test]
    fn cancel_nonexistent_order_fails() {
        let mut ob = book();
        assert!(ob.cancel_order(999).is_err());
    }

    #[test]
    fn best_bid_ask_mid() {
        let mut ob = book();
        assert_eq!(ob.best_bid(), None);
        assert_eq!(ob.best_ask(), None);
        assert_eq!(ob.mid_price(), None);

        ob.submit_order(
            "a".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.54),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();
        ob.submit_order(
            "b".into(),
            Side::Short,
            OrderType::Limit,
            FP8::from_f64(0.56),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();

        assert_eq!(ob.best_bid(), Some(FP8::from_f64(0.54)));
        assert_eq!(ob.best_ask(), Some(FP8::from_f64(0.56)));
        assert_eq!(ob.mid_price(), Some(FP8::from_f64(0.55)));
    }

    #[test]
    fn trade_ids_increment() {
        let mut ob = book();
        ob.submit_order(
            "alice".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.55),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();
        ob.submit_order(
            "bob".into(),
            Side::Long,
            OrderType::Limit,
            FP8::from_f64(0.56),
            FP8::from_f64(100.0),
            5,
            TimeInForce::Gtc,
            false,
            None,
        )
        .unwrap();

        let (_, trades1, _stp) = ob
            .submit_order(
                "charlie".into(),
                Side::Short,
                OrderType::Limit,
                FP8::from_f64(0.55),
                FP8::from_f64(200.0),
                5,
                TimeInForce::Gtc,
                false,
                None,
            )
            .unwrap();

        // Should produce 2 trades (match bob@0.56 first, then alice@0.55)
        assert_eq!(trades1.len(), 2);
        assert!(
            trades1[0].trade_id < trades1[1].trade_id,
            "trade IDs should increment"
        );
    }

    #[test]
    fn order_ids_increment() {
        let mut ob = book();
        let (o1, _, _) = ob
            .submit_order(
                "a".into(),
                Side::Long,
                OrderType::Limit,
                FP8::from_f64(0.55),
                FP8::from_f64(100.0),
                5,
                TimeInForce::Gtc,
                false,
                None,
            )
            .unwrap();
        let (o2, _, _) = ob
            .submit_order(
                "b".into(),
                Side::Short,
                OrderType::Limit,
                FP8::from_f64(0.56),
                FP8::from_f64(100.0),
                5,
                TimeInForce::Gtc,
                false,
                None,
            )
            .unwrap();
        assert!(o2.id > o1.id, "order IDs should increment");
    }

    // ── O-M4 STP rate-limit ───────────────────────────────────

    /// Helper: submit a limit order, ignoring all return fields.
    #[allow(clippy::type_complexity)]
    fn place(
        ob: &mut OrderBook,
        user: &str,
        side: Side,
        price: f64,
        size: f64,
    ) -> Result<(Order, Vec<Trade>, Vec<(Order, i64)>)> {
        ob.submit_order(
            user.into(),
            side,
            OrderType::Limit,
            FP8::from_f64(price),
            FP8::from_f64(size),
            1,
            TimeInForce::Gtc,
            false,
            None,
        )
    }

    #[test]
    fn stp_event_records_against_user_budget() {
        // Alice places resting bid → resting ask of her own at a price
        // that crosses the bid → STP fires once.
        let mut ob = book();
        place(&mut ob, "alice", Side::Long, 1.00, 10.0).unwrap();
        let (_, _, stp) = place(&mut ob, "alice", Side::Short, 1.00, 5.0).unwrap();
        assert_eq!(stp.len(), 1, "exactly one self-cross expected");
        // Budget is 30/min — 1 used, plenty of room.
        assert!(ob.stp_rate.peek("alice"));
    }

    #[test]
    fn stp_rate_limit_rejects_31st_self_crossing_order() {
        // Drive 30 self-crosses; the 31st must be rejected at the gate
        // before match_order is called (i.e. submit_order returns Err).
        let mut ob = book();
        // Place a single resting buy big enough to be hit 30 times.
        place(&mut ob, "alice", Side::Long, 1.00, 1000.0).unwrap();
        for i in 0..30 {
            let r = place(&mut ob, "alice", Side::Short, 1.00, 1.0);
            assert!(r.is_ok(), "self-cross #{i} should be admitted");
        }
        let r = place(&mut ob, "alice", Side::Short, 1.00, 1.0);
        assert!(r.is_err(), "31st order should be rejected");
        let msg = format!("{}", r.err().unwrap());
        assert!(msg.contains("self-trade rate exceeded"), "wrong msg: {msg}");
    }

    #[test]
    fn non_self_crossing_orders_do_not_consume_stp_budget() {
        // Alice's orders never cross her own → budget untouched even
        // after many submissions. This pins that we record only on
        // STP fire, not on every order.
        let mut ob = book();
        place(&mut ob, "bob", Side::Long, 0.99, 100.0).unwrap();
        for _ in 0..100 {
            // Alice places a resting ask above the bid → never crosses bob.
            place(&mut ob, "alice", Side::Short, 1.10, 1.0).unwrap();
        }
        // Alice's STP budget is fully intact.
        assert!(ob.stp_rate.peek("alice"));
    }

    #[test]
    fn stp_rate_limit_is_per_user() {
        // Alice exhausts her budget → Bob is unaffected.
        let mut ob = book();
        place(&mut ob, "alice", Side::Long, 1.00, 1000.0).unwrap();
        for _ in 0..30 {
            place(&mut ob, "alice", Side::Short, 1.00, 1.0).unwrap();
        }
        assert!(place(&mut ob, "alice", Side::Short, 1.00, 1.0).is_err());

        // Bob can still submit even when crossing alice's resting bid
        // (cross-user fill, no STP).
        let r = place(&mut ob, "bob", Side::Short, 1.00, 1.0);
        assert!(r.is_ok(), "bob's order must not be rate-limited");
    }
}
