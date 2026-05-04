# AKS smoke test for `azcp-cluster`

End-to-end validation of `azcp-cluster` against a large model in an Azure
storage account on a 2-node AKS cluster.

## Prerequisites

- `kubectl` already configured for the target AKS cluster.
- `az` CLI logged into the subscription that owns the storage account.
- The cluster image has been built and pushed by the `cluster-image` GitHub
  workflow (or built and pushed manually). Note the tag, e.g. `edge`.
- Two nodes in the cluster have an NVMe (or fast local SSD) mounted at
  `/mnt/nvme` with at least 1.5x the dataset size free.
- The kubelet's user-assigned managed identity has **Storage Blob Data
  Reader** on the source account. Recommended over SAS: SAS tokens expire
  mid-run on long downloads, identity does not.

## 1. (Optional) Open the storage account to public network access

If the source account has `publicNetworkAccess=Disabled`, open it for the
test. Skip if the account is already reachable from the AKS node subnet
(e.g. private endpoint or service endpoint).

```bash
az storage account update \
  --name <account> \
  --public-network-access Enabled \
  --default-action Allow
```

> **Re-secure in step 6.** Do not leave a model account open after the test.

## 2. Find the kubelet's managed identity client ID

```bash
az aks show -g <rg> -n <cluster> \
  --query "identityProfile.kubeletidentity.clientId" -o tsv
```

Confirm it has `Storage Blob Data Reader` on the source account; grant if
not:

```bash
ACCOUNT_ID=$(az storage account show -n <account> --query id -o tsv)
az role assignment create \
  --assignee <kubelet-client-id> \
  --role "Storage Blob Data Reader" \
  --scope "$ACCOUNT_ID"
```

## 3. Generate an ephemeral SSH keypair for the pods

`mpirun` on rank 0 sshes into rank 1 to launch `orted`. The pods need a
shared keypair for that.

```bash
TMP=$(mktemp -d)
ssh-keygen -t ed25519 -N '' -f "$TMP/id_ed25519" -C 'azcp-cluster-test'

kubectl create secret generic azcp-cluster-test-sshkey \
  --from-file=id_ed25519="$TMP/id_ed25519" \
  --from-file=id_ed25519.pub="$TMP/id_ed25519.pub"

rm -rf "$TMP"
```

## 4. Edit the manifest and deploy

Edit `k8s/azcp-cluster-test.yaml`:

- Replace `ghcr.io/REPLACE_WITH_OWNER/azcp/azcp-cluster:edge` with the
  actual image reference produced by the `cluster-image` workflow.
- Replace `REPLACE_WITH_ACCOUNT`, `REPLACE_WITH_CONTAINER`,
  `REPLACE_WITH_PREFIX` in `SOURCE_URL` with the source location. Keep the
  trailing slash on the prefix.
- Replace `REPLACE_WITH_KUBELET_CLIENT_ID` with the value from step 2.
- Replace `REPLACE_WITH_NODEPOOL` in `nodeSelector.agentpool` with the pool
  whose nodes have `/mnt/nvme` provisioned.
- Remove the placeholder `azcp-cluster-test-sshkey` Secret block at the top
  of the file (the real one was created in step 3).

Deploy:

```bash
kubectl apply -f k8s/azcp-cluster-test.yaml
kubectl rollout status statefulset/azcp-cluster-test --timeout=5m
```

## 5. Wait for the launcher pod and verify

Pod 0 launches `mpirun` once both pods' sshd is up; pod 1 just runs sshd
and waits to receive bcast traffic.

It is normal for `azcp-cluster-test-0` to go through 1-3 `CrashLoopBackOff`
cycles at the start while it waits for `azcp-cluster-test-1`'s sshd to
become reachable. Watch:

```bash
kubectl logs -f azcp-cluster-test-0
```

Note: `mpirun` output appears only in the launching rank's container logs.
`azcp-cluster-test-1`'s logs only show sshd activity — that is expected.

Expected: a `[shard]` line per rank, then six summary lines on rank 0:

```
[list]     <files> files, <bytes> bytes T=<sec>s
[diff]     <to-transfer> to transfer, <skipped> skipped T=<sec>s
[download] <bytes> bytes T=<sec>s BW=<aggregate>
[bcast]    <files> files <bytes> bytes T=<sec>s BW=<per-receiver>
[filelist] skipped (no --save-filelist) T=<sec>s
[total]    T=<sec>s
```

Verify both pods got the full dataset:

```bash
kubectl exec azcp-cluster-test-0 -- du -sh /mnt/nvme/deepseek
kubectl exec azcp-cluster-test-1 -- du -sh /mnt/nvme/deepseek
```

