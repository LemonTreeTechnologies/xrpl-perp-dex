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
}

pub struct ShardRouter {
    shards: HashMap<u32, PerpClient>,
    shard_count: u32,
    sorted_ids: Vec<u32>,
}

impl ShardRouter {
    /// Build from a shards.toml config file.
    pub async fn from_config(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: ShardsConfig =
            toml::from_str(&data).context("invalid shards.toml")?;

        let mut shards = HashMap::new();
        let mut sorted_ids = Vec::new();
        for entry in &config.shards {
            let client = PerpClient::new(&entry.enclave_url)?;
            match client.set_shard_id(entry.shard_id).await {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(shard_id = entry.shard_id, url = %entry.enclave_url,
                        "set_shard_id not supported by enclave: {}", e);
                }
            }
            info!(shard_id = entry.shard_id, url = %entry.enclave_url, "shard registered");
            shards.insert(entry.shard_id, client);
            sorted_ids.push(entry.shard_id);
        }
        sorted_ids.sort();
        let shard_count = sorted_ids.len() as u32;

        Ok(Self {
            shards,
            shard_count,
            sorted_ids,
        })
    }

    /// Build a single-shard router (no config file needed).
    pub async fn single(enclave_url: &str, shard_id: u32) -> Result<Self> {
        let client = PerpClient::new(enclave_url)?;
        match client.set_shard_id(shard_id).await {
            Ok(_) => info!(shard_id, url = %enclave_url, "single-shard router"),
            Err(e) => {
                tracing::warn!(shard_id, url = %enclave_url,
                    "set_shard_id not supported by enclave (old binary?): {}", e);
            }
        }

        let mut shards = HashMap::new();
        shards.insert(shard_id, client);

        Ok(Self {
            shards,
            shard_count: 1,
            sorted_ids: vec![shard_id],
        })
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
        self.sorted_ids.iter().map(move |id| (*id, &self.shards[id]))
    }

    /// Number of shards.
    pub fn count(&self) -> u32 {
        self.shard_count
    }

    /// The vault shard (shard 0 by convention).
    pub fn vault_shard(&self) -> &PerpClient {
        self.shards.get(&0).unwrap_or_else(|| {
            self.shards.values().next().expect("no shards configured")
        })
    }
}
