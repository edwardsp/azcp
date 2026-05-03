use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};

use azcp::BlobItem;

use crate::cli::{Args, Compare};
use crate::filelist;
use crate::paths::local_rel;

pub struct Plan {
    pub to_transfer: Vec<BlobItem>,
    pub skipped: usize,
}

impl std::fmt::Debug for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plan")
            .field("to_transfer", &self.to_transfer.len())
            .field("skipped", &self.skipped)
            .finish()
    }
}

pub fn plan(args: &Args, entries: Vec<BlobItem>) -> Result<Plan> {
    match args.compare {
        Compare::None => Ok(Plan {
            skipped: 0,
            to_transfer: entries,
        }),
        Compare::Size => Ok(filter(entries, |e| {
            size_matches(&args.source, &args.dest, e)
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
            Ok(filter(entries, |e| {
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
                size_matches(&args.source, &args.dest, e)
            }))
        }
    }
}

fn filter(entries: Vec<BlobItem>, skip_if: impl Fn(&BlobItem) -> bool) -> Plan {
    let mut to_transfer = Vec::with_capacity(entries.len());
    let mut skipped = 0usize;
    for e in entries {
        if skip_if(&e) {
            skipped += 1;
        } else {
            to_transfer.push(e);
        }
    }
    Plan {
        to_transfer,
        skipped,
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
    use azcp::BlobProperties;
    use std::fs;
    use std::path::PathBuf;

    fn item(name: &str, size: u64, lm: Option<&str>) -> BlobItem {
        BlobItem {
            name: name.to_string(),
            properties: Some(BlobProperties {
                last_modified: lm.map(str::to_string),
                content_length: Some(size),
                content_type: None,
                content_md5: None,
                etag: None,
                blob_type: None,
                access_tier: None,
            }),
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("azcp-cluster-diff-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn touch(dir: &Path, name: &str, size: u64) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, vec![0u8; size as usize]).unwrap();
    }

    fn args(dest: PathBuf, compare: Compare, filelist: Option<PathBuf>) -> Args {
        Args {
            source: "https://x.blob.core.windows.net/c/p/".into(),
            dest,
            shardlist: None,
            compare,
            filelist,
            save_filelist: None,
            concurrency: 1,
            block_size: 1,
            parallel_files: 1,
            bcast_chunk: 1,
            max_retries: 0,
            no_progress: true,
            stage: crate::cli::Stage::All,
        }
    }

    #[test]
    fn none_skips_zero() {
        let dest = tmpdir("none");
        let entries = vec![item("a", 10, None), item("b", 20, None)];
        let plan = plan(&args(dest, Compare::None, None), entries).unwrap();
        assert_eq!(plan.skipped, 0);
        assert_eq!(plan.to_transfer.len(), 2);
    }

    #[test]
    fn size_skips_match_keeps_mismatch() {
        let dest = tmpdir("size");
        touch(&dest, "match.bin", 10);
        touch(&dest, "wrong.bin", 99);
        let entries = vec![
            item("match.bin", 10, None),
            item("wrong.bin", 10, None),
            item("missing.bin", 10, None),
        ];
        let plan = plan(&args(dest, Compare::Size, None), entries).unwrap();
        assert_eq!(plan.skipped, 1);
        assert_eq!(plan.to_transfer.len(), 2);
        let names: Vec<_> = plan.to_transfer.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"wrong.bin"));
        assert!(names.contains(&"missing.bin"));
    }

    #[test]
    fn filelist_requires_filelist_arg() {
        let dest = tmpdir("filelist-missing");
        let entries = vec![item("a", 1, Some("t"))];
        let err = plan(&args(dest, Compare::Filelist, None), entries).unwrap_err();
        assert!(err.to_string().contains("--filelist"));
    }

    #[test]
    fn filelist_skips_only_when_size_and_lm_and_local_match() {
        let dest = tmpdir("filelist");
        let prior = dest.join("prior.tsv");
        fs::write(
            &prior,
            "skip.bin\t10\t2024-01-01T00:00:00Z\n\
             lm-changed.bin\t10\t2024-01-01T00:00:00Z\n\
             size-changed.bin\t10\t2024-01-01T00:00:00Z\n",
        )
        .unwrap();
        touch(&dest, "skip.bin", 10);
        touch(&dest, "lm-changed.bin", 10);
        touch(&dest, "size-changed.bin", 10);
        let entries = vec![
            item("skip.bin", 10, Some("2024-01-01T00:00:00Z")),
            item("lm-changed.bin", 10, Some("2024-02-01T00:00:00Z")),
            item("size-changed.bin", 11, Some("2024-01-01T00:00:00Z")),
            item("missing-from-prior.bin", 10, Some("2024-01-01T00:00:00Z")),
        ];
        let plan = plan(&args(dest, Compare::Filelist, Some(prior)), entries).unwrap();
        assert_eq!(plan.skipped, 1, "only skip.bin should be skipped");
        let names: Vec<_> = plan.to_transfer.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"lm-changed.bin"));
        assert!(names.contains(&"size-changed.bin"));
        assert!(names.contains(&"missing-from-prior.bin"));
    }
}
