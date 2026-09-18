#!/usr/bin/env bash
# Fails unless every given .so carries the expected SBPF bytecode version.
#
# A v0 artifact builds and deploys fine today and becomes un-upgradable once
# SIMD-0500 activates, so anything that bypasses build-sbf.sh must fail loudly
# rather than ship.
#
# SIMD-0161 stores the version in the ELF header's e_flags field, which sits at
# byte 48 of an ELF64 header and is 4 bytes little-endian. Reading it with od
# keeps this dependency-free; llvm-readelf renders the same field as "Flags".
#
# Usage: assert-sbpf-version.sh <file.so> [more.so ...]
#        SBPF_ARCH=v3 assert-sbpf-version.sh ...   (default v3)
set -euo pipefail

WANT="${SBPF_ARCH:-v3}"
WANT="${WANT#v}"

if [ $# -eq 0 ]; then
  echo "usage: assert-sbpf-version.sh <file.so> [more.so ...]" >&2
  exit 2
fi

status=0
for so in "$@"; do
  if [ ! -f "$so" ]; then
    echo "ERROR: $so does not exist" >&2
    status=1
    continue
  fi
  got=$(od -An -t u4 -j 48 -N 4 "$so" | tr -d '[:space:]')
  if [ "$got" != "$WANT" ]; then
    echo "ERROR: $so is SBPF v${got}, expected v${WANT}" >&2
    echo "       Something built this outside deploy-scripts/build-sbf.sh." >&2
    status=1
  else
    echo "  ok: $so is SBPF v${got}"
  fi
done
exit $status
