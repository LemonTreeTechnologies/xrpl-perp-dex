//! REQ-β4.2 X-β4.2-2/3 — Path A pre-flight capacity-check (the STATIC half of
//! the two-layer operator-safety gate, `feedback_rehearse_before_irreversible`).
//!
//! Reads the actual sealed state on a node and compares it, BEFORE any «go»,
//! against every capacity limit the migration depends on: the export ciphertext
//! buffer (bytes), the enclave heap (bytes, PEAK not payload — X-β4.2-3), and
//! the file-count ceilings (manifest + per-shard — X-β4.2-2). Any breach is a
//! hard STOP. This is the check that, had it existed, would have printed
//! "10 MB state > 4 MB export buffer → CANNOT MIGRATE" before the 2026-07-27
//! migration was ever authorized, instead of the operator learning it by the
//! export failing at the destructive step.
//!
//! Thresholds are FIXED invariants, NOT operator-tunable (RESP-β4.2 Q-β4.2-4):
//! an operator who could lower a margin "to make it fit" reconstructs the very
//! blind-authorization failure this gate exists to prevent.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

/// Capacity limits, MIRRORED from `EthSignerEnclave/Enclave/PathALimits.h`
/// (`PATH_A_EXPORT_CIPHER_BUF_SIZE`, `PATH_A_MANIFEST_MAX_FILES`,
/// `PATH_A_ENCLAVE_HEAP_MAX_BYTES`) and `EnclaveLimits.h`
/// (`ENCLAVE_LIMITS_PERP_STATE_MAX_FILES_PER_SHARD`).
///
/// The AUTHORITATIVE guard is the C++ `static_assert` in PathALimits.h that ties
/// the buffer/manifest to the caps at compile time; these constants mirror it for
/// the operator-facing pre-flight. Keep in lockstep (a future `build.rs` could
/// parse the header). Conservative by construction: a mirror that drifts LOW only
/// makes this gate STOP *earlier* (safe); a mirror drifting HIGH is caught by the
/// dynamic `--dry-run` rehearsal (the other half of the gate) and by the export's
/// own runtime buffer check.
pub mod limits {
    /// PATH_A_EXPORT_CIPHER_BUF_SIZE — host allocates this for the ciphertext.
    pub const EXPORT_CIPHER_BUF_BYTES: u64 = 16 * 1024 * 1024;
    /// Enclave.config.xml HeapMaxSize (0x4000000).
    pub const ENCLAVE_HEAP_MAX_BYTES: u64 = 64 * 1024 * 1024;
    /// PATH_A_MANIFEST_MAX_FILES — one manifest entry per resealed file.
    pub const MANIFEST_MAX_FILES: u64 = 512;
    /// ENCLAVE_LIMITS_PERP_STATE_MAX_FILES_PER_SHARD.
    pub const PERP_STATE_MAX_FILES_PER_SHARD: u64 = 200;
    /// Export holds plaintexts + composite + payload at once ≈ 2.5–3× payload
    /// (X-β4.2-3: the heap axis fails at the PEAK, not the flat payload — the
    /// observed -99 was heap OOM before the buffer check).
    pub const HEAP_PEAK_FACTOR: u64 = 3;
    /// Approx per-file SGX seal overhead (sgx_sealed_data_t header+MAC), used to
    /// back out a plaintext estimate from on-disk sealed sizes.
    pub const SEAL_OVERHEAD_BYTES: u64 = 592;
}

/// One limit check with the numbers behind the verdict.
#[derive(Debug, Clone)]
pub struct CapacityCheck {
    pub axis: &'static str,
    pub usage: u64,
    pub limit: u64,
    pub unit: &'static str,
    pub ok: bool,
}

impl CapacityCheck {
    fn new(axis: &'static str, usage: u64, limit: u64, unit: &'static str) -> Self {
        Self {
            axis,
            usage,
            limit,
            ok: usage <= limit,
            unit,
        }
    }
    /// margin = limit/usage (how much headroom); <1.0 means breach.
    pub fn margin(&self) -> f64 {
        if self.usage == 0 {
            f64::INFINITY
        } else {
            self.limit as f64 / self.usage as f64
        }
    }
}

/// Assessment of a node's sealed state against the migration capacity limits.
#[derive(Debug, Clone)]
pub struct CapacityReport {
    pub total_sealed_bytes: u64,
    pub total_files: u64,
    /// Conservative plaintext estimate = sealed − files×overhead (the export
    /// buffer holds plaintext; using the on-disk sealed size as an upper bound
    /// if the subtraction underflows).
    pub est_payload_bytes: u64,
    pub est_heap_peak_bytes: u64,
    /// shard_id → number of perp-state sealed files in that shard.
    pub perp_files_by_shard: BTreeMap<u32, u64>,
    pub checks: Vec<CapacityCheck>,
}

