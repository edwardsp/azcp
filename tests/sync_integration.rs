//! End-to-end integration tests for `azcp sync`.
//!
//! Skipped unless both env vars are set:
//!   AZCP_TEST_ACCOUNT    - storage account name
//!   AZCP_TEST_CONTAINER  - container name
//!
//! Ambient credentials are used (az CLI login or AZURE_STORAGE_* env vars).
//! Each test uses a unique prefix and cleans up via `azcp rm --recursive`.
//!
//! Run:
//!   AZCP_TEST_ACCOUNT=foo AZCP_TEST_CONTAINER=bar \
//!     cargo test --test sync_integration -- --nocapture

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

struct TestEnv {
    account: String,
    container: String,
    prefix: String,
}

impl TestEnv {
    fn from_env(test_name: &str) -> Option<Self> {
        let account = std::env::var("AZCP_TEST_ACCOUNT").ok()?;
        let container = std::env::var("AZCP_TEST_CONTAINER").ok()?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let prefix = format!("azcp-it/{test_name}-{nanos}");
        Some(Self {
            account,
            container,
            prefix,
        })
    }

    fn blob_url(&self) -> String {
        format!(
            "https://{}.blob.core.windows.net/{}/{}",
            self.account, self.container, self.prefix
        )
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = run_azcp(&["rm", "--recursive", &self.blob_url()]);
    }
}

macro_rules! skip_unless_configured {
    ($name:expr) => {
        match TestEnv::from_env($name) {
            Some(env) => env,
            None => {
                eprintln!(
                    "SKIP {}: set AZCP_TEST_ACCOUNT and AZCP_TEST_CONTAINER to run",
                    $name
                );
                return;
            }
        }
    };
}

fn azcp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_azcp"))
}

fn run_azcp(args: &[&str]) -> Output {
    Command::new(azcp_bin())
        .args(args)
        .output()
        .expect("failed to spawn azcp")
}

fn run_azcp_ok(args: &[&str]) -> String {
    let out = run_azcp(args);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "azcp {args:?} failed\n--stdout--\n{stdout}\n--stderr--\n{stderr}"
    );
    stdout
}

fn write_file(dir: &Path, rel: &str, contents: &[u8]) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut f = fs::File::create(&path).unwrap();
    f.write_all(contents).unwrap();
}

fn make_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    write_file(tmp.path(), "a.txt", b"hello\n");
    write_file(tmp.path(), "b.txt", b"world\n");
    write_file(tmp.path(), "sub/c.txt", b"nested\n");
    tmp
}

fn parse_summary(stdout: &str) -> (u64, u64, u64) {
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.contains("Sync complete:"))
        .unwrap_or_else(|| panic!("no summary line in output:\n{stdout}"));
    let parse_num = |needle: &str| -> u64 {
        let idx = line
            .find(needle)
            .unwrap_or_else(|| panic!("missing '{needle}' in summary: {line}"));
        let before = &line[..idx];
        let n: String = before
            .chars()
            .rev()
            .skip_while(|c| c.is_whitespace())
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        n.parse()
            .unwrap_or_else(|_| panic!("bad number before '{needle}' in: {line}"))
    };
    let planned = parse_num("planned");
    let transferred = if line.contains("uploaded") {
        parse_num("uploaded")
    } else {
        parse_num("downloaded")
    };
    let unchanged = parse_num("unchanged");
    (planned, transferred, unchanged)
}

#[test]
fn sync_upload_then_rerun_skips() {
    let env = skip_unless_configured!("upload_rerun");
    let src = make_fixture();
    let src_path = src.path().to_str().unwrap();

    let out = run_azcp_ok(&["sync", src_path, &env.blob_url()]);
    let (planned, uploaded, _) = parse_summary(&out);
    assert_eq!(planned, 3, "initial: {out}");
    assert_eq!(uploaded, 3, "initial: {out}");

    let out = run_azcp_ok(&["sync", src_path, &env.blob_url()]);
    let (planned, uploaded, unchanged) = parse_summary(&out);
    assert_eq!(planned, 0, "rerun: {out}");
    assert_eq!(uploaded, 0, "rerun: {out}");
    assert_eq!(unchanged, 3, "rerun: {out}");
}

#[test]
fn compare_method_size_misses_same_size_change() {
    let env = skip_unless_configured!("size_miss");
    let src = make_fixture();
    let src_path = src.path().to_str().unwrap();

    run_azcp_ok(&["sync", src_path, &env.blob_url()]);
    write_file(src.path(), "a.txt", b"HELLO\n");

    let out = run_azcp_ok(&[
        "sync",
        src_path,
        &env.blob_url(),
        "--compare-method",
        "size",
        "--dry-run",
    ]);
    let (planned, _, unchanged) = parse_summary(&out);
    assert_eq!(
        planned, 0,
        "size mode should miss content-only change: {out}"
    );
    assert_eq!(unchanged, 3, "{out}");
}

