#!/usr/bin/env bash
# Import the azcp-cluster OCI image into an enroot squashfs image.
# Run on a login node (or any host with a writable shared filesystem).
#
# Re-run when bumping versions; the version is encoded in the output
# filename so multiple versions can coexist on the same shared FS.
set -euo pipefail

VERSION="${VERSION:-v0.2.0}"
IMAGE_DIR="${IMAGE_DIR:-/shared/images}"
OUT="${IMAGE_DIR}/azcp-cluster-${VERSION}.sqsh"

mkdir -p "$IMAGE_DIR"

if [[ -f "$OUT" ]]; then
  echo "$OUT already exists; remove it to re-import." >&2
  exit 0
fi

# enroot's docker:// URI uses '#' to separate registry from path.
# ghcr.io#OWNER/IMAGE:TAG -> https://ghcr.io/v2/OWNER/IMAGE:TAG
enroot import -o "$OUT" "docker://ghcr.io#edwardsp/azcp/azcp-cluster:${VERSION}"

# `azcp-cluster.sqsh` is the version-agnostic name sbatch scripts reference.
ln -sfn "azcp-cluster-${VERSION}.sqsh" "${IMAGE_DIR}/azcp-cluster.sqsh"

echo
echo "Imported: $OUT"
echo "Symlink:  ${IMAGE_DIR}/azcp-cluster.sqsh -> azcp-cluster-${VERSION}.sqsh"
echo
echo "Next: submit examples/slurm/azcp-cluster.sbatch"
