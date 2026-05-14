//! Range-shard plan: every file is split into one or more byte-range
//! shards; each shard has a single owner rank that downloads it and
//! broadcasts it to the rest of the cluster.
//!
//! `shard_size == 0` collapses to one shard per file (the v0.3.1 plan
//! shape) and is the default; the legacy [`crate::stages::owners::compute`]
//! path is a thin wrapper around `compute_shards(.., 0)`.
//!
//! Owner assignment uses the same Longest-Processing-Time (LPT) bin pack
//! as `owners::compute`, just over the richer shard list. Determinism
//! across ranks is preserved: the input list, sort order, and tie-breakers
//! are identical on every rank, so no broadcast of the plan is required.

use azcp::BlobItem;

/// One byte-range shard of a file, assigned to one owner rank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardSpec {
    /// Index into the original `entries` slice.
    pub file_idx: usize,
    /// 0..N for each file. `shard_idx == 0` is always present, even for
    /// 0-byte files (where it's the only shard with `byte_len == 0`).
    pub shard_idx: usize,
    /// Byte offset within the file at which this shard begins.
    pub byte_offset: u64,
    /// Length of this shard in bytes. Last shard of a file may be shorter
    /// than `shard_size`; first shard of a 0-byte file is 0.
    pub byte_len: u64,
    /// Owner rank for this shard (downloads + broadcasts it).
    pub owner_rank: usize,
}

fn file_size(item: &BlobItem) -> u64 {
    item.properties
        .as_ref()
        .and_then(|p| p.content_length)
        .unwrap_or(0)
}

/// Derive `shard_size` from `--file-shards N`: split the largest file
/// into roughly `N` pieces, rounded up to a multiple of `bcast_chunk`
/// so the O_DIRECT alignment invariant holds. Returns 0 if `entries`
/// is empty or the largest file is 0 bytes (no sharding makes sense).
pub fn compute_shard_size_from_file_shards(
    entries: &[BlobItem],
    file_shards: usize,
    bcast_chunk: u64,
) -> u64 {
    assert!(file_shards >= 1, "file_shards must be >= 1");
    assert!(bcast_chunk > 0, "bcast_chunk must be > 0");
    let max_size = entries.iter().map(file_size).max().unwrap_or(0);
    if max_size == 0 {
        return 0;
    }
    let raw = max_size.div_ceil(file_shards as u64);
    raw.div_ceil(bcast_chunk) * bcast_chunk
}

fn shards_per_file(size: u64, shard_size: u64) -> usize {
    if shard_size == 0 || size <= shard_size {
        1
    } else {
        size.div_ceil(shard_size) as usize
    }
}

