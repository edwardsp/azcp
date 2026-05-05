# Performance tuning for high-bandwidth NICs

`azcp`'s default flags saturate a 25-50 GbE link with ease. Past that, the
bottleneck stops being the wire and starts being the host's network stack,
the kernel's per-CPU softirq queues, and (on container platforms) the
overlay network sitting between your process and the NIC. This document
covers the layers you have to peel back to drive a **100-200+ Gbps** NIC
at line rate, and the recipes for both bare VMs and Kubernetes/AKS.

This is **single-process / single-node** tuning for plain `azcp`. For
multi-node broadcast, see [`docs/cluster.md`](cluster.md).

## When you need this

You only need this if:

- Your NIC is fast enough that one process can't saturate it (typically
  **100 GbE and up**).
- Your host has multiple NUMA nodes (most modern dual-socket or large
  single-socket servers).
- You are seeing wire utilization well below NIC capacity despite plenty
  of `--workers` and `--concurrency`.

On 10/25 GbE the techniques here still apply, but the gains are small —
`--workers` alone is usually enough.

## Bottleneck progression (measured)

The following was measured downloading a 385 GiB / 175-file checkpoint
from a single Azure storage account to a single node with a 200 Gbps NIC.
Each row removes one bottleneck:

| Configuration | `azcp` flags (per process) | Procs | App Gbps | Wire Gbps | NIC util |
|---|---|---:|---:|---:|---:|
| Kubernetes overlay pod (default) | `--workers 8 --concurrency 32 --block-size 16777216` | 1 | 62.7 | ~70 | 35% |
| Pod with `hostNetwork: true` | `--workers 8 --concurrency 64 --block-size 16777216` | 1 | 117 | 127 | 64% |
| `hostNetwork` + NUMA-pinned (single proc) | `numactl --cpunodebind=N --membind=N azcp ... --workers 8 --concurrency 64 --block-size 16777216` | 1 | 131 | 145 | 73% |
| `hostNetwork` + NUMA-pinned, 2 procs | `numactl ... azcp ... --workers 1 --concurrency 32 --shard i/2 --block-size 16777216` | 2 | 150 | 158 | 79% |
| `hostNetwork` + NUMA-pinned, **4 procs** | `numactl ... azcp ... --workers 1 --concurrency 32 --shard i/4 --block-size 16777216` | **4** | **170** | **186** | **93%** |

All rows used the same 385 GiB / 175-file dataset (~2.2 GiB avg file
size) with `--recursive`. `--parallel-files` was left at its default
(`min(concurrency, file_count)`). The single-proc rows benefit from
higher per-runtime concurrency (`--concurrency 64`); the multi-proc rows
split work via `--shard` and use lower per-process concurrency since
aggregate in-flight requests = `procs × concurrency`.

Net: **+103 Gbps (2.7×)** by combining `hostNetwork` (or running on bare
metal/VMs), NUMA pinning, and multiple cooperating processes.

## NUMA — what it is and why it matters

Modern multi-socket and many-core servers split CPUs and memory into
**NUMA nodes** (Non-Uniform Memory Access). Each NUMA node has its own
bank of RAM and its own slice of the PCIe bus. Memory on the *local*
node is fast to reach; memory on a *remote* node goes over an
inter-socket link and is slower.

Your NIC is physically attached to *one* NUMA node. When packets arrive,
the kernel's softirq runs on a CPU near the NIC and writes data into RAM.
If your `azcp` process happens to be running on the *other* NUMA node,
every received byte gets copied across the inter-socket link before your
code can touch it. On a 200 Gbps NIC this costs ~10-15 Gbps and noticeably
increases tail latency.

Find your NIC's NUMA node:

```bash
# Replace eth0 with your interface name (see `ip link`)
cat /sys/class/net/eth0/device/numa_node
# 1   ← NIC is on NUMA node 1
```

Then pin `azcp` to the same node using `numactl` (install via
`apt install numactl` / `dnf install numactl`):

```bash
numactl --cpunodebind=1 --membind=1 azcp copy ...
```

