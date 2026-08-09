-- REQ-β3.2c-impl (D-1 PERSIST, RESP-β3.2c): current_membership_bundle.
--
-- The bundle a `source` node serves to a `newcomer` (X-β3.2-3) is the
-- (statement, quorum_bundle) that authorised the CURRENT epoch. The enclave
-- sealed only the RESULT of that authorisation, not the attestation itself
-- (`ecall_bootstrap_from_quorum_attestation` re-verifies a SUPPLIED bundle),
-- so the tuple must be retained OUTSIDE the enclave. In-memory retention is
-- empty after an orchestrator restart, so a restarted in-sync node could not
-- serve a join until it witnessed a fresh membership-change in-process — not a
-- real capability. This table makes any in-sync node able to serve, always.
--
-- Single row (id=1, CHECK-pinned): the cluster has exactly one current
-- membership tuple; each successful `Seal`/`Bootstrap` apply REPLACES it
-- (ON CONFLICT (id) DO UPDATE). This is a replica of enclave-verified state,
-- never a source of truth: a `newcomer` re-verifies the M-1 quorum sigs + the
-- escrow-bound message hash INSIDE its enclave on import, so a tampered row
-- can at worst make an export fail, never forge a membership.
--
-- authority_* = the set the current epoch installed (M, the enclave's sealed
-- set). attesting_* = the OUTGOING (M-1) set whose quorum signed the M
-- transition (self == authority at genesis). signers are packed as the SAME
-- wire the enclave/Deliver use: count * 24 bytes (account_id[20] || weight_be[4]).

BEGIN;

CREATE TABLE IF NOT EXISTS current_membership_bundle (
    -- singleton guard: exactly one row, always id=1
    id INTEGER PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    escrow_hex VARCHAR(40) NOT NULL,
    authority_epoch BIGINT NOT NULL,
    confirmed_epoch BIGINT NOT NULL,
    prev_epoch_hash_hex VARCHAR(64) NOT NULL,
    authority_signers_hex TEXT NOT NULL,
    authority_signer_count INTEGER NOT NULL,
    authority_quorum INTEGER NOT NULL,
    attesting_signers_hex TEXT NOT NULL,
    attesting_signer_count INTEGER NOT NULL,
    attesting_quorum INTEGER NOT NULL,
    quorum_bundle_hex TEXT NOT NULL,
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

COMMIT;
