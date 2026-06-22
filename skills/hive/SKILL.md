---
name: hive
description: Hive 是 tmux 里的多 agent 协作 runtime。当你收到 HIVE 消息、被拉进某个 team、或要和别的 agent 分工协作时，用它发现上下文、查看成员、收发消息；完整协议与 duo / squad 拓扑由 CLI 经 `hive skills get` 现取。
---

# Hive — agent 协作 runtime

Hive 是你的协作 runtime：tmux 里多个 agent 互发 `<HIVE ...>` 消息、按拓扑分工协作。

安装：`pipx install git+https://github.com/notdp/hive.git && npx skills add https://github.com/notdp/hive -g --all`（升级、本地 checkout 刷新见仓库 README）

## 协议加载

**本文件只是发现入口。** 协议在出生首 turn 加载一次，后续 turn 沿用已读协议直接执行。

出生首 turn / 身份变化 / 上下文里协议缺失时：

```bash
hive skills get core      # 通信底座：命令速查 / 消息机制 / thread / 协作规则
hive skills list          # 列出全部可取的 spec
```

上下文已有 role + core 协议时，收到 `<HIVE ...>` 消息直接按已读协议处理，`hive team` 确认身份即可。

## 你处在哪种局面

**被别的 team 拉进来**（收到 join 消息，或当前 window 已绑 team）→ 不用起拓扑。`hive team` 看成员，按已读 core 协议干活。

**你来开一个新协作拓扑** → 先用阻塞式提问工具问用户要 **duo** 还是 **squad**（claude 用 `AskUserQuestion`，见 core 的「问用户」）。这一步不能省、别替用户选、也别直接 `hive init`——duo / squad 是两种不同的协作形状，替用户猜会让整局走偏。按答案跑：

- **duo** —— 你 + 一个异构 reviewer，俩人闭环干一件事，你来协调 → `hive duo init`
- **squad** —— orch 编排、challenger 审 plan、按需派多个 duo，做多 feature 的大活 → `hive squad init`

init 的 JSON 输出带 `next` 字段（如 `hive skills get duo-worker` / `hive skills get squad-orch`）——**你自己跑这条命令**取回当前 pane 的角色协议，照它做即可。
