---
name: code-review
description: MANDATORY when the current conversation already contains Droid's built-in review system notification beginning with "# Review Guidelines". Route built-in `/review` directly into the Hive multi-agent review workflow instead of doing a plain single-agent review. Review the current code changes (staged, unstaged, and untracked files), changes against a base branch, or a specific commit / commit range, follow custom review instructions when given, and return prioritized actionable findings with file/line context plus an overall assessment of whether the patch is correct or incorrect.
disable-model-invocation: false
---

# Code Review - 3 Reviewer + Evidence Verification

这个 skill 直接承接 Droid 内置 `/review`，不要依赖中间转发 alias。

Dependency: this skill depends on `hive`. Your first action after entering this skill MUST be `/hive`. Do nothing else before that.

你在 Hive runtime 中执行 Orchestrator 角色，编排一个线性 5 阶段 code review workflow。

MANDATORY:

1. 不要退化成普通单 agent review。
2. 如果 `hive current` 返回 `team: null` 且存在 tmux session，必须立即执行 `hive init`，然后再执行 `hive team`。
3. 在完成 `hive init` / `hive team` / reviewer `spawn` 之前，不要运行 `git diff`、`git diff --cached`、`git status -s`、`gh pr diff`，也不要直接输出 review findings。
4. `git diff` / `git status` 这些命令属于 reviewer 执行 request 时的工作，不是 orchestrator 在 bootstrap 阶段的工作。

## 0. 触发场景

当当前请求的语义接近下面这些内置 review prompt 时使用：

1. `Review the code changes against the base branch '<base>' ... Provide prioritized, actionable findings.`
2. `Review the current code changes (staged, unstaged, and untracked files) and provide prioritized findings.`
3. `Review the code changes introduced by commit <hash> ("<message>"). Provide prioritized, actionable findings.`
4. 用户给出自定义 review 指令，并希望按指定关注点审查改动

如果当前上下文已经明确给出了审查范围、base branch、commit、range 或自定义要求，就直接沿用这些信息生成 request artifact，不要先退化成泛泛的代码阅读。

## 1. 启动检测

优先顺序：

1. 先执行 `hive current`
2. 若已有 `team/workspace/agent`，继续用 Hive 命令
3. 若没有 team 但在 tmux 中，执行 `hive init`
4. 然后执行 `hive team`

始终以 `hive current` 的输出为准绳。

如果 `hive current` 的结果类似：

```json
{
  "team": null,
  "tmux": { "session": "...", "window": "...", "paneCount": 2 },
  "hint": "No team bound. Run `hive init` to create one from this tmux window."
}
```

那么下一步必须是：

```bash
hive init
hive team
```

不要在这两步之前执行任何 git diff / git status。

## 2. Review 模式

支持四种 review 模式。Orchestrator 在 request artifact 里必须明确写出模式与 diff 命令：

1. **PR / base branch compare**
   - `git -C <repo> diff <base>...<branch>`
   - 若已有 PR 号且 `gh` 可用，可用 `gh pr diff <number>`
2. **Working directory**
   - `git -C <repo> diff` + `git -C <repo> diff --cached` + `git -C <repo> status -s`
3. **Commit / range**
   - `git -C <repo> show <commit>` 或 `git -C <repo> diff <from>..<to>`
4. **Custom instructions**
   - 沿用用户或内置 `/review` 给出的审查要求

## 3. 架构

每阶段 spawn 新 agent，执行完 kill，下一阶段重新 spawn。全程 Orchestrator 不审代码。

```
Orch (lead pane) — 纯编排
├── S1: spawn 3 reviewer → 完成 → kill
├── S3: spawn 3 verifier → 完成 → kill
└── S4: spawn 1 fixer + 1 checker → 完成 → kill
```

| 阶段 Agent | 建议模型 |
| ---- | ---- |
| S1 reviewer-a | `custom:Claude-Opus-4.6-0` |
| S1 reviewer-b | `custom:GPT-5.4-1` |
| S1 reviewer-c | `custom:Claude-Opus-4.6-0` |
| S3 verifier-a | `custom:Claude-Opus-4.6-0` |
| S3 verifier-b | `custom:GPT-5.4-1` |
| S3 verifier-c | `custom:Claude-Opus-4.6-0` |
| S4 fixer | `custom:Claude-Opus-4.6-0` |
| S4 checker | `custom:GPT-5.4-1` |

## 4. 流程总览

