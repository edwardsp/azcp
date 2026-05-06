use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use md5::{Digest, Md5};
use mpi::environment;
use mpi::topology::SimpleCommunicator;
use mpi::traits::*;
use rayon::prelude::*;

use azcp::BlobItem;

use crate::cli::Args;
use crate::paths::local_rel;

const DIGEST_LEN: usize = 16;

pub fn run(world: &SimpleCommunicator, args: &Args, entries: &[BlobItem]) -> Result<()> {
    let rank = world.rank() as usize;
    let world_size = world.size() as usize;
    let n = entries.len();

    let t0 = environment::time();

    let local_digests: Vec<[u8; DIGEST_LEN]> = entries
        .par_iter()
        .map(|e| {
            let path = args.dest.join(local_rel(&args.source, &e.name));
            compute_md5(&path).with_context(|| format!("md5 {}", path.display()))
        })
        .collect::<Result<_>>()?;

    let local_bytes: Vec<u8> = local_digests
        .iter()
        .flat_map(|d| d.iter().copied())
        .collect();
    let mut all_bytes = vec![0u8; n * DIGEST_LEN * world_size];
    if !local_bytes.is_empty() {
        world.all_gather_into(&local_bytes[..], &mut all_bytes[..]);
    }

    let t_hash = environment::time() - t0;

    if rank != 0 {
        return Ok(());
    }

    let mut mismatches: Vec<String> = Vec::new();
    let mut blob_md5_checked: usize = 0;
    let mut blob_md5_passed: usize = 0;
    let mut cross_rank_only: usize = 0;

    let b64 = base64::engine::general_purpose::STANDARD;

    for (i, e) in entries.iter().enumerate() {
        let rank0 = &all_bytes[i * DIGEST_LEN..(i + 1) * DIGEST_LEN];

        let mut diverged = false;
        for r in 1..world_size {
            let dr = &all_bytes
                [r * n * DIGEST_LEN + i * DIGEST_LEN..r * n * DIGEST_LEN + (i + 1) * DIGEST_LEN];
            if dr != rank0 {
                diverged = true;
                mismatches.push(format!(
                    "{}: rank 0 md5={} != rank {r} md5={}",
                    e.name,
                    hex(rank0),
                    hex(dr)
                ));
            }
        }

        let blob = e
            .properties
            .as_ref()
            .and_then(|p| p.content_md5.as_ref())
            .and_then(|s| b64.decode(s).ok())
            .filter(|d| d.len() == DIGEST_LEN);

        match blob {
            Some(d) => {
                blob_md5_checked += 1;
                if d.as_slice() == rank0 {
                    blob_md5_passed += 1;
                } else {
                    mismatches.push(format!(
                        "{}: rank 0 md5={} != blob Content-MD5={}",
                        e.name,
                        hex(rank0),
                        hex(&d)
                    ));
                }
            }
            None => {
                if !diverged {
                    cross_rank_only += 1;
                }
            }
        }
    }

    println!(
        "[verify] {n} files T={:.2}s: {cross_rank_only} cross-rank-OK, \
         {blob_md5_checked} blob-md5-checked ({blob_md5_passed} pass), \
         {} mismatches",
        t_hash,
        mismatches.len()
    );
    for m in &mismatches {
        eprintln!("[verify] MISMATCH {m}");
    }
    if !mismatches.is_empty() {
        return Err(anyhow!("{} verification failure(s)", mismatches.len()));
    }
    Ok(())
}

fn compute_md5(path: &Path) -> Result<[u8; DIGEST_LEN]> {
    let mut f = File::open(path)?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}