/// Build the full shard plan and assign each shard to an owner rank via
/// LPT bin packing.
///
/// `shard_size == 0` (the default) yields exactly one shard per file
/// covering the whole file — equivalent to v0.3.1's plan shape.
///
/// `count <= 1` short-circuits all owners to rank 0 (single-rank job or
/// degenerate test).
pub fn compute_shards(entries: &[BlobItem], count: usize, shard_size: u64) -> Vec<ShardSpec> {
    // Build the raw shard list (owner_rank=0 placeholder) preserving
    // original file order. Each file contributes >= 1 shard.
    let mut shards: Vec<ShardSpec> = Vec::with_capacity(entries.len());
    for (file_idx, entry) in entries.iter().enumerate() {
        let size = file_size(entry);
        let n = shards_per_file(size, shard_size);
        for shard_idx in 0..n {
            let byte_offset = shard_idx as u64 * shard_size;
            let byte_len = if shard_size == 0 {
                size
            } else {
                let remaining = size.saturating_sub(byte_offset);
                remaining.min(shard_size)
            };
            shards.push(ShardSpec {
                file_idx,
                shard_idx,
                byte_offset,
                byte_len,
                owner_rank: 0,
            });
        }
    }

    if count <= 1 {
        return shards;
    }

    // LPT bin pack: sort indices by (size desc, file_idx asc, shard_idx asc),
    // greedy assign to least-loaded bin (rank-asc tie-break).
    let mut order: Vec<usize> = (0..shards.len()).collect();
    order.sort_by(|&a, &b| {
        let sa = &shards[a];
        let sb = &shards[b];
        sb.byte_len
            .cmp(&sa.byte_len)
            .then_with(|| sa.file_idx.cmp(&sb.file_idx))
            .then_with(|| sa.shard_idx.cmp(&sb.shard_idx))
    });

    let mut loads = vec![0u64; count];
    for &i in &order {
        let bin = loads
            .iter()
            .enumerate()
            .min_by(|(ai, al), (bi, bl)| al.cmp(bl).then_with(|| ai.cmp(bi)))
            .map(|(b, _)| b)
            .unwrap_or(0);
        loads[bin] = loads[bin].saturating_add(shards[i].byte_len);
        shards[i].owner_rank = bin;
    }
    shards
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
    fn empty_input_yields_empty_plan() {
        assert!(compute_shards(&[], 4, 0).is_empty());
        assert!(compute_shards(&[], 4, 1024).is_empty());
    }

    #[test]
    fn shard_size_zero_yields_one_shard_per_file() {
        let entries = vec![item("a", 100), item("b", 200), item("c", 300)];
        let plan = compute_shards(&entries, 4, 0);
        assert_eq!(plan.len(), 3);
        for (i, s) in plan.iter().enumerate() {
            assert_eq!(s.file_idx, i);
            assert_eq!(s.shard_idx, 0);
            assert_eq!(s.byte_offset, 0);
        }
        assert_eq!(plan[0].byte_len, 100);
        assert_eq!(plan[1].byte_len, 200);
        assert_eq!(plan[2].byte_len, 300);
    }

    #[test]
    fn small_file_under_shard_size_is_single_shard() {
        let entries = vec![item("a", 1000)];
        let plan = compute_shards(&entries, 1, 4096);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].byte_offset, 0);
        assert_eq!(plan[0].byte_len, 1000);
    }

    #[test]
    fn file_equal_to_shard_size_is_single_shard() {
        let entries = vec![item("a", 4096)];
        let plan = compute_shards(&entries, 1, 4096);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].byte_len, 4096);
    }

    #[test]
    fn file_one_byte_over_shard_size_yields_two_shards() {
        let entries = vec![item("a", 4097)];
        let plan = compute_shards(&entries, 1, 4096);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].byte_offset, 0);
        assert_eq!(plan[0].byte_len, 4096);
        assert_eq!(plan[1].byte_offset, 4096);
        assert_eq!(plan[1].byte_len, 1);
    }

    #[test]
    fn ten_x_shard_size_yields_ten_shards() {
        let entries = vec![item("a", 10 * 4096)];
        let plan = compute_shards(&entries, 1, 4096);
        assert_eq!(plan.len(), 10);
        for (i, s) in plan.iter().enumerate() {
            assert_eq!(s.shard_idx, i);
            assert_eq!(s.byte_offset, (i as u64) * 4096);
            assert_eq!(s.byte_len, 4096);
        }
    }

    #[test]
    fn zero_byte_file_yields_one_zero_length_shard() {
        let entries = vec![item("empty", 0)];
        let plan = compute_shards(&entries, 1, 4096);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].byte_len, 0);
    }

    #[test]
    fn count_one_assigns_all_to_rank_zero() {
        let entries = vec![item("a", 100), item("b", 200)];
        let plan = compute_shards(&entries, 1, 0);
        assert!(plan.iter().all(|s| s.owner_rank == 0));
    }

    #[test]
    fn lpt_balances_six_files_across_four_ranks() {
        // Sizes 8k+1k=9k, 7k+2k=9k, 6k+3k=9k, 5k+4k=9k → LPT achieves
        // optimal split with spread = 0.
        let sizes = [8_000u64, 7_000, 6_000, 5_000, 4_000, 3_000, 2_000, 1_000];
        let entries: Vec<BlobItem> = sizes
            .iter()
            .enumerate()
            .map(|(i, s)| item(&format!("f{i}"), *s))
            .collect();
        let plan = compute_shards(&entries, 4, 0);
        assert_eq!(plan.len(), 8);
        let mut loads = [0u64; 4];
        for s in &plan {
            loads[s.owner_rank] += s.byte_len;
        }
        let max = *loads.iter().max().unwrap();
        let min = *loads.iter().min().unwrap();
        assert!(
            max - min <= 1_000,
            "LPT spread too wide: loads={loads:?}"
        );
    }

    #[test]
    fn lpt_distributes_shards_of_one_file_across_ranks() {
        // 1 file, 8 equal shards, 4 ranks → 2 shards/rank.
        let entries = vec![item("big", 8 * 4096)];
        let plan = compute_shards(&entries, 4, 4096);
        assert_eq!(plan.len(), 8);
        let mut counts = [0usize; 4];
        for s in &plan {
            counts[s.owner_rank] += 1;
        }
        assert_eq!(counts, [2, 2, 2, 2], "shards not evenly distributed");
    }

    #[test]
    fn deterministic_across_calls() {
        let entries: Vec<BlobItem> = (0..20)
            .map(|i| item(&format!("f{i}"), (i as u64 + 1) * 1000))
            .collect();
        let plan_a = compute_shards(&entries, 7, 1500);
        let plan_b = compute_shards(&entries, 7, 1500);
        assert_eq!(plan_a, plan_b);
    }

    #[test]
    fn matches_legacy_owners_at_shard_size_zero() {
        // Mirrors owners::tests::matches_apply_shard_partition. The
        // owner_rank vector from compute_shards(.., 0) must equal the
        // legacy compute() output for the same inputs.
        let entries: Vec<BlobItem> = (0..20)
            .map(|i| item(&format!("f{i}"), (i as u64 + 1) * 1000))
            .collect();
        let count = 4;
        let shards = compute_shards(&entries, count, 0);
        let from_shards: Vec<usize> = shards.iter().map(|s| s.owner_rank).collect();
        let from_legacy = crate::stages::owners::compute(&entries, count);
        assert_eq!(from_shards, from_legacy);
    }

    #[test]
    fn shard_size_from_file_shards_basic() {
        let entries = vec![item("small", 1024), item("big", 10 * 1024 * 1024 * 1024)];
        let chunk = 64u64 * 1024 * 1024;
        let s = compute_shard_size_from_file_shards(&entries, 4, chunk);
        assert_eq!(s % chunk, 0, "must be chunk-aligned");
        assert!(s >= (10u64 * 1024 * 1024 * 1024).div_ceil(4));
        assert!(s < (10u64 * 1024 * 1024 * 1024).div_ceil(4) + chunk);
    }

    #[test]
    fn shard_size_from_file_shards_empty_is_zero() {
        assert_eq!(compute_shard_size_from_file_shards(&[], 4, 1 << 26), 0);
    }

    #[test]
    fn shard_size_from_file_shards_zero_byte_max_is_zero() {
        let entries = vec![item("z", 0), item("z2", 0)];
        assert_eq!(
            compute_shard_size_from_file_shards(&entries, 4, 1 << 26),
            0
        );
    }

    #[test]
    fn shard_size_from_file_shards_alignment() {
        let entries = vec![item("a", 100 * 1024 * 1024 + 1)];
        let chunk = 64u64 * 1024 * 1024;
        let s = compute_shard_size_from_file_shards(&entries, 1, chunk);
        assert_eq!(s, 128 * 1024 * 1024);
    }
}
