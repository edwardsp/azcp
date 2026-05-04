use crate::stages::presence::Presence;

pub struct Classification {
    pub skipped: usize,
    pub bcast_only: Vec<(usize, usize)>,
    pub needs_download: Vec<usize>,
}

pub fn classify(presence: &Presence) -> Classification {
    let mut skipped = 0usize;
    let mut bcast_only = Vec::new();
    let mut needs_download = Vec::new();
    for i in 0..presence.num_files {
        let count = presence.count(i);
        if count == presence.world_size {
            skipped += 1;
        } else if count == 0 {
            needs_download.push(i);
        } else {
            bcast_only.push((i, presence.first_haver(i).unwrap()));
        }
    }
    Classification {
        skipped,
        bcast_only,
        needs_download,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::presence::Presence;

    fn make_presence(rows: &[u8], num_files: usize, world_size: usize) -> Presence {
        let bytes_per_rank = num_files.div_ceil(8);
        assert_eq!(rows.len(), bytes_per_rank * world_size);
        Presence::from_raw_for_tests(world_size, num_files, bytes_per_rank, rows.to_vec())
    }

    #[test]
    fn classify_three_categories() {
        // bit i (LSB-first) of byte = 1 iff that rank has file i.
        // rank0={3} rank1={1,2} rank2={1,3}; counts file0..3 = [0,2,1,2]; world=3
        let p = make_presence(&[0b1000, 0b0110, 0b1010], 4, 3);
        let c = classify(&p);
        assert_eq!(c.skipped, 0);
        assert_eq!(c.needs_download, vec![0]);
        let bcast: std::collections::HashMap<_, _> = c.bcast_only.iter().copied().collect();
        assert_eq!(bcast.get(&1), Some(&1));
        assert_eq!(bcast.get(&2), Some(&1));
        assert_eq!(bcast.get(&3), Some(&0));
    }

    #[test]
    fn classify_all_have_skipped() {
        let p = make_presence(&[0b11, 0b11], 2, 2);
        let c = classify(&p);
        assert_eq!(c.skipped, 2);
        assert!(c.bcast_only.is_empty());
        assert!(c.needs_download.is_empty());
    }

    #[test]
    fn classify_none_have_all_download() {
        let p = make_presence(&[0, 0], 3, 2);
        let c = classify(&p);
        assert_eq!(c.skipped, 0);
        assert!(c.bcast_only.is_empty());
        assert_eq!(c.needs_download, vec![0, 1, 2]);
    }
}
