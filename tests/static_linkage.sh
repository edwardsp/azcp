#!/usr/bin/env bash
# Given a portable release binary, reject any host shared-library dependency.
set -euo pipefail

binary="${1:?usage: bash tests/static_linkage.sh PATH_TO_AZCP}"
export LC_ALL=C

# Capture first so readelf errors cannot be mistaken for a passing check.
program_headers=$(readelf --program-headers --wide "$binary")
dynamic_section=$(readelf --dynamic --wide "$binary")

if [[ "$program_headers" == *INTERP* ]]; then
  echo "FAIL: $binary requires a dynamic interpreter" >&2
  exit 1
fi

if [[ "$dynamic_section" == *NEEDED* ]]; then
  echo "FAIL: $binary requires shared libraries" >&2
  echo "$dynamic_section" >&2
  exit 1
fi

echo "PASS: $binary has no dynamic interpreter or shared-library dependencies"
