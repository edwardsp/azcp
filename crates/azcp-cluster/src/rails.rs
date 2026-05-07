//! Detect active InfiniBand HCAs from sysfs.
//!
//! `/sys/class/infiniband/<dev>/ports/<port>/state` contains a line like
//! `4: ACTIVE` when the port is up; `link_layer` is `InfiniBand` (vs
//! `Ethernet` for RoCE). We only count IB ports here because the GB300 /
//! H100-v5 multi-rail story is IB-specific. RoCE multi-rail works too but
//! has different gotchas; gate it behind a future flag if needed.
//!
//! Returned strings are formatted as `<dev>:<port>` so they can be joined
//! with commas and passed straight to `UCX_NET_DEVICES`.
//!
//! Probed before `MPI_Init`. Safe to call from a single thread.
use std::fs;
use std::path::PathBuf;

use anyhow::Result;

/// Enumerate every ACTIVE InfiniBand port on this host, in stable
/// `mlx5_0`, `mlx5_1`, ... order. Returns entries shaped `<dev>:<port>`
/// (e.g. `mlx5_0:1`) suitable for `UCX_NET_DEVICES`.
pub fn enumerate_active_ib_hcas() -> Result<Vec<String>> {
    let root = PathBuf::from("/sys/class/infiniband");
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut devs: Vec<String> = fs::read_dir(&root)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    devs.sort();

    let mut out: Vec<String> = Vec::new();
    for dev in devs {
        let ports_dir = root.join(&dev).join("ports");
        let Ok(ports_iter) = fs::read_dir(&ports_dir) else {
            continue;
        };
        let mut ports: Vec<String> = ports_iter
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        ports.sort();

        for port in ports {
            let port_dir = ports_dir.join(&port);
            let state = fs::read_to_string(port_dir.join("state"))
                .unwrap_or_default()
                .to_ascii_uppercase();
            if !state.contains("ACTIVE") {
                continue;
            }
            let link_layer = fs::read_to_string(port_dir.join("link_layer"))
                .unwrap_or_default()
                .trim()
                .to_string();
            // Empty link_layer (older kernels) → assume InfiniBand.
            if !link_layer.is_empty() && !link_layer.eq_ignore_ascii_case("InfiniBand") {
                continue;
            }
            out.push(format!("{dev}:{port}"));
        }
    }
    Ok(out)
}
