#!/usr/bin/env bash
# The demo's monitor: the branch on the server and its CHANGELOG.md, redrawn each time `scn watch`
# reports that main moved.
set -u
# The server is reached directly, not through a proxy
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY
export NO_PROXY='*'
export SCION_TOKEN=$(cat "$DEMO_TOKENS/monitor.token") SCION_CONFIG_FILE=$DEMO_WORK/monitor.json
draw() {
  scn cat main:CHANGELOG.md --repo "$DEMO_REPO" > "$DEMO_WORK/monitor-changelog" 2>/dev/null
  out=$(scn log main --repo "$DEMO_REPO" -n "${DEMO_MONITOR_COMMITS:-20}" --json 2>/dev/null \
    | python3 "$(dirname "$0")/monitor.py" "${DEMO_BASE:-}" "$DEMO_WORK/monitor-changelog")
  # Redraw in place: home, each line cleared to its end, then the rest of the screen
  printf '\e[H'
  printf '%s\n' "$out" | while IFS= read -r line; do printf '%s\e[K\n' "$line"; done
  printf '\e[J'
}
draw
# Redraw whenever the server reports that main moved
scn watch main --repo "$DEMO_REPO" --json 2>/dev/null | while read -r _; do draw; done
