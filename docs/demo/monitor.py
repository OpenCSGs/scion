"""Renders the demo's monitor from `scn log --json` (stdin) and CHANGELOG.md on main (a file)."""

import json
import sys

B, D, G, M, C, O = "\033[1m", "\033[2m", "\033[32m", "\033[35m", "\033[36m", "\033[0m"
# Only the commits made after the demo started: the history from `base` down is left out
base = sys.argv[1]
changelog = open(sys.argv[2]).read().splitlines() if len(sys.argv) > 2 else []
entries = []
for e in json.load(sys.stdin)["entries"]:
    if e["id"] == base:
        break
    entries.append(e)
print(f"{B}main{O}  {D}{len(entries)} commits landed by the agents, newest first{O}\n")
for e in entries:
    subject = e["message"].splitlines()[0] if e["message"] else ""
    print(f"{G}{e['id'][:12]}{O}  {M}{e['author']:<8}{O} {subject}")
# Every agent's lines, merged however their lands interleaved
notes = [line for line in changelog if line.startswith("- `tb ") and "added by agent-" in line]
print(f"\n{B}CHANGELOG.md on main{O}  {D}{len(notes)} lines from the agents{O}\n")
for line in notes:
    print(f"{C}{line}{O}")
