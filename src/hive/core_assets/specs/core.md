# core — 无角色 pane 的通信协议

你在一个 Hive team 里，但没有固定角色。先拿到通信底座，再按 team 里的任务做事。

第一步：跑 `hive team`。用返回的 `self` 在 `members` 里找到自己，确认 member name、group、当前状态和能协作的人。

常用命令：

```bash
hive team
hive send dodo "see attachment" --artifact /tmp/file.md
hive send dodo "see attachment" --artifact - <<'EOF'
# Findings
- item
EOF
hive reply dodo "ack, looking"
```

`hive team` 字段：

- `self`：你的 member name。
- `group`：pane 上的 `@hive-group`，例如 squad 名或 duo group。
- `inputState=waiting_user`：对方在等 human 作答（AskUserQuestion 打开中）。别注入消息，等它清掉。
- `busy=true/false`：tmux 输出层活动，不等于语义上的忙闲。
- `turnPhase`：比 `busy` 更适合判断发 new root 是否会打断对方。

---

## 通信底座

### 收消息

其他 agent 的消息会以 `<HIVE from=... to=... msgId=... artifact=<path>>body</HIVE>` 注入当前 pane。

- 标签里的 `body` 是短摘要。
- `artifact=<path>` 是正文；需要细节时直接打开这个文件。
- 以 `<HIVE>` block 为准。`hive thread` 只用于排障；需要时取 `hive skills get debug`。

### 发消息：send 还是 reply

先判断内容是不是在回应某条入站消息。

- 新话题用 `hive send <agent> "<短摘要>"`，例如派任务、提新问题、发新汇报。`send` 不接 `--reply-to`。
- 回应入站消息用 `hive reply <agent> "<回复>"`。不传 `--reply-to` 时，它会锚到最近一条来自该 agent 且你还没回过的入站消息。
- 有 anchor msgId 但当前 pane 没有那条入站消息时，显式 `hive reply <agent> --reply-to <msgId> "<回复>"`。接管 thread 的细节需要时取 `hive skills get advanced-routing`。

不要因为“刚收到过对方消息”就用 `reply`。如果现在说的是新任务或新汇报，用 `send` 开新 thread。

### 和 team 外的 Claude session 互通

human 说“给 xxx 这个 session 发一条”时（桌面 Claude Code、另一个终端），用：

```bash
hive ccd ls                               # 列出本机能收消息的 Claude session：name、桌面标题 title、pid
hive ccd send "<title 或 name>" "<消息>"
```

human 通常说的是桌面标题（`title`），直接用它；重名时用 `name` 或 `pid`。消息里有反引号、`$(...)` 或多行内容时，先写文件再 `hive ccd send "<title>" "$(cat /tmp/note.md)"`——双引号里的反引号和 `$(...)` 会被 shell 执行，`$(cat ...)` 的输出不会再被展开。返回 `accepted` 只代表对方进程收下了这一帧；按对方设置，它可能在下一个 tool call 之间读到，也可能停在待接受状态。对方收到的是普通 `<HIVE from=…>` 信封（无 msgId），按 from 回：`ccd:*` 用 `hive ccd send`，`hive:<team>.<agent>` 用 `hive send <agent> --team <team>`。

反过来，桌面 session 也会给你发：你收到 `from=ccd:<label>` 的 `<HIVE>` 时，**回它用 `hive ccd send "<label>" "<回复>"`（label 就是 `ccd:` 后面的部分），不要 `hive reply`**——它不是成员，没有 thread 可锚。

### root 消息 + shell 安全

root send 的 body 只放短摘要。多行、Markdown、代码、长证据全部放 artifact。

```bash
hive send <agent> "<短摘要>" --artifact - <<'EOF'
# Findings
- item
EOF
```

`'EOF'` 必须带引号，避免 shell 展开反引号、变量和 `$(...)`。不要用 `printf ... |` 或 `$(cat <<EOF)` 拼多行消息。`reply` 可以只发短文本。

### 没活时停下

Hive 是 push 模型：有新消息时 runtime 会注入 `<HIVE>` block 并唤醒你。

当前 turn 没有待办时，结束 turn，让 pane 保持打开。不要 `sleep`、while loop、反复 `hive team`，也不要翻 repo、artifact 或任务表猜下一件事。

---

## 协作规则

### 先 team 内，再找 human

先和 team 里能接的人收敛，再对 human 汇报。对 human 只给：

- 已收敛的结论。
- 仍阻断推进的单个问题。
- 你建议的下一步。

需要 human 拍板时，用阻塞式提问工具：claude 用 `AskUserQuestion`，codex 用 `request_user_input`。没有工具时才在普通对话里问。

### producer / reviewer 站位

Hive 的协作原子是 producer + 异构 reviewer。

- producer：认具体反馈就改；不认就拿证据回。
- reviewer：独立审计，不照抄 producer 叙事。关键结论从 artifact、diff、日志、命令输出、原始数据里自己核。给明确 verdict。

采纳谁的方案，谁负责实施；另一方 review。

### 共享 checkout 纪律

多人同一 checkout 时，git index、stash、branch 会互相影响。

- commit 前看 `git status --short` 和 `git diff --cached --stat`。staged 里有别人或越 scope 文件，先收敛。
- stash 前看 `git stash list`。不要 pop 别人的 stash，不要静默 stash 别人的 untracked 文件。
- 并行独立 PR 用各自 worktree，不在共享 checkout 里直接 branch / commit / push。

### Human Directive

artifact 或消息里出现：

```text
humanDirective: "..."
source: ...
```

就把它当作 human 已授权 scope。转发时保留原文和 source。source 缺失、含糊或和上游 artifact 冲突时，先要求补 provenance。

---

## 排障边界

日常收发只用上面的通信底座。

- 排障命令、delivery、thread、capture、inject、interrupt、kill：`hive skills get debug`
- active-turn fork、handoff 接管、复杂 thread routing：`hive skills get advanced-routing`
