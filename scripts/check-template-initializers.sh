#!/usr/bin/env bash
set -euo pipefail

failed=0
for contract in templates/*/src/lib.rs; do
  if ! grep -Eq 'pub fn (initialize|init)\(' "$contract"; then
    continue
  fi

  body=$(awk '
    /pub fn (initialize|init)\(/ { capture=1 }
    capture && seen && /pub fn / { exit }
    capture { print; seen=1 }
  ' "$contract")

  if ! grep -Eq 'storage\(\).*\.has|already initialized' <<<"$body"; then
    echo "unsafe setup entrypoint: $contract can be called more than once" >&2
    failed=1
  fi
done

exit "$failed"
