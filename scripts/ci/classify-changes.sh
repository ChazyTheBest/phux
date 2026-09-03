#!/usr/bin/env bash
# Classify a newline-delimited changed-file list for the required Phux CI jobs.
# Empty input and unknown root paths fail closed into the full Phux lanes.
set -euo pipefail

mapfile -t files

docs_only=true
phux_needed=false
seen_file=false
seen_cockpit_file=false
seen_release_manifest=false

for file in "${files[@]}"; do
  [[ -n "${file}" ]] || continue
  seen_file=true

  case "${file}" in
    skills/*)
      docs_only=false
      ;;
    docs/*|ADR/*|*.md)
      ;;
    *)
      docs_only=false
      ;;
  esac

  # Cockpit owns its complete subtree and its three dedicated workflows.
  # Everything else is Phux-owned or shared and therefore runs the root lanes.
  case "${file}" in
    clients/cockpit/*|.github/workflows/cockpit-ci.yml|.github/workflows/cockpit-release.yml|.github/workflows/cockpit-sdk-head.yml)
      seen_cockpit_file=true
      ;;
    .release-please-manifest.json)
      # A Cockpit Release Please PR changes this root file alongside the
      # component subtree. Defer it so that exact shape stays Cockpit-only;
      # a manifest-only edit still fails closed below.
      seen_release_manifest=true
      ;;
    *)
      phux_needed=true
      ;;
  esac
done

if [[ "${seen_file}" == "false" ]]; then
  docs_only=false
  phux_needed=true
fi

if [[ "${seen_release_manifest}" == "true" && "${seen_cockpit_file}" == "false" ]]; then
  phux_needed=true
fi

printf 'docs_only=%s\n' "${docs_only}"
printf 'phux_needed=%s\n' "${phux_needed}"
