use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct TransferProgress {
    multi: Arc<MultiProgress>,
    total_bar: ProgressBar,
    total_files: u64,
    files_done: AtomicU64,
    enabled: bool,
}

impl TransferProgress {
    pub fn new(total_files: u64, total_bytes: u64, enabled: bool) -> Self {
        let multi = if enabled {
            Arc::new(MultiProgress::new())
        } else {
            let m = MultiProgress::new();
            m.set_draw_target(ProgressDrawTarget::hidden());
            Arc::new(m)
        };
        let total_bar = multi.add(ProgressBar::new(total_bytes.max(1)));
        if enabled {
            total_bar.set_style(
                ProgressStyle::default_bar()
                    .template(
                        "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {msg} {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>12} ETA {eta:>5}",
                    )
                    .unwrap()
                    .progress_chars("#>-"),
            );
            total_bar.set_message(format!("0/{total_files} files"));
            total_bar.enable_steady_tick(Duration::from_millis(150));
        } else {
            total_bar.set_draw_target(ProgressDrawTarget::hidden());
        }
        Self {
            multi,
            total_bar,
            total_files,
            files_done: AtomicU64::new(0),
            enabled,
        }
    }

    pub fn create_file_bar(&self, size: u64) -> ProgressBar {
        let pb = self.multi.add(ProgressBar::new(size));
        if self.enabled {
            pb.set_style(
                ProgressStyle::default_bar()
                    .template(
                        "  {msg:<40!.dim} [{bar:25.green/white}] {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>12} ETA {eta:>5}",
                    )
                    .unwrap()
                    .progress_chars("=>-"),
            );
            pb.enable_steady_tick(Duration::from_millis(150));
        } else {
            pb.set_draw_target(ProgressDrawTarget::hidden());
        }
        pb
    }

    pub fn add_bytes(&self, n: u64) {
        self.total_bar.inc(n);
    }

    pub fn complete_file(&self) {
        let done = self.files_done.fetch_add(1, Ordering::Relaxed) + 1;
        self.total_bar
            .set_message(format!("{}/{} files", done, self.total_files));
    }

    pub fn finish(&self) {
        if self.enabled {
            self.total_bar.finish_with_message(format!(
                "{}/{} files",
                self.files_done.load(Ordering::Relaxed),
                self.total_files
            ));
        }
    }

    pub fn println(&self, msg: &str) {
        if self.enabled {
            let _ = self.multi.println(msg);
        } else {
            println!("{msg}");
        }
    }
}
