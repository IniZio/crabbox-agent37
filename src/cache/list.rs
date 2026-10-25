//! The read-only listing of an owner's seeds.

use std::cmp::Reverse;

use super::meta::{generation, now, scope_of, seed_of, KEY};
use crate::agent37::Instance;
use crate::lease::status_of;
use crate::protocol::SeedRow;

/// `owner`'s seeds, newest first, in any status.
pub(crate) fn seeds(instances: &[Instance], owner: &str) -> Vec<SeedRow> {
    let mut found: Vec<&Instance> = instances.iter().filter(|i| seed_of(i, owner)).collect();
    found.sort_by_key(|i| Reverse((generation(i), i.id.clone())));
    let now = now();
    found
        .into_iter()
        .map(|i| SeedRow {
            cloud_id: i.id.clone(),
            scope: scope_of(i.meta(KEY).unwrap_or_default()).to_owned(),
            gen: generation(i),
            age_seconds: now.saturating_sub(generation(i)),
            disk_gb: i.disk_gb,
            status: status_of(i).to_owned(),
        })
        .collect()
}
