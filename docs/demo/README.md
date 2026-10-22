# The demo video

English | [简体中文](README.zh-CN.md)

[`scion-demo.mp4`](scion-demo.mp4) (and the shorter [`scion-demo.gif`](scion-demo.gif) in the project README) shows four agents changing one repository at once while a monitor watches `main` on the server. The agents are scripts: they run real `scn` commands against a real server, but write fixed changes instead of asking a model.

- [`run.sh`](run.sh) forks a repository for the run, then lays out tmux: four agents on the left, the monitor on the right.
- [`agent.sh`](agent.sh) is one agent. Each step adds a `tb` command from [`command.py.in`](command.py.in), shows `scn status -sb`, commits and lands; the last step also adds the agent's lines to `CHANGELOG.md`, where agents run into each other. A refused land is followed by `scn sync`, keeping both sides of each conflict, `scn resolve`, `scn commit` and another land.
- [`monitor.sh`](monitor.sh) and [`monitor.py`](monitor.py) redraw, twice a second, the commits on `main` since the run started and the agents' lines in `CHANGELOG.md` on `main`.
- [`demo.tape`](demo.tape) records it with [VHS](https://github.com/charmbracelet/vhs).

## Recording it again

You need a server with a repository to fork (`agentlab/toolbox` by default, set `DEMO_SOURCE` for another), `tmux`, `scn`, `python3`, and VHS with ffmpeg (`brew install vhs`). Put tokens in a directory, one file each: `admin.token` (to fork), `agent-1.token` … `agent-4.token` (land on the fork) and `monitor.token` (read it). Then, from the repository root:

```sh
DEMO_SERVER=http://localhost:7080 DEMO_TOKENS=/path/to/tokens vhs docs/demo/demo.tape
# The README's GIF: 8 frames a second, 1280 pixels wide
ffmpeg -i docs/demo/scion-demo.mp4 \
  -vf "fps=8,scale=1280:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=64[p];[b][p]paletteuse=dither=none" \
  docs/demo/scion-demo.gif
```

Without VHS, `DEMO_SERVER=… DEMO_TOKENS=… docs/demo/run.sh` runs the same demo in your terminal; `tmux kill-session -t scion-demo` ends it.
