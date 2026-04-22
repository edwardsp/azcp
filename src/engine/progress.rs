use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::storage::blob::client::RetryStats;

pub struct TransferProgress {
    multi: Arc<MultiProgress>,
    total_bar: ProgressBar,
    total_files: AtomicU64,
    files_done: AtomicU64,
    total_bytes: AtomicU64,
    enabled: bool,
    retry_stats: std::sync::Mutex<Option<Arc<RetryStats>>>,
    refresh_started: std::sync::atomic::AtomicBool,
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
            total_files: AtomicU64::new(total_files),
            files_done: AtomicU64::new(0),
            total_bytes: AtomicU64::new(total_bytes),
            enabled,
            retry_stats: std::sync::Mutex::new(None),
            refresh_started: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn add_total(&self, files: u64, bytes: u64) {
        self.total_files.fetch_add(files, Ordering::Relaxed);
        let new_bytes = self.total_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if self.enabled {
            self.total_bar.set_length(new_bytes.max(1));
            self.refresh_message();
        }
    }

    pub fn attach_retry_stats(self: &Arc<Self>, stats: Arc<RetryStats>) {
        if !self.enabled {
            return;
        }
        *self.retry_stats.lock().unwrap() = Some(stats.clone());
        if self
            .refresh_started
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let this = Arc::clone(self);
        // Refresh the total-bar message every 250ms so throttle counts appear
        // in near-real-time even during long single-file uploads where
        // complete_file() is not called.
        tokio::spawn(async move {
            loop {
                if this.total_bar.is_finished() {
                    break;
                }
                this.refresh_message();
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
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
        self.files_done.fetch_add(1, Ordering::Relaxed);
        self.refresh_message();
    }

    fn refresh_message(&self) {
        let done = self.files_done.load(Ordering::Relaxed);
        let total = self.total_files.load(Ordering::Relaxed);
        let base = format!("{}/{} files", done, total);
        let stats_suffix: String = self
            .retry_stats
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| format_retry_stats(s.as_ref())))
            .unwrap_or_default();
        if stats_suffix.is_empty() {
            self.total_bar.set_message(base);
        } else {
            self.total_bar.set_message(format!("{base} {stats_suffix}"));
        }
    }

    pub fn finish(&self) {
        if self.enabled {
            self.refresh_message();
            self.total_bar.finish();
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

fn format_retry_stats(s: &RetryStats) -> String {
    let t503 = s.throttle_503.load(Ordering::Relaxed);
    let t429 = s.throttle_429.load(Ordering::Relaxed);
    let t5xx = s.server_5xx.load(Ordering::Relaxed);
    let ttx = s.transport_err.load(Ordering::Relaxed);
    if t503 + t429 + t5xx + ttx == 0 {
        return String::new();
    }
    let mut parts = Vec::new();
    if t503 > 0 {
        parts.push(format!("503x{t503}"));
    }
    if t429 > 0 {
        parts.push(format!("429x{t429}"));
    }
    if t5xx > 0 {
        parts.push(format!("5xx×{t5xx}"));
    }
    if ttx > 0 {
        parts.push(format!("net×{ttx}"));
    }
    format!("[retry {}]", parts.join(" "))
}
