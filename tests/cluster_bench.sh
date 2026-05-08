#!/usr/bin/env bash
# tests/cluster_bench.sh
#
# Reproducible parameter sweep for `azcp-cluster` bcast performance.
# Submits a configurable number of runs against a live AKS cluster (one
# StatefulSet per config), parses the [bcast]/[download]/[total] lines
# from rank-0 logs, and prints a markdown results table.
#
# Prerequisites:
#   - kubectl configured for the target cluster.
#   - The SSH-keypair Secret `azcp-cluster-bench-sshkey` exists in the
#     target namespace (create with the snippet in the StatefulSet
#     example: `examples/aks/statefulset-fallback.yaml`).
#   - Source storage account is reachable from the node pool, with the
#     kubelet's managed identity granted Storage Blob Data Reader.
#   - The node pool has IB resources (`rdma/ib`) and NVMe at /mnt/nvme.
#
# Required env:
#   SOURCE_URL          Blob prefix to broadcast (e.g. https://acct.blob.../ctr/prefix/)
#   IMAGE               Container image (e.g. ghcr.io/edwardsp/azcp/azcp-cluster:v0.3.0)
#   AZURE_CLIENT_ID     Kubelet's user-assigned managed identity client ID
#   NODEPOOL            Agent pool name (nodeSelector.agentpool)
#
# Optional env:
#   REPLICAS            Number of nodes (default: 16)
#   REPS                Repetitions per config (default: 3)
#   NAMESPACE           K8s namespace (default: default)
#   DEST                Per-pod destination path (default: /mnt/nvme/dataset)
#   CONFIGS             Newline-separated "label;chunk;pipeline;tls" tuples.
#                       Default exercises the standard sweep.
#   COMPARE             azcp-cluster --compare value (default: none, for cold runs)
#
# Usage:
#   SOURCE_URL=https://...blob.core.windows.net/models/foo/ \
#   IMAGE=ghcr.io/edwardsp/azcp/azcp-cluster:v0.3.0 \
#   AZURE_CLIENT_ID=... \
#   NODEPOOL=gb300 \
#     ./tests/cluster_bench.sh > bench-results.md

set -euo pipefail

: "${SOURCE_URL:?SOURCE_URL required}"
: "${IMAGE:?IMAGE required}"
: "${AZURE_CLIENT_ID:?AZURE_CLIENT_ID required}"
: "${NODEPOOL:?NODEPOOL required}"

REPLICAS="${REPLICAS:-16}"
REPS="${REPS:-3}"
NAMESPACE="${NAMESPACE:-default}"
DEST="${DEST:-/mnt/nvme/dataset}"
COMPARE="${COMPARE:-none}"
MEM_LIMIT="${MEM_LIMIT:-64Gi}"
SHM_SIZE="${SHM_SIZE:-}"

DEFAULT_CONFIGS='tcp;67108864;4;tcp
rdma-default;67108864;4;rc,sm,self
rdma-256m-pipe8;268435456;8;rc,sm,self
rdma-64m-pipe2;67108864;2;rc,sm,self'

CONFIGS="${CONFIGS:-$DEFAULT_CONFIGS}"

# Each rendered StatefulSet uses a unique name so concurrent benches don't
# collide and delete-on-completion is unambiguous.
# Short to keep total StatefulSet name (+11-char controller-revision-hash
# suffix) under the 63-char K8s label limit.
RUN_ID="b$(date +%s | tail -c 7)$$"

cleanup() {
  echo "# cleaning up StatefulSets matching ${RUN_ID}-*" >&2
  kubectl -n "$NAMESPACE" delete statefulset,service,configmap \
    -l "azcp-cluster-bench=$RUN_ID" --ignore-not-found --wait=false >&2 || true
}
trap cleanup EXIT

