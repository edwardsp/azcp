# `azcp-cluster` on AKS

Two deployment patterns are supported. **MPIJob (mpi-operator) is the
recommended one** — it matches how every other distributed-training job on
AKS is launched, the launcher/worker split is explicit, and `mpirun` wiring
(SSH, host file, allocation) is handled by the operator. The StatefulSet
pattern is provided as a fallback for clusters without mpi-operator
installed.

## Pattern A — MPIJob (recommended)

### Prerequisites

- AKS cluster with a node pool that has the resources you need:
  - Local fast storage at `/mnt/nvme` (or whatever path you mount).
  - For RDMA: nodes that expose `rdma/ib: <N>` (typically the
    InfiniBand-capable VM SKUs; the resource is exposed by the
    `nvidia-network-operator` or the `rdma-shared-dp` device plugin).
  - The kubelet's user-assigned managed identity has **Storage Blob Data
    Reader** on the source storage account.
- `mpi-operator` v0.5+ installed (see
  [`examples/aks/install-mpi-operator.md`](../examples/aks/install-mpi-operator.md)).

### Manifest

A complete copy-paste example is in
[`examples/aks/mpi-operator-job.yaml`](../examples/aks/mpi-operator-job.yaml).
The shape is:

- **Launcher pod** (1 replica): runs `mpirun`, exits when the job is done.
  No fabric resources required.
- **Worker pods** (N replicas, one per node via `podAntiAffinity`):
  long-lived `sshd` containers that the launcher `ssh`'s into to start
  `azcp-cluster` ranks. Each requests `rdma/ib`, has `IPC_LOCK`, mounts
  `/dev/shm` from `Memory`, and mounts the host's `/mnt/nvme`.

### Submitting a job

```bash
kubectl apply -f examples/aks/mpi-operator-job.yaml
kubectl logs -f -l training.kubeflow.org/job-role=launcher
```

The launcher's logs contain rank 0's `[list]` / `[diff]` / `[download]` /
`[bcast]` / `[filelist]` / `[total]` summary. Worker pod logs are mostly
sshd noise (rank > 0 output is multiplexed into the launcher's stream by
`mpirun`).

### Verifying the dataset on every node

```bash
for pod in $(kubectl get pods -l training.kubeflow.org/job-role=worker -o name); do
  echo "== $pod =="
  kubectl exec "$pod" -- du -sh /mnt/nvme/dataset
done
```

All sizes should be identical and equal to the source. Spot-check a sample
file's md5 across two pods to be sure the bcast didn't corrupt anything.

### When the job ends

`MPIJob` deletes the worker pods automatically when the launcher exits 0.
The host's `/mnt/nvme/dataset` directory is **not** cleaned up — that's
the point, the dataset stays cached for whatever workload reads it next.
Wipe it manually with `kubectl debug` or a cleanup `DaemonSet` if you need
to.

## Pattern B — StatefulSet (fallback)

Use this when you can't install mpi-operator (some managed environments
don't allow new CRDs, or you're doing a one-off proof-of-concept).

The StatefulSet pattern collapses launcher and worker into a single pod
spec with `replicas: N`, `podAntiAffinity`, and an entrypoint that detects
rank 0 by `POD_NAME` and runs `mpirun` against the headless service's
pod-DNS names.

A complete example is in
[`examples/aks/statefulset-fallback.yaml`](../examples/aks/statefulset-fallback.yaml).

Tradeoffs vs MPIJob:

| | MPIJob | StatefulSet |
|---|---|---|
| Launcher / worker separation | Explicit | Same pod, branch on `POD_NAME` |
| Cleanup on completion | Workers deleted automatically | Pods stay (`restartPolicy: Always` re-runs) |
| Hostfile / SSH wiring | Operator generates it | You write the entrypoint |
| Failure surfacing | Operator updates `MPIJob.status` | You parse pod logs |
| Restart-on-success | No (job ends) | Yes (useful for "loop forever pulling latest") |

### When to pick StatefulSet

- mpi-operator is not installed and installing it isn't an option.
- You want the StatefulSet's "sticky pod identity" for some other reason
  (debugging, persistent volume affinity).
- You explicitly want the pods to keep restarting and re-running the
  transfer (e.g. a cron-style "pull every hour" pattern).

For everything else, prefer MPIJob.

## Common gotchas (apply to both patterns)

- **`hostNetwork: true` collides with the node's real sshd on port 22.**
  Run the in-pod sshd on 2222 (the example does this). The Service's
  exposed port number is just a label and can stay 22.
- **`hostname` returns the node name on hostNetwork pods.** Don't use it
  for rank detection — use the downward-API `POD_NAME` env. (MPIJob's
  worker pods set the rank via the operator-injected `OMPI_*` env, so
  this only matters on the StatefulSet pattern.)
- **Open MPI's ORTE handles FQDNs poorly on hostNetwork pods.** The
  StatefulSet entrypoint resolves pod-FQDNs to IPs and passes
  `--host IP0:1,IP1:1 -mca routed direct` instead. MPIJob does the right
  thing automatically.
- **`orted` on the remote rank can't find Open MPI** because sshd's
  non-login shell doesn't source `/etc/profile`. `mpirun --prefix
  /opt/openmpi -x PATH -x LD_LIBRARY_PATH` fixes this. (`/opt/openmpi` is
  the install prefix in the cluster image; both example manifests already
  do this.)
- **Managed identity is preferred over SAS.** SAS tokens generated with
  `--auth-mode login --as-user` are capped at 24h (often less, depending
  on tenant policy) and expire mid-transfer on big datasets. Managed
  identity does not.
- **For RDMA, `IPC_LOCK` is not optional.** Without it, `ibv_reg_mr` fails
  and UCX silently falls back to TCP. You'll see a working transfer with
  ~10× lower bcast bandwidth than expected.

## Troubleshooting

- **Pods stuck in `ContainerCreating` for the SSH secret**: confirm the
  `azcp-cluster-test-sshkey` Secret exists in the same namespace as the
  workload. (StatefulSet only — MPIJob handles SSH internally.)
- **`mpirun` cannot resolve worker DNS**: confirm the headless Service
  exists; pod DNS resolution depends on it.
- **`401`/`403` on LIST**: kubelet identity is missing the
  `Storage Blob Data Reader` role on the source account, or
  `AZURE_CLIENT_ID` is wrong / pointed at an identity not attached to the
  node.
- **`hostNetwork` privilege denied**: the cluster's PodSecurityPolicy /
  PSA may forbid `hostNetwork: true`. Run in a namespace with the
  `privileged` PSA level, or remove `hostNetwork: true` (will reduce
  throughput but should still functionally work).
- **`[bcast]` BW lower than `[download]` BW**: UCX did not pick RDMA.
  Check that `IPC_LOCK` is in `securityContext.capabilities.add`,
  `/dev/shm` is a `Memory` `emptyDir`, the pod has `rdma/ib: <N>`
  resources, and `UCX_TLS=rc,sm,self` is in the mpirun env. Run
  `ucx_info -d` inside a worker pod to confirm UCX sees the IB devices.
