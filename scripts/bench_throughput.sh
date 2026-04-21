#!/usr/bin/env bash
# Throughput benchmark for azcp against a live Azure Blob container.
#
# Generates large files on a fast local filesystem, runs `azcp copy` with a
# sweep of --concurrency (and optionally --block-size) values, reports
# observed throughput, and flags any throttling responses seen in stderr.
#
# Required env:
#   AZCP_TEST_ACCOUNT     storage account name
#   AZCP_TEST_CONTAINER   container name (must already exist)
#   AZURE_STORAGE_KEY     account key (or use another auth mechanism azcp supports)
#
# Optional env:
#   AZCP_BIN              path to azcp binary          (default: azcp in PATH)
#   BENCH_WORKDIR         where to create test files    (default: /mnt/nvme/azcp-bench)
#   BENCH_FILE_SIZE_MB    per-file size in MiB          (default: 1024)
#   BENCH_FILE_COUNT      number of files               (default: 8)
#   BENCH_BLOCK_SIZE_MB   --block-size in MiB           (default: 8)
#   BENCH_CONCURRENCY     space-separated sweep values  (default: "8 16 32 64 128 256")
#   BENCH_BLOCK_SWEEP     space-separated MB values     (default: "" = no block sweep)
#   BENCH_KEEP_FILES      if set, do not delete local generated files
#
# Output: a results table (plain text) to stdout. Full per-run logs in $BENCH_WORKDIR/logs/.

set -euo pipefail

: "${AZCP_TEST_ACCOUNT:?Set AZCP_TEST_ACCOUNT}"
: "${AZCP_TEST_CONTAINER:?Set AZCP_TEST_CONTAINER}"

AZCP_BIN="${AZCP_BIN:-azcp}"
BENCH_WORKDIR="${BENCH_WORKDIR:-/mnt/nvme/azcp-bench}"
BENCH_FILE_SIZE_MB="${BENCH_FILE_SIZE_MB:-1024}"
BENCH_FILE_COUNT="${BENCH_FILE_COUNT:-8}"
BENCH_BLOCK_SIZE_MB="${BENCH_BLOCK_SIZE_MB:-8}"
BENCH_CONCURRENCY="${BENCH_CONCURRENCY:-8 16 32 64 128 256}"
BENCH_BLOCK_SWEEP="${BENCH_BLOCK_SWEEP:-}"

DATA_DIR="$BENCH_WORKDIR/data"
LOG_DIR="$BENCH_WORKDIR/logs"
mkdir -p "$DATA_DIR" "$LOG_DIR"

total_bytes=$((BENCH_FILE_SIZE_MB * BENCH_FILE_COUNT * 1024 * 1024))
total_gib=$(awk "BEGIN{printf \"%.2f\", $total_bytes/1073741824}")

echo "=== azcp throughput benchmark ==="
echo "account      : $AZCP_TEST_ACCOUNT"
echo "container    : $AZCP_TEST_CONTAINER"
echo "workdir      : $BENCH_WORKDIR"
echo "files        : $BENCH_FILE_COUNT x ${BENCH_FILE_SIZE_MB} MiB = ${total_gib} GiB"
echo "block size   : ${BENCH_BLOCK_SIZE_MB} MiB (default, may be swept)"
echo "concurrency  : $BENCH_CONCURRENCY"
echo "block sweep  : ${BENCH_BLOCK_SWEEP:-<none>}"
echo "azcp         : $AZCP_BIN ($($AZCP_BIN --version 2>/dev/null || echo unknown))"
echo

# Generate files once, reuse across runs. Use /dev/urandom-seeded then pad with
# zeros for speed — we're measuring network/disk, not entropy.
echo "=== Generating test files in $DATA_DIR ==="
for i in $(seq 1 "$BENCH_FILE_COUNT"); do
  f="$DATA_DIR/file_${i}.bin"
  if [[ ! -f "$f" ]] || [[ "$(stat -c%s "$f" 2>/dev/null || echo 0)" -ne $((BENCH_FILE_SIZE_MB * 1024 * 1024)) ]]; then
    echo "  creating $f (${BENCH_FILE_SIZE_MB} MiB)"
    # fallocate is instant on ext4/xfs; fall back to dd if not supported.
    fallocate -l "${BENCH_FILE_SIZE_MB}MiB" "$f" 2>/dev/null \
      || dd if=/dev/zero of="$f" bs=1M count="$BENCH_FILE_SIZE_MB" status=none
  else
    echo "  reuse    $f"
  fi
done
sync
echo

dest_base="https://${AZCP_TEST_ACCOUNT}.blob.core.windows.net/${AZCP_TEST_CONTAINER}/azcp-bench"

run_one() {
  local label="$1" conc="$2" bsz_mb="$3"
  local bsz=$((bsz_mb * 1024 * 1024))
  local prefix="run-$(date +%s%N)"
  local dest="$dest_base/$prefix/"
  local log="$LOG_DIR/${label}.log"

  "$AZCP_BIN" rm "$dest" --recursive >/dev/null 2>&1 || true

  # Drop caches is not possible without root inside the pod; accept warm cache.
  local t0 t1 dur
  t0=$(date +%s.%N)
  set +e
  "$AZCP_BIN" copy "$DATA_DIR" "$dest" \
    --recursive --concurrency "$conc" --block-size "$bsz" \
    >"$log" 2>&1
  local rc=$?
  set -e
  t1=$(date +%s.%N)
  dur=$(awk "BEGIN{printf \"%.2f\", $t1-$t0}")

  local mib_s gib_s gbps throttled
  mib_s=$(awk "BEGIN{printf \"%.1f\", $total_bytes/1048576/$dur}")
  gib_s=$(awk "BEGIN{printf \"%.2f\", $total_bytes/1073741824/$dur}")
  gbps=$(awk "BEGIN{printf \"%.2f\", $total_bytes*8/1000000000/$dur}")
  throttled=$(grep -cE "503|ServerBusy|slowdown|throttl" "$log" || true)

  printf "%-28s  conc=%-4s  blk=%-3s MiB  %7ss   %9s MiB/s   %6s GiB/s   %6s Gbps  rc=%s  throttle=%s\n" \
    "$label" "$conc" "$bsz_mb" "$dur" "$mib_s" "$gib_s" "$gbps" "$rc" "$throttled"

  "$AZCP_BIN" rm "$dest" --recursive >/dev/null 2>&1 || true
}

echo "=== Concurrency sweep (block-size=${BENCH_BLOCK_SIZE_MB} MiB) ==="
for c in $BENCH_CONCURRENCY; do
  run_one "conc-${c}" "$c" "$BENCH_BLOCK_SIZE_MB"
done
echo

if [[ -n "$BENCH_BLOCK_SWEEP" ]]; then
  # shellcheck disable=SC2206
  conc_arr=($BENCH_CONCURRENCY)
  mid_c="${conc_arr[$(( ${#conc_arr[@]} / 2 ))]}"
  echo "=== Block-size sweep (concurrency=${mid_c}) ==="
  for b in $BENCH_BLOCK_SWEEP; do
    run_one "blk-${b}" "$mid_c" "$b"
  done
  echo
fi

echo "=== Done. Per-run logs in $LOG_DIR ==="
if [[ -z "${BENCH_KEEP_FILES:-}" ]]; then
  echo "Removing generated files (set BENCH_KEEP_FILES=1 to keep)"
  rm -rf "$DATA_DIR"
fi
