# Examples

Copy-paste deployment manifests and submit scripts for `azcp-cluster`. All
examples consume the same multi-arch container image
(`ghcr.io/edwardsp/azcp/azcp-cluster:vX.Y.Z`) — there are no per-platform
binaries.

## AKS

| File | Purpose |
|---|---|
| [`aks/install-mpi-operator.md`](aks/install-mpi-operator.md) | Install + verify the Kubeflow MPI Operator (one-time per cluster). |
| [`aks/mpi-operator-job.yaml`](aks/mpi-operator-job.yaml) | **Standalone download.** `MPIJob` (Launcher + Worker) running `azcp-cluster` across N nodes with RDMA. Use when you just want to populate per-node NVMe and run the downstream job by hand. |
| [`aks/mpijob-download-then-train.yaml`](aks/mpijob-download-then-train.yaml) | **Production pattern.** Single `MPIJob` with a two-phase launcher: phase 1 runs `azcp-cluster` to populate per-node NVMe, phase 2 runs your training/inference workload on the same pods against the cached data (e.g. `torchrun` launched under `mpirun` as a process launcher). Requires a merged image — see `Dockerfile.training`. Bottom-of-file comments cover scheduling alternatives (initContainer pattern, Argo, Volcano, JobSet+Kueue) for the can't-merge case. |
| [`aks/Dockerfile.training`](aks/Dockerfile.training) | Build the merged image used by `mpijob-download-then-train.yaml`. Starts `FROM` your existing training image (default: NGC PyTorch) and `COPY --from=ghcr.io/edwardsp/azcp/azcp-cluster:vX.Y.Z` the Open MPI + UCX stack and `azcp-cluster` binary into `/opt/azcp/` (namespaced to coexist with HPC-X), plus `openssh-server` for the MPI launcher. |
| [`aks/init-container-download-then-train.yaml`](aks/init-container-download-then-train.yaml) | **Pure-Kubernetes alternative to the MPIJob path.** Plain `batch/v1.Job` (Indexed, parallelism=N) + headless Service. An `initContainer` runs `azcp-cluster` to populate per-node NVMe via RDMA broadcast; the main container then runs training in the **same pod** against the cached data — co-location is automatic, no operator dependency, scheduler picks the nodes. Optionally layer Kueue on the Job for gang admission with a single label. End-to-end validated on GB300 with a real 385 GiB DeepSeek-R1 cache (66s, 4 nodes). |
| [`aks/statefulset-fallback.yaml`](aks/statefulset-fallback.yaml) | Fallback for clusters without mpi-operator. Same outcome as the standalone download, more wiring. |

Background and tradeoffs: [`docs/cluster-aks.md`](../docs/cluster-aks.md).

## Slurm

| File | Purpose |
|---|---|
| [`slurm/enroot-import.sh`](slurm/enroot-import.sh) | One-time: import the OCI image into an enroot squashfs on the shared filesystem. |
| [`slurm/azcp-cluster.sbatch`](slurm/azcp-cluster.sbatch) | **Standalone download.** sbatch script using `srun --container-image=...` (enroot + pyxis). |
| [`slurm/download-then-train.sbatch`](slurm/download-then-train.sbatch) | **Production pattern.** Single sbatch with two `srun` steps: stage 1 runs `azcp-cluster` to populate per-node NVMe; stage 2 runs your training/inference workload (different image) on the same allocation against the cached data. |
| [`slurm/apptainer.sbatch`](slurm/apptainer.sbatch) | Alternative for sites without pyxis. Uses `apptainer exec`. |

Background and tradeoffs: [`docs/cluster-slurm.md`](../docs/cluster-slurm.md).
