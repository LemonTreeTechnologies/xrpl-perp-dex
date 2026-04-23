-- O-H4: resting orders reloaded from PG without signature binding.
--
-- Before this migration, `resting_orders` trusted every row on failover
-- reload — a compromised PG could inject forged orders with arbitrary
-- user_ids. This migration pins each row to a re-verifiable XRPL
-- signature binding (body, signature, timestamp, address, pubkey) so
-- the reload path can reject rows that don't validate.
--
-- Resting orders are short-lived (cancelled on restart today, persisted
-- on hot-failover). The audit explicitly approved dropping legacy rows
-- on upgrade rather than backfilling synthetic bindings.
--
-- Safe to re-run on an already-migrated DB.

BEGIN;

-- Drop legacy rows that predate the binding (they cannot be verified).
DELETE FROM resting_orders
    WHERE NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'resting_orders'
          AND column_name = 'signed_body_hex'
    );

ALTER TABLE resting_orders
    ADD COLUMN IF NOT EXISTS signed_body_hex TEXT,
    ADD COLUMN IF NOT EXISTS signature_hex TEXT,
    ADD COLUMN IF NOT EXISTS signer_timestamp TEXT,
    ADD COLUMN IF NOT EXISTS signer_address VARCHAR(36),
    ADD COLUMN IF NOT EXISTS signer_pubkey_hex VARCHAR(66);

-- Any row left without a binding at this point is unverifiable — drop it.
DELETE FROM resting_orders
    WHERE signed_body_hex IS NULL
       OR signature_hex   IS NULL
       OR signer_timestamp IS NULL
       OR signer_address   IS NULL
       OR signer_pubkey_hex IS NULL;

ALTER TABLE resting_orders
    ALTER COLUMN signed_body_hex SET NOT NULL,
    ALTER COLUMN signature_hex   SET NOT NULL,
    ALTER COLUMN signer_timestamp SET NOT NULL,
    ALTER COLUMN signer_address   SET NOT NULL,
    ALTER COLUMN signer_pubkey_hex SET NOT NULL;

COMMIT;
