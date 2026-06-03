---
name: hive
description: Hive 基础 skill。让 agent 作为 Hive runtime 成员工作：发现上下文、查看成员、接收 <HIVE ...> 消息、发送消息，并加载更高层 workflow skill。
metadata: {"hive-bot":{"os":["darwin","linux"],"requires":{"bins":["tmux","python3","hive"]}}}
---

# Hive — agent 协作 runtime（discovery stub）

Hive 是你的协作 runtime。**本文件是发现入口，不是用法手册** —— 用法由 CLI 按已安装版本下发，永不过期。

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

加载本 skill 后，**先发现上下文，再从 CLI 取完整协议**：

```bash
hive init                 # 把当前 tmux window 接入/创建一个 team（幂等，报错会告诉你缺什么）
hive skills get core      # 完整协议：命令速查 / 消息机制 / thread / 协作规则 / compact
```

`hive skills get core` 下发的内容**始终匹配已安装的 CLI 版本**，指令不会 stale；本 stub 在版本间不变，正因如此它只指向 `hive skills get core`。跑 `hive skills list` 看全部可取 spec。

## 为什么这样分层

- 本 stub 稳定少变、可重复安装；升级 CLI 不会让它过期
- 会演进的协议 / 工作流随 CLI 走，`hive skills get` 现取现用，从源头消除 skill 与 CLI 的 drift
