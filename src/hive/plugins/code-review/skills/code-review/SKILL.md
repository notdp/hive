---
name: code-review
description: 基于 Hive 协作面做交叉代码审查。支持 PR 分支比较、工作区变更、历史 commit 三种模式。
disable-model-invocation: true
---

# Cross Review

你在 Hive runtime 中执行交叉代码审查。

## Review 模式

收到 review 任务后，必须先从 orchestrator 的 request 或 workspace state 里读取 **明确的 review 模式**。不要根据线索猜测；如果缺少 `mode`、`base`、`branch`、`commit`、`range` 这类关键字段，先请求补充上下文，不要自行默认。

### 1. Review against a base branch（PR Style）

比较当前分支与基准分支的差异。

```bash
git log --oneline <base>..<branch>      # 确认 commit 范围
git diff <base>...<branch>              # 三点 diff，只看目标分支上的变更
```

- 如果 orchestrator 明确给了 PR 编号，并且当前环境可用 `gh`，用 `gh pr diff <number>` 拿 diff
- 如果给的是 `base` + `branch`，用上面的 git 命令
- 如果缺少 `base` 或 `branch`，先请求澄清，不要默认成 `main` 或 `HEAD`

### 2. Review uncommitted changes（Working directory）

审查尚未提交的本地变更。

```bash
git diff                                # unstaged 变更
git diff --cached                       # staged 变更
git status -s                           # 变更文件列表
```

- 同时看 staged 和 unstaged，给出完整画面
- 注意标注哪些变更已 staged、哪些还未 stage

### 3. Review a commit（From history）

审查某个具体的 commit。

```bash
git show <commit-sha> --stat            # commit 概览
git show <commit-sha>                   # 完整 diff
```

- 如果给了 commit range，用 `git diff <from>..<to>`

## 请求契约

阶段 1 的 request 至少要写清：

- Mode
- Repo Path
- Subject
- Diff Commands
- Output Artifact
- Done Command

Agent 只执行 request 里明确给出的 diff 命令，不再自行推断 review 对象。

## 流程

1. 确定 review 模式和 diff 来源
2. 拿到 diff，通读全部变更
3. 归纳 correctness / risk / follow-up
4. 把结论写成 artifact
5. 用 `hive status-set done ... --meta artifact=<path>` 回传

## 输出格式

- Summary（变更概要 + review 模式）
- Findings（按文件/模块分组）
- Risks
- Follow-up

## 协作约束

- 优先通过 `hive status-set` 回传完成态
- 非必要不要再补一条重复的 `hive send ... complete`
- 如果需要额外上下文，再使用 `hive send`
