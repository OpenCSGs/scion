#!/usr/bin/env bash
# The Scion demo: four agents change one repository at once while a monitor shows the branch on
# the server. Needs a server, a repository to fork, and tokens in $DEMO_TOKENS (agent-1..4,
# monitor, and admin to fork). See README.md in this directory.
#   DEMO_SERVER=http://host:7080 DEMO_TOKENS=dir docs/demo/run.sh
set -eu
unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY
export NO_PROXY='*'
here=$(cd "$(dirname "$0")" && pwd)
: "${DEMO_SERVER:?}" "${DEMO_TOKENS:?}"
export DEMO_TOKENS SCION_NO_DAEMON=1
export DEMO_WORK=$(mktemp -d /tmp/scion-demo.XXXX)
export DEMO_REPO=$DEMO_SERVER/agentlab/toolbox-demo-$(date +%H%M%S)
SCION_TOKEN=$(cat "$DEMO_TOKENS/admin.token") SCION_CONFIG_FILE=$DEMO_WORK/admin.json \
  scn repo fork "$DEMO_SERVER/${DEMO_SOURCE:-agentlab/toolbox}" "$DEMO_REPO" -q >/dev/null
export DEMO_BASE=$(SCION_TOKEN=$(cat "$DEMO_TOKENS/admin.token") SCION_CONFIG_FILE=$DEMO_WORK/admin.json \
  scn log main --repo "$DEMO_REPO" -n 1 --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["entries"][0]["id"])')

s=scion-demo
tmux kill-session -t $s 2>/dev/null || true
tmux new-session -d -s $s -x "${DEMO_COLS:-200}" -y "${DEMO_ROWS:-50}" \
  -e DEMO_TOKENS="$DEMO_TOKENS" -e DEMO_WORK="$DEMO_WORK" -e DEMO_REPO="$DEMO_REPO" -e DEMO_BASE="$DEMO_BASE" \
  -e SCION_NO_DAEMON=1 -e DEMO_PAUSE="${DEMO_PAUSE:-0.6}"
tmux set -t $s status off
tmux set -t $s pane-border-status top
tmux set -t $s pane-border-format ' #[bold]#{pane_title} '
# Monitor on the right; four agents on the left
a1=$(tmux display -p -t $s '#{pane_id}')
mon=$(tmux split-window -h -t "$a1" -l 42% -P -F '#{pane_id}')
a3=$(tmux split-window -v -t "$a1" -P -F '#{pane_id}')
a2=$(tmux split-window -h -t "$a1" -P -F '#{pane_id}')
a4=$(tmux split-window -h -t "$a3" -P -F '#{pane_id}')
# Each pane runs its program directly, without a shell prompt
tmux respawn-pane -k -t "$mon" "$here/monitor.sh"
tmux select-pane -t "$mon" -T "Scion server: ${DEMO_REPO#"$DEMO_SERVER"/}"
for spec in "$a1 agent-1 upper title caps" "$a2 agent-2 sort uniq tac" "$a3 agent-3 lower swap strip" \
  "$a4 agent-4 head tail rev"; do
  set -- $spec
  pane=$1; shift
  # The pane stays open when the agent is done
  tmux respawn-pane -k -t "$pane" "sleep 0.8; $here/agent.sh $*; tail -f /dev/null"
  tmux select-pane -t "$pane" -T "$1"
done
[ -n "${DEMO_NO_ATTACH:-}" ] || tmux attach -t $s
