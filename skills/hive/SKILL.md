---
name: hive
description: Hive 基础 skill。让 agent 作为 Hive runtime 成员工作：发现上下文、查看成员、接收 <HIVE ...> 消息、发送消息；完整协议与协作拓扑（cell / crew）由 CLI 经 `hive skills get` 现取。
metadata: {"hive-bot":{"os":["darwin","linux"],"requires":{"bins":["tmux","python3","hive"]}}}
---

# Hive — agent 协作 runtime

Hive 是你的协作 runtime：tmux 里多个 agent 互发 `<HIVE ...>` 消息、按拓扑分工协作。

安装：`pipx install git+https://github.com/notdp/hive.git && npx skills add https://github.com/notdp/hive -g --all`（升级、本地 checkout 刷新见仓库 README）

## 先取协议

**本文件只是发现入口，不是用法手册。** 跑任何 `hive` 命令前，先从 CLI 取协议：

```bash
hive skills get core      # 从这开始：命令速查 / 消息机制 / thread / 协作规则 / compact
hive skills list          # 列出全部可取的 spec
```

## 你处在哪种局面

**被别的 team 拉进来**（收到 join 消息，或当前 window 已绑 team）→ 不用起拓扑。`hive team` 看成员，按 core 干活。

**你来开一个新协作拓扑** → 先用阻塞式提问工具问用户要 **cell** 还是 **crew**（claude 用 `AskUserQuestion`，见 core 的「问用户」）。这一步不能省、别替用户选、也别直接 `hive init`——cell / crew 是两种不同的协作形状，替用户猜会让整局走偏。按答案跑：

- **cell** —— 你 + 一个异族 reviewer，俩人闭环干一件事，你来协调 → `hive cell init`
- **crew** —— orch 编排、challenger 审 plan、按需派多个 cell，做多 feature 的大活 → `hive crew init`

init 完，当前 pane 的角色（worker / orch）协议会自动到位，照它做即可。
