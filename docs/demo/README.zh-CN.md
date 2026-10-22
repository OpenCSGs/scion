# 演示视频

[English](README.md) | 简体中文

[`scion-demo.mp4`](scion-demo.mp4)（以及项目 README 里更短的 [`scion-demo.gif`](scion-demo.gif)）展示四个 agent 同时修改同一个仓库，右侧实时显示服务端上的 `main`。这些 agent 是脚本：对真实的服务端执行真实的 `scn` 命令，只是写入的改动是固定的，没有调用模型。

- [`run.sh`](run.sh)：为这次演示 fork 一个仓库，然后用 tmux 分屏：左侧四个 agent，右侧监控。
- [`agent.sh`](agent.sh)：一个 agent。每一步从 [`command.py.in`](command.py.in) 加一个 `tb` 命令，显示 `scn status -sb`，提交并合入；最后一步还把这个 agent 的几行写进 `CHANGELOG.md`，agent 之间会在这里撞上。合入被拒绝后，依次执行 `scn sync`、保留每处冲突的双方、`scn resolve`、`scn commit`，再次合入。
- [`monitor.sh`](monitor.sh) 和 [`monitor.py`](monitor.py)：每秒刷新两次，显示演示开始后 `main` 上的提交，以及 `main` 上 `CHANGELOG.md` 里各 agent 的行。
- [`demo.tape`](demo.tape)：用 [VHS](https://github.com/charmbracelet/vhs) 录制。

## 重新录制

需要：一个服务端，上面有一个可以 fork 的仓库（默认 `agentlab/toolbox`，用 `DEMO_SOURCE` 指定别的）；`tmux`、`scn`、`python3`；VHS 和 ffmpeg（`brew install vhs`）。把令牌放在一个目录里，每个一个文件：`admin.token`（用来 fork）、`agent-1.token` … `agent-4.token`（在 fork 出的仓库里合入）和 `monitor.token`（读取）。然后在仓库根目录执行：

```sh
DEMO_SERVER=http://localhost:7080 DEMO_TOKENS=/path/to/tokens vhs docs/demo/demo.tape
# README 用的 GIF：每秒 8 帧，宽 1280 像素
ffmpeg -i docs/demo/scion-demo.mp4 \
  -vf "fps=8,scale=1280:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=64[p];[b][p]paletteuse=dither=none" \
  docs/demo/scion-demo.gif
```

没有 VHS 时，`DEMO_SERVER=… DEMO_TOKENS=… docs/demo/run.sh` 直接在终端里运行同样的演示；`tmux kill-session -t scion-demo` 结束。