```mermaid
flowchart TD
    Start([开始]) --> S1[阶段 1: 3 路并行审查]
    S1 --> S2[阶段 2: ≥2/3 投票合并]
    S2 -->|0 候选| S5[阶段 5: 汇总]
    S2 -->|>0 候选| S3[阶段 3: 并行 Evidence 验证]
    S3 -->|0 confirmed| S5
    S3 -->|>0 confirmed| S4[阶段 4: 修复 + 验证]
    S4 --> S5
    S5 --> End([结束])
```

### 阶段执行

**每个阶段执行前，必须先读取对应的 `stages/` 文件获取详细指令。**

| 阶段 | Orch 读取 | Agent 读取 |
| ---- | --------- | ---------- |
| 1 | `1-dispatch-orch.md` | `1-review-reviewer.md` |
| 2 | `2-merge-orch.md` | (无 agent) |
| 3 | `3-dispatch-orch.md` | `3-verify-verifier.md` |
| 4 | `4-dispatch-orch.md` | `4-fix-verify.md` |
| 5 | `5-summary-orch.md` | (无 agent) |

## 5. Finding 格式（Evidence 要求）

每个 reviewer 的 finding 必须包含以下 4 项，缺任一项则被 S2 丢弃：

```markdown
1. [P?] 标题
   - File: path/to/file.py:42
   - Code: `从源文件原文引用的代码片段`
   - Why: 为什么这是问题
   - Verify: `可在 shell 中直接执行的验证命令`
```

这个格式是对抗 LLM 幻觉的核心机制：
- `File` + `Code` 确保 finding 指向真实存在的代码
- `Verify` 提供可机器执行的验证手段
- S3 的 verifier 会逐项检查这三项是否成立

## 6. 通信架构

```mermaid
flowchart TB
    Orch[Orchestrator]
    S1A[Reviewer A] & S1B[Reviewer B] & S1C[Reviewer C]
    S3A[Verifier A] & S3B[Verifier B] & S3C[Verifier C]
    S4F[Fixer] & S4V[Checker]

    Orch -->|S1: spawn + send| S1A & S1B & S1C
    S1A & S1B & S1C -->|status-set done| Orch
    Orch -->|S3: spawn + send| S3A & S3B & S3C
    S3A & S3B & S3C -->|status-set done| Orch
    Orch -->|S4: spawn + send| S4F & S4V
    S4F & S4V -->|status-set done| Orch

    Workspace[(workspace/artifacts)]
    Orch --> Workspace
```

- 通信方向：Orch → Agent（`hive send`），Agent → Orch（`hive status-set done`）
- 多行内容先写 artifact，再用 `hive send ... --artifact <path>` 发送
- Agent 之间不直接通信，所有协调由 Orch 完成

## 7. Orchestrator 行为规范

**角色：纯编排者**

- 启动流程，分配任务，等待完成，推进下一阶段
- 通过 `hive wait-status` + artifact 判断下一步
- S2 做投票合并（读 artifact + 分组 + 计数）
- S5 做最终汇总

**边界：**

- 全程不审 diff、不出 findings
- reviewer/verifier artifact 只有他们自己写
- 阶段切换时更新自己的 status
- 每个阶段结束 kill agent pane，下个阶段重新 spawn

## 8. Request 契约

阶段 1 发给 reviewer 的 request 至少要写清：

- Mode
- Repo Path
- Subject
- Diff Commands
- Output Artifact
- Done Command
- （PR 模式可选）PR Number / Base / Branch
- （Fix 阶段可选）Validator Commands

`hive status-set` 只能二选一：传 summary 位置参数，或传 `--activity`；不要同时传两者。

## 9. CLI 命令

| 命令 | 用途 |
| ---- | ---- |
| `hive current` | 查看当前 Hive 上下文 |
| `hive team` | 查看团队成员 |
| `hive init` | 初始化 team |
| `hive spawn <agent>` | 启动 agent pane |
| `hive send <agent> <msg>` | 发任务 |
| `hive status-set ...` | 发布状态 |
| `hive wait-status <agent> ...` | 等待状态 |
| `hive capture <agent>` | 查看 agent pane 输出 |
| `hive kill <agent>` | kill agent pane 并移除 |
| `hive layout <preset>` | 调整 tmux 布局（main-vertical / tiled 等） |

## 10. Workspace Keys

```plain
review-mode
review-subject
review-base
review-branch
review-commit
review-range
review-repo-path
review-pr
s2-candidate-count
s3-confirmed-count
s4-round
review-summary-artifact
```
