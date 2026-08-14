#!/bin/sh
set -u

[ "$#" -eq 1 ] || exit 2
[ -n "${PHUX_TERMINAL_ID:-}" ] || exit 0

phux=${PHUX_AGENT_PHUX_BIN:-phux}
target="@$PHUX_TERMINAL_ID"

case "$1" in
  start) "$phux" agent set "$target" --name claude --kind claude >/dev/null 2>&1 || true ;;
  blocked) "$phux" ask "$target" "Claude needs attention" >/dev/null 2>&1 || true ;;
  clear) "$phux" agent clear "$target" >/dev/null 2>&1 || true ;;
  *) exit 2 ;;
esac