impl CapacityReport {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
}

/// Classify a sealed filename to the shard it belongs to (perp-state files only).
/// `s0_perp_bindings_40.sealed` → Some(0). Non-perp files → None.
fn perp_shard_of(name: &str) -> Option<u32> {
    let rest = name.strip_prefix('s')?;
    if !name.contains("_perp_") {
        return None;
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().ok()
}

/// Walk `accounts_dir`, tally sealed state, and run every capacity check.
/// Read-only; touches nothing.
pub fn assess_accounts_dir(accounts_dir: &Path) -> Result<CapacityReport> {
    let mut total_sealed_bytes: u64 = 0;
    let mut total_files: u64 = 0;
    let mut perp_files_by_shard: BTreeMap<u32, u64> = BTreeMap::new();

    let entries = std::fs::read_dir(accounts_dir)
        .with_context(|| format!("read accounts dir {}", accounts_dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // Count every sealed migration input: *.sealed section files plus the
        // account-pool *.account files (the migration reseals each).
        if !(name.ends_with(".sealed") || name.ends_with(".account")) {
            continue;
        }
        total_files += 1;
        total_sealed_bytes += meta.len();
        if let Some(shard) = perp_shard_of(&name) {
            *perp_files_by_shard.entry(shard).or_insert(0) += 1;
        }
    }

    let overhead = total_files.saturating_mul(limits::SEAL_OVERHEAD_BYTES);
    // Conservative: never claim the plaintext is *smaller* than a floor of 0,
    // and if overhead somehow exceeds sealed bytes, fall back to sealed bytes.
    let est_payload_bytes = total_sealed_bytes.saturating_sub(overhead).max(
        // guard: if subtraction underflowed to 0 on a real (non-empty) state,
        // use sealed bytes as the conservative upper bound.
        if total_sealed_bytes > 0 && total_sealed_bytes <= overhead {
            total_sealed_bytes
        } else {
            0
        },
    );
    let est_heap_peak_bytes = est_payload_bytes.saturating_mul(limits::HEAP_PEAK_FACTOR);

    let max_perp_per_shard = perp_files_by_shard.values().copied().max().unwrap_or(0);

    let checks = vec![
        CapacityCheck::new(
            "export ciphertext buffer (payload bytes)",
            est_payload_bytes,
            limits::EXPORT_CIPHER_BUF_BYTES,
            "B",
        ),
        CapacityCheck::new(
            "enclave heap (export PEAK ≈ 3× payload)",
            est_heap_peak_bytes,
            limits::ENCLAVE_HEAP_MAX_BYTES,
            "B",
        ),
        CapacityCheck::new(
            "migration manifest (total resealed files)",
            total_files,
            limits::MANIFEST_MAX_FILES,
            "files",
        ),
        CapacityCheck::new(
            "perp-state files per shard (max shard)",
            max_perp_per_shard,
            limits::PERP_STATE_MAX_FILES_PER_SHARD,
            "files",
        ),
    ];

    Ok(CapacityReport {
        total_sealed_bytes,
        total_files,
        est_payload_bytes,
        est_heap_peak_bytes,
        perp_files_by_shard,
        checks,
    })
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Human-readable table for the operator. Breaching rows are marked FAIL.
pub fn render(report: &CapacityReport) -> String {
    let mut out = String::new();
    out.push_str("Path A migration — pre-flight capacity check (REQ-β4.2)\n");
    out.push_str(&format!(
        "  sealed state: {} files, {:.2} MiB on disk (≈ {:.2} MiB payload, \
         {:.2} MiB export peak)\n",
        report.total_files,
        mib(report.total_sealed_bytes),
        mib(report.est_payload_bytes),
        mib(report.est_heap_peak_bytes),
    ));
    if !report.perp_files_by_shard.is_empty() {
        let shards: Vec<String> = report
            .perp_files_by_shard
            .iter()
            .map(|(s, n)| format!("shard {s}: {n}"))
            .collect();
        out.push_str(&format!(
            "  perp-state files by shard: {}\n",
            shards.join(", ")
        ));
    }
    out.push_str("  ┌─ checks ────────────────────────────────────────────────────┐\n");
    for c in &report.checks {
        let (usage_s, limit_s) = if c.unit == "B" {
            (
                format!("{:.2} MiB", mib(c.usage)),
                format!("{:.0} MiB", mib(c.limit)),
            )
        } else {
            (
                format!("{} {}", c.usage, c.unit),
                format!("{} {}", c.limit, c.unit),
            )
        };
        out.push_str(&format!(
            "  {:<4} {:<44} {} / {}  (margin {:.2}×)\n",
            if c.ok { "OK" } else { "FAIL" },
            c.axis,
            usage_s,
            limit_s,
            c.margin(),
        ));
    }
    out.push_str("  └─────────────────────────────────────────────────────────────┘\n");
    if report.ok() {
        out.push_str("  VERDICT: capacity OK — safe to proceed to the dry-run rehearsal.\n");
    } else {
        out.push_str(
            "  VERDICT: STOP — state exceeds a migration capacity limit. This state \
             CANNOT be Path-A-migrated by this enclave. Do NOT start the ceremony.\n",
        );
    }
    out
}

/// Assess + render + gate. Returns Err (for a non-zero exit) if any limit is
/// breached — the migration must NOT proceed.
pub fn assess_and_gate(accounts_dir: &Path) -> Result<CapacityReport> {
    let report = assess_accounts_dir(accounts_dir)?;
    print!("{}", render(&report));
    if !report.ok() {
        anyhow::bail!(
            "pre-flight capacity check FAILED — {} of {} limits breached; migration blocked",
            report.checks.iter().filter(|c| !c.ok).count(),
            report.checks.len(),
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &Path, name: &str, bytes: usize) {
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        f.write_all(&vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn perp_shard_parsing() {
        assert_eq!(perp_shard_of("s0_perp_bindings_40.sealed"), Some(0));
        assert_eq!(perp_shard_of("s3_perp_users_1.sealed"), Some(3));
        assert_eq!(perp_shard_of("s0_perp_meta.sealed"), Some(0));
        assert_eq!(perp_shard_of("signer_list.sealed"), None);
        assert_eq!(perp_shard_of("0xabc.account"), None);
    }

    #[test]
    fn small_state_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write_file(d, "signer_list.sealed", 2256);
        write_file(d, "ecdh_identity.sealed", 680);
        write_file(d, "0xabc.account", 1200);
        for i in 0..5 {
            write_file(d, &format!("s0_perp_users_{i}.sealed"), 20560);
        }
        let r = assess_accounts_dir(d).unwrap();
        assert!(r.ok(), "small state should pass: {}", render(&r));
        assert_eq!(r.total_files, 8);
        assert_eq!(r.perp_files_by_shard.get(&0), Some(&5));
    }

    #[test]
    fn the_2026_07_27_state_would_stop_on_file_count() {
        // Reproduce the live failure shape: 179 files, ~10 MB. Even under the
        // NEW 16 MB buffer / 64 MB heap this must be caught — the file count
        // (179) is fine under 512, but per-shard perp count is the axis to
        // watch. Here we push perp-per-shard OVER 200 to prove the STOP fires.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write_file(d, "signer_list.sealed", 2256);
        for i in 0..201 {
            write_file(d, &format!("s0_perp_bindings_{i}.sealed"), 60560);
        }
        let r = assess_accounts_dir(d).unwrap();
        assert!(!r.ok(), "over-cap per-shard state must STOP");
        let per_shard = r
            .checks
            .iter()
            .find(|c| c.axis.contains("per shard"))
            .unwrap();
        assert!(!per_shard.ok);
        assert_eq!(per_shard.usage, 201);
    }

    #[test]
    fn oversize_bytes_stops_on_buffer_and_heap() {
        // A single shard with many full chunks → payload > 16 MB buffer.
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        // 300 files × 80 KB ≈ 24 MB sealed → payload > 16 MB, peak > 64 MB.
        // Spread across shards so per-shard stays under 200.
        for shard in 0..2u32 {
            for i in 0..150 {
                write_file(d, &format!("s{shard}_perp_users_{i}.sealed"), 81920);
            }
        }
        let r = assess_accounts_dir(d).unwrap();
        assert!(!r.ok());
        let buf = r.checks.iter().find(|c| c.axis.contains("buffer")).unwrap();
        assert!(!buf.ok, "payload should exceed the 16 MiB buffer");
        let heap = r.checks.iter().find(|c| c.axis.contains("heap")).unwrap();
        assert!(!heap.ok, "peak should exceed the 64 MiB heap");
    }
}