Spot-check md5 on a sample file across pods:

```bash
SAMPLE=$(kubectl exec azcp-cluster-test-0 -- bash -c \
  'find /mnt/nvme/deepseek -type f | head -1')
kubectl exec azcp-cluster-test-0 -- md5sum "$SAMPLE"
kubectl exec azcp-cluster-test-1 -- md5sum "$SAMPLE"
```

The two md5s must be identical.

## 6. Teardown

```bash
kubectl delete -f k8s/azcp-cluster-test.yaml
kubectl delete secret azcp-cluster-test-sshkey

# Wipe the NVMe scratch on each node (the StatefulSet leaves it behind).
# Use `kubectl debug` to reach the host filesystem.
for node in $(kubectl get nodes -l agentpool=<pool> -o name); do
  kubectl debug "$node" --image=busybox --profile=sysadmin -it -- \
    chroot /host rm -rf /mnt/nvme/deepseek
done

# Re-secure the account if you opened it in step 1.
az storage account update \
  --name <account> \
  --public-network-access Disabled \
  --default-action Deny
```

## Pass criteria

- Both pods report identical, full-size `du -sh` output.
- The sample md5sums match across pods.
- The `[bcast]` `BW=` value (per-receiver) **exceeds** the `[download]` `BW=`
  (aggregate). This proves the fabric path is faster than per-node Azure
  egress, which is the whole point of `azcp-cluster`.

## Skip-on-rerun check (optional)

The StatefulSet's `restartPolicy: Always` means the pod restarts after
rank 0 exits 0. The second iteration should produce:

```
[diff]     0 to transfer, 524 skipped
[download] 0 bytes T=0.00s
[bcast]    0 files 0 bytes
[total]    T=<<1s
```

This validates `--compare size` correctly skips already-present files.

## Gotchas

This section captures non-obvious things learned wiring this up. The
manifest already accounts for all of these — they are documented so future
edits don't regress them.

- **`hostNetwork: true` collides with the node's real sshd on port 22.**
  The pod's in-container sshd must bind a different port (we use 2222),
  and the SSH client config + `sshd_config` must both agree on it. The
  Service's port number is just a label and can stay 22.
- **`/run/sshd` must exist** before launching `sshd -D`, otherwise it
  exits immediately with "Missing privilege separation directory". The
  entrypoint creates it.
- **`hostname` returns the node name on hostNetwork pods**, not the pod
  name, so rank detection by `hostname` is broken. The entrypoint uses a
  downward-API `POD_NAME` env instead.
- **Open MPI's ORTE handles FQDNs poorly on hostNetwork pods.** Passing
  `-hostfile` with the StatefulSet pod-FQDNs causes orted launch failures.
  The entrypoint resolves the FQDNs to IPs locally and passes
  `--host IP0:1,IP1:1 -mca routed direct` instead.
- **`orted` on the remote rank can't find Open MPI** because sshd's
  non-login shell doesn't source `/etc/profile`. `mpirun --prefix
  /opt/openmpi -x PATH -x LD_LIBRARY_PATH` fixes this. (`/opt/openmpi` is
  the install prefix in the cluster image; adjust if you change the
  Dockerfile.)
- **`mpirun` output only appears in the launching rank's logs.** Rank 1's
  pod logs only show sshd. To see what the remote rank's `azcp-cluster`
  printed, look at rank 0's logs (where mpirun multiplexes the streams).
- **Managed identity is preferred over SAS.** SAS tokens generated with
  `--auth-mode login --as-user` are capped at 24h (often less, depending
  on tenant policy) and expire mid-transfer on big datasets. Managed
  identity does not.

## Troubleshooting

- **Pods stuck in `ContainerCreating` for the SSH secret**: confirm step 3
  created `azcp-cluster-test-sshkey` in the same namespace as the
  StatefulSet.
- **`mpirun` cannot resolve pod 1's DNS name**: confirm the headless
  Service exists (`kubectl get svc azcp-cluster-test`); pod DNS resolution
  depends on it.
- **`401`/`403` on LIST**: kubelet identity is missing the
  `Storage Blob Data Reader` role on the source account, or
  `AZURE_CLIENT_ID` is wrong / pointed at an identity not attached to the
  node.
- **`hostNetwork` privilege denied**: the cluster's PodSecurityPolicy /
  PSA may forbid `hostNetwork: true`. Run in a namespace with the
  `privileged` PSA level, or remove `hostNetwork: true` (will reduce
  throughput but should still functionally work).
- **Rank 0 `CrashLoopBackOff` early on**: usually benign — rank 0 retries
  while waiting for rank 1's sshd. If it persists past ~2 minutes, check
  rank 1's logs for sshd startup errors.
