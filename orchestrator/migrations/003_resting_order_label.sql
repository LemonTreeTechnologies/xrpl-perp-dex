-- V1 vault: add `label` column to resting_orders for categorical tagging
-- (e.g. "vAMM") used by rebate-eligibility filtering.
-- Nullable; clients may omit. Safe to re-run on already-migrated DB.

BEGIN;

ALTER TABLE resting_orders
    ADD COLUMN IF NOT EXISTS label VARCHAR(64);

COMMIT;
