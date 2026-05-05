# Examples

Copy-paste deployment manifests and submit scripts for `azcp-cluster`. All
examples consume the same multi-arch container image
(`ghcr.io/edwardsp/azcp/azcp-cluster:vX.Y.Z`) — there are no per-platform
binaries.

## AKS

| File | Purpose |
|---|---|
| [`aks/install-mpi-operator.md`](aks/install-mpi-operator.md) | Install + verify the Kubeflow MPI Operator (one-time per cluster). |
| [`aks/mpi-operator-job.yaml`](aks/mpi-operator-job.yaml) | **Recommended.** `MPIJob` (Launcher + Worker) running `azcp-cluster` across N nodes with RDMA. |
| [`aks/statefulset-fallback.yaml`](aks/statefulset-fallback.yaml) | Fallback for clusters without mpi-operator. Same outcome, more wiring. |

Background and tradeoffs: [`docs/cluster-aks.md`](../docs/cluster-aks.md).

## Slurm

| File | Purpose |
|---|---|
| [`slurm/enroot-import.sh`](slurm/enroot-import.sh) | One-time: import the OCI image into an enroot squashfs on the shared filesystem. |
| [`slurm/azcp-cluster.sbatch`](slurm/azcp-cluster.sbatch) | **Recommended.** sbatch script using `srun --container-image=...` (enroot + pyxis). |
| [`slurm/apptainer.sbatch`](slurm/apptainer.sbatch) | Alternative for sites without pyxis. Uses `apptainer exec`. |

Background and tradeoffs: [`docs/cluster-slurm.md`](../docs/cluster-slurm.md).
