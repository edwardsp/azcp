#!/usr/bin/env bash
# Generate the shardlist that every rank will consume.
#
# Run once on any host with credentials + azcp installed. Place the output
# on a shared filesystem (NFS, AzureFile, S3-mounted, ConfigMap...) reachable
# by every node in the job.
#
# Usage:
#   ./generate_shardlist.sh \
#       https://acct.blob.core.windows.net/models/llama3-70b/ \
#       /shared/manifests/llama3-70b.shardlist

set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <model_uri> <output_path>" >&2
  exit 2
fi

MODEL_URI="$1"
OUTPUT="$2"

mkdir -p "$(dirname "$OUTPUT")"

azcp ls "$MODEL_URI" \
  --recursive \
  --machine-readable \
  > "$OUTPUT"

N_FILES=$(grep -cv '^#' "$OUTPUT" || true)
TOTAL_BYTES=$(awk -F'\t' '!/^#/ && $2 != "<DIR>" { s += $2 } END { print s+0 }' "$OUTPUT")
TOTAL_GIB=$(awk -v b="$TOTAL_BYTES" 'BEGIN { printf "%.1f", b / 1024^3 }')

echo "wrote $OUTPUT  files=$N_FILES  total=${TOTAL_GIB} GiB"
