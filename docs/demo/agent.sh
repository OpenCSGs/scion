#!/usr/bin/env bash
# One scripted agent for the demo: clones the repository, then for each step writes a change,
# commits and lands it. When a land conflicts, it syncs, keeps both sides of each conflict,
# resolves and lands again. Usage: agent.sh NAME COMMAND...  (one `tb` command per step)
set -u
# The server is reached directly, not through a proxy
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY
export NO_PROXY='*'
name=$1; shift
here=$(cd "$(dirname "$0")" && pwd)
work=$DEMO_WORK/$name
export SCION_TOKEN=$(cat "$DEMO_TOKENS/$name.token") SCION_CONFIG_FILE=$DEMO_WORK/$name.json
export SCION_AUTHOR="$name <$name@agentlab>"
bold=$'\e[1m' dim=$'\e[2m' cyan=$'\e[36m' green=$'\e[32m' yellow=$'\e[33m' off=$'\e[0m'

say() { printf '%s\n' "${dim}# $*${off}"; }
run() {
  local shown=() a
  for a in "$@"; do
    case $a in *" "*) shown+=("\"$a\"") ;; *) shown+=("$a") ;; esac
  done
  printf '%s %s\n' "${cyan}${bold}${name}\$${off}" "${shown[*]}"
  "$@"
}
pause() { sleep "${DEMO_PAUSE:-0.4}"; }

# Keeps both sides of every conflict (ours first), dropping the markers and the base
keep_both() {
  python3 - "$@" <<'PY'
import sys
for path in sys.argv[1:]:
    out, side = [], None
    for line in open(path):
        if line.startswith("<<<<<<<"): side = "ours"; continue
        if line.startswith("|||||||"): side = "base"; continue
        if line.startswith("=======") and side: side = "theirs"; continue
        if line.startswith(">>>>>>>"): side = None; continue
        if side != "base": out.append(line)
    open(path, "w").writelines(out)
PY
}

land() {
  local tries=0 files
  until run scn land; do
    tries=$((tries + 1))
    [ $tries -le 5 ] || return 1
    say "conflict: bring in the others' changes and keep both"
    run scn sync
    files=$(scn status --porcelain | awk '/^(UU|AA) /{print $2}')
    keep_both $files
    run scn resolve $files
    run scn commit
  done
}

cd "$DEMO_WORK"
run scn clone "$DEMO_REPO" "$name" -q
cd "$work"
# Each step adds a command (a file of its own, which the server merges when it rebases the
# commit), and the last one also notes them in CHANGELOG.md, where agents run into each other
last=${!#}
for cmd in "$@"; do
  pause
  say "add tb $cmd"
  sed "s/@CMD@/$cmd/g" "$here/command.py.in" > "tb/commands/$cmd.py"
  if [ "$cmd" = "$last" ]; then
    for c in "$@"; do
      sed -i.bak "/^## Unreleased/{n;a\\
- \`tb $c\`: added by $name.
}" CHANGELOG.md && rm CHANGELOG.md.bak
    done
  fi
  run scn status -sb
  run scn commit -m "tb $cmd"
  land
done
printf '%s\n' "${green}${bold}$name: done${off}"
