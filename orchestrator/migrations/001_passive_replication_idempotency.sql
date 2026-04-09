-- Migration: passive replication idempotency keys
--
-- Adds UNIQUE constraints on (trade_id, market) for `trades` and on
-- `position_id` for `liquidations` so that ON CONFLICT DO NOTHING can make
-- duplicate inserts a no-op when every operator replays the same events.
--
-- Pre-migration: deduplicate any existing duplicate rows (shouldn't exist
-- on the current live sequencer but the migration must be safe to run
-- anyway). We keep the row with the lowest `id` (oldest insertion).
--
-- Usage:  psql -d perp_dex -f migrations/001_passive_replication_idempotency.sql

BEGIN;

-- ── trades ──────────────────────────────────────────────────────
-- Deduplicate any existing rows that share (trade_id, market).
DELETE FROM trades t
USING trades t2
WHERE t.trade_id = t2.trade_id
  AND t.market = t2.market
  AND t.id > t2.id;

ALTER TABLE trades
    DROP CONSTRAINT IF EXISTS trades_trade_id_market_key;

ALTER TABLE trades
    ADD CONSTRAINT trades_trade_id_market_key UNIQUE (trade_id, market);

-- ── liquidations ────────────────────────────────────────────────
-- Deduplicate any existing rows that share position_id.
DELETE FROM liquidations l
USING liquidations l2
WHERE l.position_id = l2.position_id
  AND l.id > l2.id;

ALTER TABLE liquidations
    DROP CONSTRAINT IF EXISTS liquidations_position_id_key;

ALTER TABLE liquidations
    ADD CONSTRAINT liquidations_position_id_key UNIQUE (position_id);

COMMIT;