`--cpunodebind` restricts threads to CPUs on that node; `--membind`
restricts allocations to that node's memory. Both matter.

A single NUMA-pinned process won't saturate a fast NIC by itself — see
the next section.

## Multi-process for line rate

Even with `hostNetwork` and NUMA pinning, a single `azcp` process tops
out around 130 Gbps because tokio's scheduler and `reqwest`'s connection
pool serialize work that no amount of internal parallelism can spread.
To go further, run **multiple cooperating processes**, each pinned to the
NIC's NUMA node, each handling a deterministic shard of the file set:

```bash
# 4 cooperating processes, all on NUMA node 1, each owning 1/4 of the workload
for i in 0 1 2 3; do
  numactl --cpunodebind=1 --membind=1 \
    azcp copy https://acct.blob.core.windows.net/ctr/prefix/ ./dst/ \
      --recursive --shard $i/4 --workers 1 --concurrency 32 \
      --block-size 16777216 &
done
wait
```

`--shard i/N` makes process `i` of `N` deterministically own its slice
(size-balanced LPT bin packing — see the README's "Throughput tuning"
section). Sharding works the same whether the `N` processes run on one
node or across many.

## Bare VM recipe

```bash
# 1. Find your NIC's NUMA node
NUMA=$(cat /sys/class/net/eth0/device/numa_node)

# 2. Single high-throughput invocation (good up to ~130 Gbps single-process)
numactl --cpunodebind=$NUMA --membind=$NUMA \
  azcp copy https://acct.blob.core.windows.net/ctr/prefix/ ./dst/ \
    --recursive --workers 8 --concurrency 32 --block-size 16777216

# 3. Or fan out to multiple processes for max throughput
for i in 0 1 2 3; do
  numactl --cpunodebind=$NUMA --membind=$NUMA \
    azcp copy https://acct.blob.core.windows.net/ctr/prefix/ ./dst/ \
      --recursive --shard $i/4 --workers 1 --concurrency 32 \
      --block-size 16777216 &
done
wait
```

## Kubernetes / AKS recipe

The default pod network on most Kubernetes clusters (including AKS Azure
CNI overlay) puts every packet through a `veth` pair, a bridge, and (on
overlay) a VXLAN tunnel. The pod's `veth` typically has a single RX queue
with RPS (Receive Packet Steering) disabled, so all inbound network
softirq for the pod lands on **one CPU**. That CPU saturates around
50-70 Gbps regardless of how many workers `azcp` is using.

`hostNetwork: true` bypasses the overlay entirely — your process uses
the host's NIC directly, with all of its hardware queues and softirq
spread across many cores.

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: azcp-fast
spec:
  hostNetwork: true
  dnsPolicy: ClusterFirstWithHostNet
  containers:
  - name: azcp
    image: your/image-with-azcp-and-numactl
    command: ["bash", "-lc"]
    args:
    - |
      set -euo pipefail
      apt-get update && apt-get install -y numactl
      NUMA=$(cat /sys/class/net/eth0/device/numa_node)
      for i in 0 1 2 3; do
        numactl --cpunodebind=$NUMA --membind=$NUMA \
          azcp copy https://acct.blob.core.windows.net/ctr/prefix/ /dst/ \
            --recursive --shard $i/4 --workers 1 --concurrency 32 \
            --block-size 16777216 &
      done
      wait
```

`hostNetwork: true` is the single biggest win on AKS — typically
**+50 Gbps** over the overlay, before any other tuning. It does mean the
pod shares the host's network namespace, so coordinate port usage with
anything else running on the node.

## Account-level ceiling

Once a single node is doing 170-200 Gbps, the next ceiling you'll hit is
the **storage account egress cap** itself (~230 Gbps observed for a
standard general-purpose v2 account, with hard 503 `ServerBusy` storms
above that). Scale across multiple accounts or request a quota increase
if you need more.

For "many nodes, same dataset" workloads this is exactly where
`azcp-cluster` earns its keep — see [`cluster.md`](cluster.md).
