# AKS smoke test for `azcp-cluster`

End-to-end validation of `azcp-cluster` against the 400 GB DeepSeek model in
the `paulgb300models` storage account on a 2-node AKS cluster.

## Prerequisites

- `kubectl` already configured for the target AKS cluster.
- `az` CLI logged into the subscription that owns `paulgb300models`.
- The cluster image has been built and pushed by the `cluster-image` GitHub
  workflow (or built and pushed manually). Note the tag, e.g. `edge`.
- Two nodes in the cluster have an NVMe (or fast local SSD) mounted at
  `/mnt/nvme` with at least 500 GB free.

## 1. Open the storage account to public network access

`paulgb300models` is private by default. Open it temporarily for the test:

```bash
az storage account update \
  --name paulgb300models \
  --public-network-access Enabled
```

> **Re-disable in step 6.** Do not leave this open after the test.

## 2. Generate a SAS token for the container

Replace `<container>` with the container hosting the DeepSeek model.

```bash
EXPIRY=$(date -u -d '+2 hour' '+%Y-%m-%dT%H:%MZ')
SAS=$(az storage container generate-sas \
  --account-name paulgb300models \
  --name <container> \
  --permissions rl \
  --expiry "$EXPIRY" \
  --auth-mode login --as-user \
  -o tsv)
echo "$SAS"
```

Create a Secret holding it:

```bash
kubectl create secret generic azcp-cluster-test-sas \
  --from-literal=sas="?$SAS"
```

## 3. Generate an ephemeral SSH keypair for the pods

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

- Replace `ghcr.io/REPLACE_WITH_OWNER/azcp/azcp-cluster:edge` with the actual
  image reference produced by the `cluster-image` workflow.
- Replace `REPLACE_WITH_CONTAINER/REPLACE_WITH_PREFIX/` in `SOURCE_URL` with
  the container + prefix that holds the DeepSeek model. Keep the trailing
  slash.
- Remove the placeholder `azcp-cluster-test-sshkey` Secret block at the top
  of the file (you already created the real one in step 3).

Deploy:

```bash
kubectl apply -f k8s/azcp-cluster-test.yaml
kubectl rollout status statefulset/azcp-cluster-test --timeout=5m
```

## 5. Wait for the launcher pod and verify

Pod 0 launches `mpirun` once both pods' sshd is up; pod 1 just runs sshd and
waits to receive bcast traffic. Watch pod 0:

```bash
kubectl logs -f azcp-cluster-test-0
```

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
kubectl delete secret azcp-cluster-test-sas azcp-cluster-test-sshkey

az storage account update \
  --name paulgb300models \
  --public-network-access Disabled
```

## Pass criteria

- Both pods report identical, full-size `du -sh` output.
- The sample md5sums match across pods.
- The `[bcast]` `BW=` value (per-receiver) **exceeds** the `[download]` `BW=`
  (aggregate). This proves the fabric path is faster than per-node Azure
  egress, which is the whole point of `azcp-cluster`.

## Troubleshooting

- **Pods stuck in `ContainerCreating` for the SSH secret**: confirm step 3
  created `azcp-cluster-test-sshkey` in the same namespace as the
  StatefulSet.
- **`mpirun` cannot resolve pod 1's DNS name**: confirm the headless Service
  exists (`kubectl get svc azcp-cluster-test`); pod DNS resolution depends
  on it.
- **`401`/`403` on LIST**: SAS token expired (2 h default in step 2) or
  insufficient permissions (need `r` and `l`).
- **`hostNetwork` privilege denied**: the cluster's PodSecurityPolicy /
  PSA may forbid `hostNetwork: true`. Run in a namespace with the
  `privileged` PSA level, or remove `hostNetwork: true` (will reduce
  throughput but should still functionally work).
