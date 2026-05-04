use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};
use mpi::topology::SimpleCommunicator;
use mpi::traits::*;

use azcp::BlobItem;

use crate::cli::{Args, Compare};
use crate::filelist;
use crate::paths::local_rel;

// Packed presence matrix: bit (rank, file) = 1 iff `rank` already has `file`
// locally with the right size. Built by every rank computing its own row and
// MPI_Allgather'ing into the full matrix. Allows every rank to deterministically
// pick the same broadcaster (lowest-rank haver) per file with no extra round-trips.
pub struct Presence {
    pub world_size: usize,
    pub num_files: usize,
    bytes_per_rank: usize,
    data: Vec<u8>,
}

impl Presence {
    pub fn has(&self, rank: usize, file: usize) -> bool {
        let byte = self.data[rank * self.bytes_per_rank + file / 8];
        (byte >> (file % 8)) & 1 == 1
    }

    pub fn count(&self, file: usize) -> usize {
        (0..self.world_size).filter(|r| self.has(*r, file)).count()
    }

    pub fn first_haver(&self, file: usize) -> Option<usize> {
        (0..self.world_size).find(|r| self.has(*r, file))
    }

    #[cfg(test)]
    pub fn from_raw_for_tests(
        world_size: usize,
        num_files: usize,
        bytes_per_rank: usize,
        data: Vec<u8>,
    ) -> Self {
        Self {
            world_size,
            num_files,
            bytes_per_rank,
            data,
        }
    }
}

pub fn run(world: &SimpleCommunicator, args: &Args, entries: &[BlobItem]) -> Result<Presence> {
    let world_size = world.size() as usize;
    let num_files = entries.len();
    let bytes_per_rank = num_files.div_ceil(8);

    let predicate = build_predicate(args)?;

    let mut local_row = vec![0u8; bytes_per_rank];
    for (i, e) in entries.iter().enumerate() {
        if predicate(e) {
            local_row[i / 8] |= 1 << (i % 8);
        }
    }

    let mut data = vec![0u8; bytes_per_rank * world_size];
    if bytes_per_rank > 0 {
        world.all_gather_into(&local_row[..], &mut data[..]);
    }

    Ok(Presence {
        world_size,
        num_files,
        bytes_per_rank,
        data,
    })
}

type Pred = Box<dyn Fn(&BlobItem) -> bool + Send>;

fn build_predicate(args: &Args) -> Result<Pred> {
    let source = args.source.clone();
    let dest = args.dest.clone();
    match args.compare {
        Compare::None => Ok(Box::new(|_e: &BlobItem| false)),
        Compare::Size => Ok(Box::new(move |e: &BlobItem| {
            size_matches(&source, &dest, e)
        })),
        Compare::Filelist => {
            let path = args
                .filelist
                .as_ref()
                .ok_or_else(|| anyhow!("--compare filelist requires --filelist FILE"))?;
            let prior_text = std::fs::read_to_string(path)
                .map_err(|e| anyhow!("failed to read --filelist {}: {e}", path.display()))?;
            let prior: HashMap<String, (u64, Option<String>)> = filelist::deserialize(&prior_text)
                .into_iter()
                .map(|e| {
                    let (size, lm) = e
                        .properties
                        .map(|p| (p.content_length.unwrap_or(0), p.last_modified))
                        .unwrap_or((0, None));
                    (e.name, (size, lm))
                })
                .collect();
            Ok(Box::new(move |e: &BlobItem| {
                let Some((prior_size, prior_lm)) = prior.get(&e.name) else {
                    return false;
                };
                let live_size = e
                    .properties
                    .as_ref()
                    .and_then(|p| p.content_length)
                    .unwrap_or(u64::MAX);
                let live_lm = e.properties.as_ref().and_then(|p| p.last_modified.as_ref());
                if live_size != *prior_size {
                    return false;
                }
                if live_lm != prior_lm.as_ref() {
                    return false;
                }
                size_matches(&source, &dest, e)
            }))
        }
    }
}

fn size_matches(source: &str, dest: &Path, e: &BlobItem) -> bool {
    let want = match e.properties.as_ref().and_then(|p| p.content_length) {
        Some(s) => s,
        None => return false,
    };
    match std::fs::metadata(dest.join(local_rel(source, &e.name))) {
        Ok(m) => m.is_file() && m.len() == want,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence_from(rows: &[&[u8]], num_files: usize) -> Presence {
        let world_size = rows.len();
        let bytes_per_rank = num_files.div_ceil(8);
        let mut data = vec![0u8; bytes_per_rank * world_size];
        for (r, row) in rows.iter().enumerate() {
            data[r * bytes_per_rank..r * bytes_per_rank + row.len()].copy_from_slice(row);
        }
        Presence {
            world_size,
            num_files,
            bytes_per_rank,
            data,
        }
    }

    #[test]
    fn has_count_first_haver() {
        // 3 ranks, 4 files: rank0 has files {0,2}, rank1 has {2}, rank2 has {1,2,3}
        // bits: rank0 = 0b0101, rank1 = 0b0100, rank2 = 0b1110
        let p = presence_from(&[&[0b0101], &[0b0100], &[0b1110]], 4);
        assert!(p.has(0, 0));
        assert!(!p.has(0, 1));
        assert!(p.has(0, 2));
        assert!(!p.has(0, 3));
        assert!(p.has(2, 1));
        assert_eq!(p.count(0), 1);
        assert_eq!(p.count(1), 1);
        assert_eq!(p.count(2), 3);
        assert_eq!(p.count(3), 1);
        assert_eq!(p.first_haver(0), Some(0));
        assert_eq!(p.first_haver(1), Some(2));
        assert_eq!(p.first_haver(2), Some(0));
        assert_eq!(p.first_haver(3), Some(2));
    }

    #[test]
    fn no_havers() {
        let p = presence_from(&[&[0u8], &[0u8]], 5);
        assert_eq!(p.count(0), 0);
        assert_eq!(p.first_haver(0), None);
    }
}
