#!/usr/bin/env bash
# Only gstcactus may embed the Cactus engine, so it ships once, and
# `links = "cactus"` does not stop two crates from depending on cactus-sys.
# Fail when anything else reaches it through normal dependencies.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

allowed=" cactus-sys cactus gst-plugin-cactus "
dependents="$(cargo tree --locked --workspace -i cactus-sys -e normal --prefix none --format '{p}' |
  awk '{ print $1 }' | sort -u)"
status=0
for package in $dependents; do
  case "$allowed" in
    *" $package "*) ;;
    *)
      printf 'error: %s links the Cactus engine; only gst-plugin-cactus may\n' "$package" >&2
      status=1
      ;;
  esac
done
exit "$status"
