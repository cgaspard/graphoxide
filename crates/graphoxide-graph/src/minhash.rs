//! Small deterministic MinHash sketch and candidate index.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinHash {
    hashvalues: Vec<u64>,
}

impl MinHash {
    pub fn new(num_perm: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(num_perm > 0, "num_perm must be positive");
        Ok(Self {
            hashvalues: vec![u64::MAX; num_perm],
        })
    }

    pub fn hashvalues(&self) -> &[u64] {
        &self.hashvalues
    }

    pub fn update(&mut self, value: &[u8]) {
        let base = fnv1a(value);
        for (index, slot) in self.hashvalues.iter_mut().enumerate() {
            let candidate = splitmix64(base ^ (index as u64).wrapping_mul(0x9e3779b97f4a7c15));
            *slot = (*slot).min(candidate);
        }
    }

    pub fn similarity(&self, other: &Self) -> f64 {
        if self.hashvalues.len() != other.hashvalues.len() || self.hashvalues.is_empty() {
            return 0.0;
        }
        self.hashvalues
            .iter()
            .zip(&other.hashvalues)
            .filter(|(left, right)| left == right)
            .count() as f64
            / self.hashvalues.len() as f64
    }
}

fn fnv1a(value: &[u8]) -> u64 {
    value.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

#[derive(Debug, Clone)]
pub struct MinHashLsh {
    threshold: f64,
    num_perm: usize,
    entries: BTreeMap<String, MinHash>,
}

impl MinHashLsh {
    pub fn new(threshold: f64, num_perm: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (0.0..=1.0).contains(&threshold),
            "threshold must be in [0, 1]"
        );
        anyhow::ensure!(num_perm > 0, "num_perm must be positive");
        Ok(Self {
            threshold,
            num_perm,
            entries: BTreeMap::new(),
        })
    }

    pub fn insert(&mut self, key: impl Into<String>, sketch: MinHash) -> anyhow::Result<()> {
        let key = key.into();
        anyhow::ensure!(
            sketch.hashvalues.len() == self.num_perm,
            "sketch num_perm does not match index"
        );
        anyhow::ensure!(
            !self.entries.contains_key(&key),
            "key '{key}' already exists"
        );
        self.entries.insert(key, sketch);
        Ok(())
    }

    pub fn query(&self, sketch: &MinHash) -> Vec<String> {
        if sketch.hashvalues.len() != self.num_perm {
            return Vec::new();
        }
        self.entries
            .iter()
            .filter(|(_, candidate)| candidate.similarity(sketch) >= self.threshold)
            .map(|(key, _)| key.clone())
            .collect()
    }
}

/// Deterministic, bounded band/row parameters for a permutation budget.
pub fn optimal_lsh_params(threshold: f64, num_perm: usize) -> (usize, usize) {
    if num_perm == 0 {
        return (0, 0);
    }
    let target_rows = if threshold <= 0.0 {
        1
    } else {
        ((1.0 / threshold).ceil() as usize).clamp(1, num_perm)
    };
    let bands = (num_perm / target_rows).max(1);
    (bands, target_rows.min(num_perm / bands).max(1))
}
