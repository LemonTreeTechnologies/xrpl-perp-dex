//! Shard router — routes user requests to the correct shard's enclave.
//!
//! Phase 1: single shard (shard_id=0), one enclave URL.
//! The router abstraction exists from day one so that adding shards
//! is a config change, not a code rewrite.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::info;

use crate::perp_client::PerpClient;

#[derive(Debug, Deserialize)]
pub struct ShardsConfig {
    pub shards: Vec<ShardEntry>,
}

#[derive(Debug, Deserialize)]
pub struct ShardEntry {
    pub shard_id: u32,
    pub enclave_url: String,
    /// Path A: 32-byte FROST group_id (hex, no 0x). Set after DKG finalize.
    /// When present → Path A announcer broadcasts a DCAP quote bound to
    /// `(ecdh_pubkey, shard_id, group_id)` every ~4 min. When absent →
    /// announcer is dormant for this shard (pre-DKG PoC state).
    #[serde(default)]
    pub frost_group_id: Option<String>,
}

/// A shard configured with a FROST group_id — ready for Path A announcements.
#[derive(Debug, Clone)]
pub struct PathAGroup {
    pub shard_id: u32,
    pub group_id_hex: String,
    pub enclave_url: String,
}

pub struct ShardRouter {
    shards: HashMap<u32, PerpClient>,
    shard_count: u32,
    sorted_ids: Vec<u32>,
    path_a_groups: Vec<PathAGroup>,
}

impl ShardRouter {
    /// Build from a shards.toml config file.
    pub async fn from_config(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: ShardsConfig = toml::from_str(&data).context("invalid shards.toml")?;

        let mut shards = HashMap::new();
        let mut sorted_ids = Vec::new();
        let mut path_a_groups = Vec::new();
        for entry in &config.shards {
            let client = PerpClient::new(&entry.enclave_url)?;
            // O-I3 (audit): if `set_shard_id` is not understood by the
            // remote enclave, the orchestrator continues but the enclave
            // is effectively running with `shard_id = 0` regardless of
            // config. Operators must alert on `audit.shard_id_unsupported
            // = true` in journalctl for multi-shard deployments. Single-
            // shard deploys are unaffected.
            if let Err(e) = client.set_shard_id(entry.shard_id).await {
                tracing::warn!(
                    target: "audit",
                    shard_id_unsupported = true,
                    shard_id = entry.shard_id,
                    url = %entry.enclave_url,
                    "set_shard_id not supported by enclave (running as shard 0): {}",
                    e
                );
            }
            info!(shard_id = entry.shard_id, url = %entry.enclave_url, "shard registered");
            if let Some(gid) = &entry.frost_group_id {
                info!(shard_id = entry.shard_id, group_id = %gid, "Path A announcer armed");
                path_a_groups.push(PathAGroup {
                    shard_id: entry.shard_id,
                    group_id_hex: gid.to_lowercase(),
                    enclave_url: entry.enclave_url.clone(),
                });
            }
            shards.insert(entry.shard_id, client);
            sorted_ids.push(entry.shard_id);
        }
        sorted_ids.sort();
        let shard_count = sorted_ids.len() as u32;

        Ok(Self {
            shards,
            shard_count,
            sorted_ids,
            path_a_groups,
        })
    }

    /// Build a single-shard router (no config file needed).
    pub async fn single(enclave_url: &str, shard_id: u32) -> Result<Self> {
        let client = PerpClient::new(enclave_url)?;
        // See `from_config` — same O-I3 audit comment applies. For a
        // single-shard deploy the warning is informational (an older
        // enclave silently runs as shard 0 anyway, which is the correct
        // value for a 1-shard cluster).
        match client.set_shard_id(shard_id).await {
            Ok(_) => info!(shard_id, url = %enclave_url, "single-shard router"),
            Err(e) => {
                tracing::warn!(
                    target: "audit",
                    shard_id_unsupported = true,
                    shard_id,
                    url = %enclave_url,
                    "set_shard_id not supported by enclave (running as shard 0): {}",
                    e
                );
            }
        }

        let mut shards = HashMap::new();
        shards.insert(shard_id, client);

        Ok(Self {
            shards,
            shard_count: 1,
            sorted_ids: vec![shard_id],
            path_a_groups: Vec::new(),
        })
    }

    /// Shards that have a `frost_group_id` configured — Path A announcer runs
    /// once per entry. Empty on the `single()` CLI path or pre-DKG.
    pub fn path_a_groups(&self) -> &[PathAGroup] {
        &self.path_a_groups
    }

    /// Route a user_id to its shard's PerpClient.
    pub fn route(&self, user_id: &str) -> &PerpClient {
        let shard_id = self.shard_for(user_id);
        &self.shards[&shard_id]
    }

    /// Get the shard_id for a user_id.
    pub fn shard_for(&self, user_id: &str) -> u32 {
        if self.shard_count <= 1 {
            return self.sorted_ids[0];
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        user_id.hash(&mut hasher);
        let idx = (hasher.finish() % self.shard_count as u64) as usize;
        self.sorted_ids[idx]
    }

    /// Get a specific shard's client (for broadcast operations like price updates).
    pub fn shard(&self, shard_id: u32) -> Option<&PerpClient> {
        self.shards.get(&shard_id)
    }

    /// Iterate over all shard clients (for broadcast operations).
    pub fn all_shards(&self) -> impl Iterator<Item = (u32, &PerpClient)> {
        self.sorted_ids
            .iter()
            .map(move |id| (*id, &self.shards[id]))
    }

    /// Number of shards.
    pub fn count(&self) -> u32 {
        self.shard_count
    }

    /// The vault shard (shard 0 by convention).
    pub fn vault_shard(&self) -> &PerpClient {
        self.shards
            .get(&0)
            .unwrap_or_else(|| self.shards.values().next().expect("no shards configured"))
    }
}
