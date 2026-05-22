//! Human-readable size unit parser shared by `azcp` and `azcp-cluster`.
//!
//! Accepts plain byte counts (`16777216`), binary units (`16MiB`, `1GiB`),
//! and decimal units (`16MB`, `1GB`). The trailing `B` is optional
//! (`32Gi`, `32G` both work — same convention as `dd`).

/// Parse a size string into bytes. See module docs for accepted forms.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let raw = s.trim();
    if raw.is_empty() {
        return Err("empty size value".into());
    }
    let lower = raw.to_lowercase();
    let split = lower
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(lower.len());
    let (num_str, unit) = lower.split_at(split);
    if num_str.is_empty() {
        return Err(format!(
            "size must start with a number, got {s:?} (try e.g. 32GiB, 1GB, 16777216)"
        ));
    }
    let num: f64 = num_str
        .parse()
        .map_err(|_| format!("bad size number {num_str:?} in {s:?}"))?;
    if num < 0.0 || !num.is_finite() {
        return Err(format!("size must be non-negative and finite, got {s:?}"));
    }
    let multiplier: f64 = match unit.trim() {
        "" | "b" | "byte" | "bytes" => 1.0,
        "k" | "kb" => 1_000.0,
        "m" | "mb" => 1_000_000.0,
        "g" | "gb" => 1_000_000_000.0,
        "t" | "tb" => 1_000_000_000_000.0,
        "ki" | "kib" => 1024.0,
        "mi" | "mib" => 1024.0 * 1024.0,
        "gi" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "ti" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => {
            return Err(format!(
                "unknown size unit {other:?} in {s:?} (try GiB, MiB, GB, MB)"
            ))
        }
    };
    Ok((num * multiplier).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_human_units() {
        assert_eq!(parse_size("16MiB").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_size("1GiB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("16777216").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_size("16MB").unwrap(), 16_000_000);
        assert_eq!(parse_size("1GB").unwrap(), 1_000_000_000);
        assert_eq!(parse_size("32G").unwrap(), 32_000_000_000);
        assert_eq!(parse_size("32Gi").unwrap(), 32 * 1024 * 1024 * 1024);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_size("16XYZ").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("-1MiB").is_err());
    }
}
