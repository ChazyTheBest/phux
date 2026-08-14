#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/home" "$tmp/xdg/phux" "$tmp/cwd"
printf 'this is not valid toml = [\n' > "$tmp/xdg/phux/config.toml"

assert_document() {
  name=$1
  output=$2
  first=$(sed -n '1p' "$output")
  skill_name=$(sed -n '2p' "$output")
  description=$(sed -n '3p' "$output")
  closing=$(sed -n '4p' "$output")
  first_heading=$(grep -m1 '^# ' "$output" || true)

  [ "$first" = '---' ]
  [ "$skill_name" = "name: $name" ]
  [ -n "${description#description: }" ]
  [ "$closing" = '---' ]
  [ -n "$first_heading" ]
  [ "$(tail -c 1 "$output" | od -An -tuC | tr -d ' ')" = 10 ]
}

check_binary() {
  name=$1
  bin=$2
  expected=$3
  conflict=$4
  output="$tmp/$name.skill"
  stderr="$tmp/$name.stderr"

  (
    cd "$tmp/cwd"
    HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/xdg" PHUX_SOCKET="$tmp/dead.sock" \
      "$bin" --skill > "$output" 2> "$stderr"
  )
  [ ! -s "$stderr" ]
  cmp "$output" "$expected"
  assert_document "$name" "$output"

  "$bin" -h > "$tmp/$name.help" 2> "$stderr"
  [ ! -s "$stderr" ]
  grep -q -- '--skill' "$tmp/$name.help"

  set +e
  "$bin" $conflict > "$tmp/$name.conflict.out" 2> "$stderr"
  code=$?
  set -e
  [ "$code" -eq 2 ]
  [ ! -s "$tmp/$name.conflict.out" ]
  grep -q -- '--skill' "$stderr"

  set -o pipefail
  "$bin" --skill 2> "$stderr" | head -n 1 > /dev/null
  [ ! -s "$stderr" ]
}

sed '/<!-- phux-skill-region:/d' "$root/skills/phux/SKILL.md" > "$tmp/phux.expected"
cp "$root/skills/phux-mcp/SKILL.md" "$tmp/phux-mcp.expected"

check_binary phux "$root/target/debug/phux" "$tmp/phux.expected" '--skill=quick ls'
check_binary phux-mcp "$root/target/debug/phux-mcp" "$tmp/phux-mcp.expected" '--skill --schema'

echo 'skill contract: phux and phux-mcp verified'
