pub fn human_bw(bytes: u64, secs: f64) -> String {
    if secs <= 0.0 || bytes == 0 {
        return "0 b/s".to_string();
    }
    let bps = (bytes as f64) * 8.0 / secs;
    let (val, unit) = if bps >= 1e9 {
        (bps / 1e9, "Gb/s")
    } else if bps >= 1e6 {
        (bps / 1e6, "Mb/s")
    } else if bps >= 1e3 {
        (bps / 1e3, "Kb/s")
    } else {
        (bps, "b/s")
    };
    format!("{val:.2} {unit}")
}
