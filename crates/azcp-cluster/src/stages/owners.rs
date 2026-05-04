use azcp::BlobItem;

pub fn compute(entries: &[BlobItem], count: usize) -> Vec<usize> {
    let n = entries.len();
    if count <= 1 {
        return vec![0; n];
    }
    let mut indexed: Vec<(usize, u64, &str)> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let sz = e
                .properties
                .as_ref()
                .and_then(|p| p.content_length)
                .unwrap_or(0);
            (i, sz, e.name.as_str())
        })
        .collect();
    indexed.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(b.2)));
    let mut loads = vec![0u64; count];
    let mut owner = vec![0usize; n];
    for (orig_idx, sz, _) in &indexed {
        let bin = loads
            .iter()
            .enumerate()
            .min_by(|(ai, al), (bi, bl)| al.cmp(bl).then_with(|| ai.cmp(bi)))
            .map(|(b, _)| b)
            .unwrap_or(0);
        loads[bin] = loads[bin].saturating_add(*sz);
        owner[*orig_idx] = bin;
    }
    owner
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