#[test]
fn compare_method_md5_detects_same_size_change() {
    let env = skip_unless_configured!("md5_detect");
    let src = make_fixture();
    let src_path = src.path().to_str().unwrap();

    run_azcp_ok(&["sync", src_path, &env.blob_url()]);

    // Azure auto-computes Content-MD5 for single-shot PutBlob uploads
    // (small files), which is what lets md5 mode work without --check-md5.
    write_file(src.path(), "a.txt", b"HELLO\n");

    let out = run_azcp_ok(&[
        "sync",
        src_path,
        &env.blob_url(),
        "--compare-method",
        "md5",
        "--dry-run",
    ]);
    let (planned, _, unchanged) = parse_summary(&out);
    assert_eq!(planned, 1, "md5 mode should catch content change: {out}");
    assert_eq!(unchanged, 2, "{out}");
    assert!(
        out.contains("a.txt"),
        "expected a.txt in dry-run list: {out}"
    );
}

#[test]
fn compare_method_always_reuploads_everything() {
    let env = skip_unless_configured!("always");
    let src = make_fixture();
    let src_path = src.path().to_str().unwrap();

    run_azcp_ok(&["sync", src_path, &env.blob_url()]);

    let out = run_azcp_ok(&[
        "sync",
        src_path,
        &env.blob_url(),
        "--compare-method",
        "always",
        "--dry-run",
    ]);
    let (planned, _, unchanged) = parse_summary(&out);
    assert_eq!(planned, 3, "{out}");
    assert_eq!(unchanged, 0, "{out}");
}

#[test]
fn delete_destination_removes_missing_remote() {
    let env = skip_unless_configured!("delete_dest");
    let src = make_fixture();
    let src_path = src.path().to_str().unwrap();

    run_azcp_ok(&["sync", src_path, &env.blob_url()]);
    fs::remove_file(src.path().join("b.txt")).unwrap();

    let out = run_azcp_ok(&[
        "sync",
        src_path,
        &env.blob_url(),
        "--delete-destination",
        "--dry-run",
    ]);
    assert!(
        out.contains("delete remote") && out.contains("b.txt"),
        "expected b.txt delete plan: {out}"
    );

    run_azcp_ok(&["sync", src_path, &env.blob_url(), "--delete-destination"]);

    let out = run_azcp_ok(&["sync", src_path, &env.blob_url()]);
    let (planned, _, unchanged) = parse_summary(&out);
    assert_eq!(planned, 0, "{out}");
    assert_eq!(unchanged, 2, "{out}");
}

#[test]
fn download_direction_blob_to_local() {
    let env = skip_unless_configured!("download");
    let src = make_fixture();
    let src_path = src.path().to_str().unwrap();

    run_azcp_ok(&["sync", src_path, &env.blob_url()]);

    let dl = TempDir::new().unwrap();
    let dl_path = dl.path().to_str().unwrap();

    let out = run_azcp_ok(&["sync", &env.blob_url(), dl_path]);
    let (planned, downloaded, _) = parse_summary(&out);
    assert_eq!(planned, 3, "{out}");
    assert_eq!(downloaded, 3, "{out}");

    assert!(dl.path().join("a.txt").exists());
    assert!(dl.path().join("b.txt").exists());
    assert!(dl.path().join("sub/c.txt").exists());

    let out = run_azcp_ok(&["sync", &env.blob_url(), dl_path]);
    let (planned, downloaded, unchanged) = parse_summary(&out);
    assert_eq!(planned, 0, "{out}");
    assert_eq!(downloaded, 0, "{out}");
    assert_eq!(unchanged, 3, "{out}");
}

#[test]
fn include_and_exclude_patterns_filter() {
    let env = skip_unless_configured!("patterns");
    let src = make_fixture();
    let src_path = src.path().to_str().unwrap();

    let out = run_azcp_ok(&[
        "sync",
        src_path,
        &env.blob_url(),
        "--include-pattern",
        "*.txt",
        "--compare-method",
        "always",
        "--dry-run",
    ]);
    let (planned, _, _) = parse_summary(&out);
    assert_eq!(planned, 3, "include *.txt: {out}");

    let out = run_azcp_ok(&[
        "sync",
        src_path,
        &env.blob_url(),
        "--exclude-pattern",
        "sub/*",
        "--compare-method",
        "always",
        "--dry-run",
    ]);
    let (planned, _, _) = parse_summary(&out);
    assert_eq!(planned, 2, "exclude sub/*: {out}");
    assert!(
        !out.contains("sub/c.txt"),
        "sub/c.txt should be excluded: {out}"
    );
}
