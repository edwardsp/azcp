#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/azcp-cluster"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if [[ ! -x "$BIN" ]]; then
  echo "building azcp-cluster (release)..." >&2
  cargo build --release -p azcp-cluster
fi

cat > "$WORK/files.tsv" <<EOF
foo/a.bin	1024	2024-01-01T00:00:00Z
foo/b.bin	2048	2024-01-02T00:00:00Z
EOF

mkdir -p "$WORK/match/foo" "$WORK/empty"
truncate -s 1024 "$WORK/match/foo/a.bin"
truncate -s 2048 "$WORK/match/foo/b.bin"

echo "==> 1. --stage init prints hello and exits 0"
mpirun -np 1 "$BIN" --stage init dummy:// "$WORK/empty" 2>&1 | grep -q "hello rank 0/1"

echo "==> 2. multi-rank-per-node aborts with exit 2"
set +e
mpirun -np 2 --oversubscribe "$BIN" --stage init dummy:// "$WORK/empty" >/dev/null 2>&1
rc=$?
set -e
[[ "$rc" -eq 2 ]] || { echo "expected exit 2, got $rc"; exit 1; }

echo "==> 3. --stage list reads shardlist and bcasts"
out=$(mpirun -np 1 "$BIN" --stage list --shardlist "$WORK/files.tsv" dummy:// "$WORK/empty" 2>&1)
echo "$out" | grep -q "\[list\] 2 files, 3072 bytes"

echo "==> 4. --compare size skips when local files match"
out=$(mpirun -np 1 "$BIN" --shardlist "$WORK/files.tsv" --compare size --save-filelist "$WORK/out.tsv" dummy:// "$WORK/match" 2>&1)
echo "$out" | grep -q "\[diff\] 0 to transfer, 2 skipped"

echo "==> 5. exactly six summary lines, in order"
six=$(echo "$out" | grep -E "^\[(list|diff|download|bcast|filelist|total)\] ")
count=$(echo "$six" | wc -l)
[[ "$count" -eq 6 ]] || { echo "expected 6 summary lines, got $count:"; echo "$six"; exit 1; }
order=$(echo "$six" | awk '{print $1}' | tr '\n' ' ')
[[ "$order" == "[list] [diff] [download] [bcast] [filelist] [total] " ]] || {
  echo "wrong order: $order"; exit 1
}

echo "==> 6. --save-filelist round-trips the listing"
diff -q <(sort "$WORK/files.tsv") <(sort "$WORK/out.tsv")

echo "all smoke checks passed"
