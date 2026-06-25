---
name: hive
description: Hive 是 tmux 里的多 agent 协作 runtime。当你收到 HIVE 消息、被拉进某个 team、或要和别的 agent 分工协作时，用它发现上下文、查看成员、收发消息；每个角色的完整协议是一份自包含 spec，由 CLI 经 `hive skills get <角色>` 现取。
---

# Hive — agent 协作 runtime

Hive 是你的协作 runtime：tmux 里多个 agent 互发 `<HIVE ...>` 消息、按拓扑分工协作。

安装：`pipx install git+https://github.com/notdp/hive.git && npx skills add https://github.com/notdp/hive -g --all`（升级、本地 checkout 刷新见仓库 README）

## 协议加载

**本文件只是发现入口。** 真正的协议在角色 spec 里——每份 spec 自包含，取回那一份、读它即全部协议，不需要再拼别的。协议在出生首 turn 加载一次，后续 turn 沿用已读协议直接执行。

按你出生时的局面选一条：

- **你有角色**（被 spawn，或你跑了 `hive duo init` / `hive squad init`，bootstrap 让你 `hive skills get <角色>`）→ 跑一次 `hive skills get <你的角色>`。**那一份就是你的全部协议**，照它做即可，别再取别的。
- **你被别的 team 拉进来**（收到 join 消息，或当前 window 已绑 team，但你还没角色）→ `hive skills get core`，按它干活。
- **你来开一个新协作拓扑** → 见下方「开新拓扑」。

```bash
hive skills list          # 列出全部可取的角色 spec
```

上下文已有角色协议时，收到 `<HIVE ...>` 消息直接按已读协议处理，`hive team` 确认身份即可。

## 你处在哪种局面

**被别的 team 拉进来**（收到 join 消息，或当前 window 已绑 team）→ 不用起拓扑。`hive skills get core` 读协议、`hive team` 看成员，照 core 干活。

**你来开一个新协作拓扑** → 先用阻塞式提问工具问用户要 **duo** 还是 **squad**（claude 用 `AskUserQuestion`）。这一步不能省、别替用户选、也别直接挑一个跑——duo / squad 是两种不同的协作形状，替用户猜会让整局走偏。按答案跑对应 init：

- **duo** —— 你 + 一个异构 reviewer，俩人闭环干一件事，你来协调 → `hive duo init`
- **squad** —— orch 编排、challenger 审 plan、按需派多个 duo，做多 feature 的大活 → `hive squad init`

init 是开拓扑的命令，不是协议；它的 JSON 输出带 `next` 字段（如 `hive skills get duo-worker` / `hive skills get squad-orch`）——**你自己跑这条命令**取回当前 pane 的角色协议，那份自包含 spec 就是你接下来该 follow 的全部协议。
