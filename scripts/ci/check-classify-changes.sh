#!/usr/bin/env bash
set -euo pipefail

ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
CLASSIFIER="${ROOT}/scripts/ci/classify-changes.sh"

check() {
  local name="$1" expected_docs="$2" expected_phux="$3"
  shift 3
  local output
  output="$(printf '%s\n' "$@" | "${CLASSIFIER}")"
  grep -Fxq "docs_only=${expected_docs}" <<<"${output}" || {
    printf 'error: %s docs_only mismatch\n%s\n' "${name}" "${output}" >&2
    return 1
  }
  grep -Fxq "phux_needed=${expected_phux}" <<<"${output}" || {
    printf 'error: %s phux_needed mismatch\n%s\n' "${name}" "${output}" >&2
    return 1
  }
}

check empty false true
check cockpit-source false false clients/cockpit/src/main.zig
check cockpit-doc true false clients/cockpit/README.md
check cockpit-workflow false false .github/workflows/cockpit-ci.yml
check phux-source false true crates/phux-server/src/lib.rs
check shared-ffi false true crates/phux-client-ffi/src/lib.rs
check shared-cargo false true Cargo.lock
check shared-release-config false true release-please-config.json
check manifest-only false true .release-please-manifest.json
check cockpit-release-metadata false false clients/cockpit/version.txt .release-please-manifest.json
check root-doc true true docs/RELEASING.md
check mixed false true clients/cockpit/src/main.zig crates/phux-protocol/src/lib.rs

printf 'CI change classification passed.\n'
