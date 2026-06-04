---
name: hive
description: Hive 基础 skill。让 agent 作为 Hive runtime 成员工作：发现上下文、查看成员、接收 <HIVE ...> 消息、发送消息；完整协议与协作拓扑（cell / crew）由 CLI 经 `hive skills get` 现取。
metadata: {"hive-bot":{"os":["darwin","linux"],"requires":{"bins":["tmux","python3","hive"]}}}
---

# Hive — agent 协作 runtime（discovery stub）

Hive 是你的协作 runtime。**本文件是发现入口，不是用法手册** —— 用法用 `hive skills get` 取。

## 安装

```bash
pipx install git+https://github.com/notdp/hive.git
npx skills add https://github.com/notdp/hive -g --all
# 升级 CLI：
pipx upgrade hive
# 升级全局 skill（从 GitHub 安装的用户用这条）：
npx skills update hive -g
# 本地 repo checkout 的刷新（skills lock 不跟踪 local source，update 用不了）：
npx skills add "$PWD" -g --all
```

`hive --help` 确认安装。skill 过期时 `hive` 命令会发 stderr 提醒，或跑 `hive doctor --skills` 看详情。

## Start here

加载本 skill 后先取完整协议：

```bash
hive skills get core      # 完整协议：命令速查 / 消息机制 / thread / 协作规则 / compact
```

跑 `hive skills list` 看全部可取 spec。

**被别的 team 拉进来**（收到 join 消息 / 当前 window 已绑 team）→ 不用起拓扑，`hive team` 看成员，按 core 干活。

**你来起一个新协作拓扑** → 先问用户要 cell 还是 crew（用 runtime 的阻塞式提问工具，见 core 的「问用户」），按答案跑：

- **cell** —— worker + 异族 validator，你协调 → `hive cell init`
- **crew** —— orch + challenger + 按需 cell，编排 → `hive crew init`

跑完后当前 pane 成为 worker / orch，会被注入 `hive skills get <role>` 自动加载角色协议；spawn 出来的 validator / challenger 同理。
