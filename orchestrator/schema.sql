-- PostgreSQL schema for Perp DEX trade history
-- Usage: psql -d perp_dex -f schema.sql

CREATE TABLE IF NOT EXISTS trades (
    id BIGSERIAL PRIMARY KEY,
    trade_id BIGINT NOT NULL,
    market VARCHAR(32) NOT NULL DEFAULT 'XRP-RLUSD-PERP',
    maker_order_id BIGINT NOT NULL,
    taker_order_id BIGINT NOT NULL,
    maker_user_id VARCHAR(36) NOT NULL,
    taker_user_id VARCHAR(36) NOT NULL,
    price BIGINT NOT NULL,
    size BIGINT NOT NULL,
    taker_side VARCHAR(8) NOT NULL,
    timestamp_ms BIGINT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    -- (trade_id, market) is the idempotency key for passive replication
    -- across operators: validators replay the same batches from the
    -- sequencer and insert the same rows, the UNIQUE constraint lets
    -- ON CONFLICT DO NOTHING make duplicates a no-op.
    UNIQUE (trade_id, market)
);
CREATE INDEX IF NOT EXISTS idx_trades_maker ON trades(maker_user_id, timestamp_ms DESC);
CREATE INDEX IF NOT EXISTS idx_trades_taker ON trades(taker_user_id, timestamp_ms DESC);
CREATE INDEX IF NOT EXISTS idx_trades_market ON trades(market, timestamp_ms DESC);

CREATE TABLE IF NOT EXISTS funding_payments (
    id BIGSERIAL PRIMARY KEY,
    user_id VARCHAR(36) NOT NULL,
    position_id BIGINT NOT NULL,
    side VARCHAR(8) NOT NULL,
    payment BIGINT NOT NULL,
    funding_rate BIGINT NOT NULL,
    mark_price BIGINT NOT NULL,
    timestamp_epoch BIGINT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_funding_user ON funding_payments(user_id, timestamp_epoch DESC);

CREATE TABLE IF NOT EXISTS deposits (
    id BIGSERIAL PRIMARY KEY,
    user_id VARCHAR(36) NOT NULL,
    amount BIGINT NOT NULL,
    currency VARCHAR(8) NOT NULL DEFAULT 'RLUSD',
    xrpl_tx_hash VARCHAR(64) NOT NULL UNIQUE,
    ledger_index BIGINT,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_deposits_user ON deposits(user_id, created_at DESC);

CREATE TABLE IF NOT EXISTS withdrawals (
    id BIGSERIAL PRIMARY KEY,
    user_id VARCHAR(36) NOT NULL,
    amount BIGINT NOT NULL,
    destination VARCHAR(36) NOT NULL,
    status VARCHAR(32) NOT NULL,
    xrpl_tx_hash VARCHAR(64),
    message TEXT,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_withdrawals_user ON withdrawals(user_id, created_at DESC);

CREATE TABLE IF NOT EXISTS liquidations (
    id BIGSERIAL PRIMARY KEY,
    position_id BIGINT NOT NULL,
    user_id VARCHAR(36) NOT NULL,
    close_price BIGINT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    -- Each position can only be liquidated once. All operators run the
    -- liquidation scan independently against their local enclave, so
    -- without this UNIQUE constraint every operator would insert the
    -- same liquidation row on its own PG, producing duplicates.
    UNIQUE (position_id)
);
CREATE INDEX IF NOT EXISTS idx_liquidations_user ON liquidations(user_id, created_at DESC);

-- Aggregate funding events. One row per funding application. Per-position
-- breakdown requires enclave changes (post-hackathon) — see funding_payments
-- table which is currently unpopulated.
CREATE TABLE IF NOT EXISTS funding_events (
    id BIGSERIAL PRIMARY KEY,
    funding_rate BIGINT NOT NULL,
    mark_price BIGINT NOT NULL,
    index_price BIGINT NOT NULL,
    timestamp_epoch BIGINT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE (timestamp_epoch)
);
CREATE INDEX IF NOT EXISTS idx_funding_events_ts ON funding_events(timestamp_epoch DESC);

-- Resting limit orders currently on the in-memory CLOB. Persisted so that
-- a new sequencer can rebuild the book from PG on failover (C5.1).
-- O-H4: every row carries the submitter's XRPL signature binding so the
-- post-failover reload can re-verify authenticity before replaying into
-- the in-memory CLOB. See migrations/002_resting_order_signatures.sql.
CREATE TABLE IF NOT EXISTS resting_orders (
    order_id BIGINT PRIMARY KEY,
    user_id VARCHAR(36) NOT NULL,
    market VARCHAR(32) NOT NULL DEFAULT 'XRP-RLUSD-PERP',
    side VARCHAR(8) NOT NULL,
    price BIGINT NOT NULL,
    size BIGINT NOT NULL,
    filled BIGINT NOT NULL DEFAULT 0,
    leverage INT NOT NULL DEFAULT 1,
    reduce_only BOOLEAN NOT NULL DEFAULT FALSE,
    timestamp_ms BIGINT NOT NULL,
    client_order_id VARCHAR(64),
    signed_body_hex TEXT NOT NULL,
    signature_hex TEXT NOT NULL,
    signer_timestamp TEXT NOT NULL,
    signer_address VARCHAR(36) NOT NULL,
    signer_pubkey_hex VARCHAR(66) NOT NULL,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS price_candles (
    id BIGSERIAL PRIMARY KEY,
    market VARCHAR(32) NOT NULL DEFAULT 'XRP-RLUSD-PERP',
    open_price BIGINT NOT NULL,
    high_price BIGINT NOT NULL,
    low_price BIGINT NOT NULL,
    close_price BIGINT NOT NULL,
    volume BIGINT NOT NULL DEFAULT 0,
    timestamp_epoch BIGINT NOT NULL,
    UNIQUE(market, timestamp_epoch)
);
CREATE INDEX IF NOT EXISTS idx_candles_market ON price_candles(market, timestamp_epoch DESC);
