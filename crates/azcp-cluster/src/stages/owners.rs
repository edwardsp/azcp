use azcp::BlobItem;

use crate::stages::shards::compute_shards;

/// Legacy file-level owner mapping. Returns one owner rank per entry,
/// in original-file order. Equivalent to projecting `owner_rank` out of
/// `compute_shards(entries, count, 0)` (one shard per file).
///
/// Kept as a thin wrapper so call sites that don't yet need range
/// sharding can stay unchanged. New code should prefer `compute_shards`.
pub fn compute(entries: &[BlobItem], count: usize) -> Vec<usize> {
    compute_shards(entries, count, 0)
        .into_iter()
        .map(|s| s.owner_rank)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use azcp::BlobProperties;

    fn item(name: &str, size: u64) -> BlobItem {
        BlobItem {
            name: name.to_string(),
            properties: Some(BlobProperties {
                last_modified: None,
                content_length: Some(size),
                content_type: None,
                content_md5: None,
                etag: None,
                blob_type: None,
                access_tier: None,
            }),
        }
    }

    #[test]
    fn matches_apply_shard_partition() {
        let entries: Vec<BlobItem> = (0..20)
            .map(|i| item(&format!("f{i}"), (i as u64 + 1) * 1000))
            .collect();
        let count = 4;
        let owners = compute(&entries, count);
        for rank in 0..count {
            let mut copy = entries.clone();
            azcp::apply_shard(
                &mut copy,
                Some((rank, count)),
                |e| e.name.clone(),
                |e| {
                    e.properties
                        .as_ref()
                        .and_then(|p| p.content_length)
                        .unwrap_or(0)
                },
            );
            let from_owners: Vec<&str> = entries
                .iter()
                .enumerate()
                .filter(|(i, _)| owners[*i] == rank)
                .map(|(_, e)| e.name.as_str())
                .collect();
            let from_apply: Vec<&str> = copy.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(from_owners, from_apply, "rank {rank} partition mismatch");
        }
    }

    #[test]
    fn count_one_returns_zeros() {
        let entries = vec![item("a", 1), item("b", 2)];
        assert_eq!(compute(&entries, 1), vec![0, 0]);
    }
}
