-- REQ-20-impl R2 commit 3: deposit_bindings mirror table.
--
-- The enclave is the source of truth for (sender_addr, dest_tag) →
-- user_id mappings. This table is a replica written AFTER the enclave
-- returns DB_OK from ecall_perp_register_deposit_binding, used for:
--   - Operator-side queries / dashboards
--   - Replication / audit cross-check (enclave vs DB divergence flags
--     state corruption)
--   - Post-restart hot-state recovery doesn't depend on this table —
--     the enclave's sealed state is canonical
--
-- Primary key (sender_addr, dest_tag) mirrors the enclave's binding
-- map key. user_id is the bound credit target. bound_via_probe_tx_hash
-- preserves the audit trail (which probe deposit caused this binding).
--
-- ON CONFLICT DO NOTHING on insert handles DB_IDEMPOTENT_OK and
-- replication race scenarios — never overwrite an existing binding
-- (FCFS semantics match the enclave).

CREATE TABLE IF NOT EXISTS deposit_bindings (
    user_id VARCHAR(36) NOT NULL,
    sender_addr VARCHAR(36) NOT NULL,
    dest_tag BIGINT NOT NULL,
    bound_at_ms BIGINT NOT NULL,
    bound_via_probe_tx_hash VARCHAR(64) NOT NULL,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    PRIMARY KEY (sender_addr, dest_tag)
);

CREATE INDEX IF NOT EXISTS deposit_bindings_user_id_idx
    ON deposit_bindings (user_id);
