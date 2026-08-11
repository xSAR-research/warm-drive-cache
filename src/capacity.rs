//! Overflow-safe projected-cache capacity accounting.
use std::{collections::HashMap, fs, path::Path};
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Projection {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub eligible: u64,
}
impl Projection {
    pub fn projected_used(self) -> u64 {
        self.used.saturating_add(self.eligible)
    }
    pub fn projected_free(self) -> i128 {
        self.total as i128 - self.projected_used() as i128
    }
    pub fn utilization(self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.projected_used() as f64 / self.total as f64
        }
    }
    pub fn warns(self) -> bool {
        self.utilization() > 0.90
    }
}
pub fn eligible_bytes<I: IntoIterator<Item = u64>>(sizes: I, min: u64, max: i64) -> u64 {
    sizes
        .into_iter()
        .filter(|n| crate::worker::should_read_file_contents(*n, min, max))
        .fold(0u64, u64::saturating_add)
}
#[cfg(unix)]
pub fn aggregate_by_filesystem<'a, I>(items: I) -> HashMap<u64, u64>
where
    I: IntoIterator<Item = (&'a Path, u64)>,
{
    use std::os::unix::fs::MetadataExt;
    let mut out = HashMap::new();
    for (p, n) in items {
        if let Ok(m) = fs::metadata(p) {
            let v = out.entry(m.dev()).or_insert(0u64);
            *v = (*v).saturating_add(n)
        }
    }
    out
}