# Render and submit one StatefulSet, wait for rank-0 to finish (=mpirun
# exits and entrypoint sleeps forever), capture rank-0 logs, parse the
# stage lines. Caller passes label, chunk, pipeline, ucx_tls.
run_one() {
  local label="$1" chunk="$2" pipeline="$3" tls="$4" rep="$5"
  local name="azcp-bench-${RUN_ID}-${label}-${rep}"
  local mpirun_extra=""
  if [[ "$tls" == "tcp" ]]; then
    mpirun_extra='-mca pml ob1 -mca btl tcp,self -mca btl_tcp_if_include eth0'
  else
    mpirun_extra="-mca pml ucx -mca osc ucx -x UCX_TLS=${tls} -x UCX_NET_DEVICES=mlx5_0:1,mlx5_1:1,mlx5_2:1,mlx5_3:1 -x UCX_IB_GID_INDEX=3"
  fi

  cat <<EOF | kubectl -n "$NAMESPACE" apply -f - >&2
---
apiVersion: v1
kind: Service
metadata:
  name: ${name}
  labels: { azcp-cluster-bench: ${RUN_ID} }
spec:
  clusterIP: None
  selector: { app: ${name} }
  ports: [{ name: ssh, port: 22 }]
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: ${name}-cfg
  labels: { azcp-cluster-bench: ${RUN_ID} }
data:
  entrypoint.sh: |
    #!/usr/bin/env bash
    set -euo pipefail
    SVC="${name}"
    rm -rf ${DEST}
    mkdir -p ${DEST}
    install -m 0700 -d /root/.ssh
    install -m 0600 /etc/sshkey/id_ed25519     /root/.ssh/id_ed25519
    install -m 0644 /etc/sshkey/id_ed25519.pub /root/.ssh/id_ed25519.pub
    install -m 0600 /etc/sshkey/id_ed25519.pub /root/.ssh/authorized_keys
    cat > /root/.ssh/config <<E
    Host *
      StrictHostKeyChecking no
      UserKnownHostsFile /dev/null
      LogLevel ERROR
      Port 2222
    E
    ssh-keygen -A
    install -d -m 0755 /run/sshd
    sed -i 's/^#\?Port .*/Port 2222/' /etc/ssh/sshd_config
    /usr/sbin/sshd -D -e &
    SSHD_PID=\$!
    if [[ "\${POD_NAME:-\$(hostname)}" != "\${SVC}-0" ]]; then
      wait "\$SSHD_PID"; exit 0
    fi
    for i in \$(seq 0 $((REPLICAS-1))); do
      until ssh -o ConnectTimeout=2 "\${SVC}-\${i}.\${SVC}" true 2>/dev/null; do
        sleep 2
      done
    done
    HOSTS=""
    for i in \$(seq 0 $((REPLICAS-1))); do
      ip=\$(getent hosts "\${SVC}-\${i}.\${SVC}" | awk 'NR==1{print \$1}')
      if [[ -z "\$HOSTS" ]]; then HOSTS="\${ip}:1"; else HOSTS="\${HOSTS},\${ip}:1"; fi
    done
    mpirun --host "\$HOSTS" -np ${REPLICAS} \\
      --allow-run-as-root --prefix /opt/openmpi \\
      -mca plm_rsh_agent ssh \\
      ${mpirun_extra} \\
      -mca oob_tcp_if_include eth0 \\
      -mca routed direct \\
      -x PATH -x LD_LIBRARY_PATH \\
      -x AZURE_CLIENT_ID \\
      azcp-cluster "${SOURCE_URL}" ${DEST} \\
        --concurrency 32 --block-size 16777216 \\
        --bcast-chunk ${chunk} --bcast-pipeline ${pipeline} \\
        ${BCAST_EXTRA_ARGS:-} \\
        --compare ${COMPARE} \\
        --no-progress
    echo "=== BENCH_DONE ==="
    sleep infinity
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: ${name}
  labels: { azcp-cluster-bench: ${RUN_ID} }
spec:
  serviceName: ${name}
  replicas: ${REPLICAS}
  podManagementPolicy: Parallel
  selector: { matchLabels: { app: ${name} } }
  template:
    metadata: { labels: { app: ${name} } }
    spec:
      hostNetwork: true
      dnsPolicy: ClusterFirstWithHostNet
      restartPolicy: Always
      nodeSelector: { agentpool: ${NODEPOOL} }
      tolerations: [{ operator: Exists }]
      affinity:
        podAntiAffinity:
          requiredDuringSchedulingIgnoredDuringExecution:
            - labelSelector: { matchLabels: { app: ${name} } }
              topologyKey: kubernetes.io/hostname
      containers:
        - name: c
          image: ${IMAGE}
          imagePullPolicy: IfNotPresent
          command: ["/bin/bash", "/etc/mpi/entrypoint.sh"]
          securityContext: { capabilities: { add: ["IPC_LOCK"] } }
          env:
            - name: POD_NAME
              valueFrom: { fieldRef: { fieldPath: metadata.name } }
            - name: AZURE_CLIENT_ID
              value: "${AZURE_CLIENT_ID}"
          volumeMounts:
            - { name: nvme, mountPath: /mnt/nvme }
            - { name: cfg,  mountPath: /etc/mpi }
            - { name: sshkey, mountPath: /etc/sshkey, readOnly: true }
            - { name: shm,  mountPath: /dev/shm }
          resources:
            requests: { cpu: "8", memory: "16Gi", rdma/ib: 4 }
            limits:   { memory: "${MEM_LIMIT}", rdma/ib: 4 }
      volumes:
        - { name: nvme,   hostPath: { path: /mnt/nvme, type: DirectoryOrCreate } }
        - { name: cfg,    configMap: { name: ${name}-cfg, defaultMode: 0o755 } }
        - { name: sshkey, secret: { secretName: azcp-cluster-bench-sshkey, defaultMode: 0o600 } }
        - { name: shm,    emptyDir: { medium: Memory${SHM_SIZE:+, sizeLimit: ${SHM_SIZE}} } }
EOF

  echo "# waiting for ${name}-0 to print BENCH_DONE..." >&2
  local deadline=$((SECONDS + 1800))
  while (( SECONDS < deadline )); do
    if kubectl -n "$NAMESPACE" logs "${name}-0" 2>/dev/null | grep -q "BENCH_DONE"; then
      break
    fi
    sleep 10
  done
  if (( SECONDS >= deadline )); then
    echo "# TIMEOUT for ${name}" >&2
    kubectl -n "$NAMESPACE" delete statefulset "$name" --ignore-not-found --wait=false >&2 || true
    return 1
  fi

  local logs
  logs=$(kubectl -n "$NAMESPACE" logs "${name}-0" 2>/dev/null)
  local download bcast total verify
  download=$(awk '/^\[download\]/{for(i=1;i<=NF;i++)if($i~/^BW=/){print $i}}' <<<"$logs" | sed 's/BW=//' | head -1)
  bcast=$(awk    '/^\[bcast\]/   {for(i=1;i<=NF;i++)if($i~/^BW=/){print $i}}' <<<"$logs" | sed 's/BW=//' | head -1)
  total=$(awk    '/^\[total\]/   {for(i=1;i<=NF;i++)if($i~/^T=/){print $i}}'  <<<"$logs" | sed 's/T=//;s/s$//' | head -1)
  verify=$(grep '^\[verify\]' <<<"$logs" | head -1 | sed 's/^\[verify\] //')
  if [[ -n "$verify" ]]; then
    echo "VERIFY: $verify" >&2
  fi
  echo "${label}|${chunk}|${pipeline}|${tls}|${rep}|${download:-?}|${bcast:-?}|${total:-?}"

  kubectl -n "$NAMESPACE" delete statefulset,service,configmap "$name" "${name}-cfg" --ignore-not-found --wait=false >&2 || true
}

declare -a results
while IFS=';' read -r label chunk pipeline tls; do
  [[ -z "$label" ]] && continue
  for rep in $(seq 1 "$REPS"); do
    if line=$(run_one "$label" "$chunk" "$pipeline" "$tls" "$rep"); then
      results+=("$line")
    fi
  done
done <<<"$CONFIGS"

# Markdown table to stdout.
cat <<MD
# azcp-cluster benchmark results

- Replicas: ${REPLICAS}
- Repetitions per config: ${REPS}
- Source: ${SOURCE_URL}
- Image: ${IMAGE}
- Compare policy: ${COMPARE}
- Run ID: ${RUN_ID}

| Config | bcast-chunk | pipeline | UCX_TLS | rep | [download] BW | [bcast] BW | [total] |
|---|---:|---:|---|---:|---:|---:|---:|
MD
for r in "${results[@]}"; do
  IFS='|' read -r label chunk pipeline tls rep download bcast total <<<"$r"
  printf "| %s | %s | %s | %s | %s | %s | %s | %ss |\n" \
    "$label" "$chunk" "$pipeline" "$tls" "$rep" "$download" "$bcast" "$total"
done
