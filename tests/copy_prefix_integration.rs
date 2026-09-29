use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

struct Fixture {
    url: String,
    local: TempDir,
}

impl Fixture {
    fn configured() -> Option<Self> {
        let account = std::env::var("AZCP_TEST_ACCOUNT").ok()?;
        let container = std::env::var("AZCP_TEST_CONTAINER").ok()?;
        let local = TempDir::new().unwrap();
        let unique = local.path().file_name().unwrap().to_str().unwrap();
        Some(Self {
            url: format!(
                "https://{account}.blob.core.windows.net/{container}/azcp-it/copy-prefix-{unique}"
            ),
            local,
        })
    }

    fn upload(&self, name: &str, contents: &[u8]) {
        let path = self.local.path().join("upload.bin");
        std::fs::write(&path, contents).unwrap();
        run_ok(&[
            "copy",
            path.to_str().unwrap(),
            &format!("{}/{name}", self.url),
        ]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let output = run(&["rm", &format!("{}/", self.url), "--recursive"]);
        if !output.status.success() {
            eprintln!(
                "cleanup failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_azcp"))
        .args(args)
        .output()
        .expect("failed to start azcp")
}

fn run_ok(args: &[&str]) -> String {
    let output = run(args);
    assert!(
        output.status.success(),
        "azcp {args:?} failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn assert_contents(dest: &Path) {
    assert_eq!(std::fs::read(dest.join("file.bin")).unwrap(), b"first file");
    assert_eq!(
        std::fs::read(dest.join("subdir/file2.bin")).unwrap(),
        b"nested file"
    );
    assert!(!dest.join("sibling.bin").exists());
}

#[test]
fn recursive_prefix_with_and_without_slash_downloads_identical_contents() {
    let Some(fixture) = Fixture::configured() else {
        eprintln!("SKIP: set AZCP_TEST_ACCOUNT and AZCP_TEST_CONTAINER for live storage tests");
        return;
    };
    fixture.upload("dataset/file.bin", b"first file");
    fixture.upload("dataset/subdir/file2.bin", b"nested file");
    fixture.upload("dataset-other/sibling.bin", b"exclude sibling prefix");

    for suffix in ["dataset", "dataset/"] {
        let source = format!("{}/{suffix}", fixture.url);
        let dest = TempDir::new().unwrap();
        let dry_run = run_ok(&[
            "copy",
            &source,
            dest.path().to_str().unwrap(),
            "--recursive",
            "--dry-run",
        ]);
        assert!(dry_run.contains("dataset/file.bin"));
        assert!(dry_run.contains("dataset/subdir/file2.bin"));
        assert!(!dry_run.contains("sibling.bin"));
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);

        run_ok(&[
            "copy",
            &source,
            dest.path().to_str().unwrap(),
            "--recursive",
            "--no-progress",
        ]);

        assert_contents(dest.path());
    }

    let dest = TempDir::new().unwrap();
    let output = run(&[
        "copy",
        &format!("{}/dataset", fixture.url),
        dest.path().to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("404"));
}

#[test]
fn recursive_exact_blob_takes_precedence_over_matching_directory() {
    let Some(fixture) = Fixture::configured() else {
        eprintln!("SKIP: set AZCP_TEST_ACCOUNT and AZCP_TEST_CONTAINER for live storage tests");
        return;
    };
    fixture.upload("dataset", b"exact blob");
    fixture.upload("dataset/child.bin", b"not selected");
    let dest = TempDir::new().unwrap();

    run_ok(&[
        "copy",
        &format!("{}/dataset", fixture.url),
        dest.path().to_str().unwrap(),
        "--recursive",
        "--no-progress",
    ]);

    assert_eq!(
        std::fs::read(dest.path().join("dataset")).unwrap(),
        b"exact blob"
    );
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 1);
}
