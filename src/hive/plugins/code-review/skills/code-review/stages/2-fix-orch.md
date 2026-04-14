# 阶段 2: 修复验证 - Orchestrator

## 概述

Spawn 1 个 fixer + 1 个 checker，通过消息驱动的修复-验证循环修复 confirmed findings。

```mermaid
flowchart TD
    Start([开始]) --> Spawn[spawn fixer + checker]
    Spawn --> SendFix[send fix task → idle]
    SendFix --> Idle[idle 等消息]
    Idle --> Msg{收到 HIVE 消息}
    Msg --> IsFix{fix done?}
    IsFix -->|是| SendVerify[send verify task → idle]
    SendVerify --> Idle
    IsFix -->|verify done| CheckResult{pass?}
    CheckResult -->|是| Kill[kill fixer + checker → S3]
    CheckResult -->|否| CheckRound{轮数 < 5?}
    CheckRound -->|是| SendFix2[send 下轮 fix task → idle]
    SendFix2 --> Idle
    CheckRound -->|否| Kill
```

## Spawn

```bash
hive spawn fixer --cli droid --model custom:Claude-Opus-4.6-0 --workflow code-review
hive spawn checker --cli droid --model custom:GPT-5.4-1 --workflow code-review

hive layout main-vertical
```

## 修复-验证循环

以下示例中的 `ARTIFACT_DIR` / `STATE_DIR` 始终指向当前 review run：

- 刚从阶段 1 进入阶段 2 时，沿用阶段 1 已确定的 run 目录
- 后续轮次若由 `fix done` / `verify done` 消息唤醒，可从消息里的 `artifact` 路径反推：`ARTIFACT_DIR=$(dirname "$ARTIFACT_PATH")`，`RUN_NAME=$(basename "$ARTIFACT_DIR")`，`STATE_DIR="$WORKSPACE/state/${RUN_NAME}"`

### 发送修复任务

```bash
ROUND=1
fix_request_id="fix-request-round-${ROUND}"
printf '%s' "$ROUND" > "$STATE_DIR/s2-round"

cat > "$ARTIFACT_DIR/s2-fix-task.md" <<EOF
# Fix Task (Round $ROUND)

修复以下 confirmed findings：
(粘贴 $ARTIFACT_DIR/confirmed-findings.md 内容)

Validator Commands:
(从 request artifact 中的 Validator Commands)

Run Artifact Root: $ARTIFACT_DIR
Run State Root: $STATE_DIR
Output Artifact: $ARTIFACT_DIR/s2-fix-round-${ROUND}.md
Reply To Message ID: $fix_request_id
Done Command: hive send orch "fix done round=$ROUND artifact=$ARTIFACT_DIR/s2-fix-round-${ROUND}.md" --artifact $ARTIFACT_DIR/s2-fix-round-${ROUND}.md
EOF

hive send fixer "阶段 2 fix：执行 fix task $ARTIFACT_DIR/s2-fix-task.md，完成时仅用其中的 Done Command 回传。"
```

发完后 **idle 等消息**。

### 收到 fix done 消息时

发送验证任务给 checker：

```bash
verify_request_id="verify-request-round-${ROUND}"

cat > "$ARTIFACT_DIR/s2-verify-task.md" <<EOF
# Verify Task (Round $ROUND)

验证 fixer 的修复是否解决了全部 confirmed findings。

Fix Artifact: $ARTIFACT_DIR/s2-fix-round-${ROUND}.md
Confirmed Findings: $ARTIFACT_DIR/confirmed-findings.md

Run Artifact Root: $ARTIFACT_DIR
Run State Root: $STATE_DIR
Output Artifact: $ARTIFACT_DIR/s2-verify-round-${ROUND}.md
Reply To Message ID: $verify_request_id
Done Command: hive send orch "verify done round=$ROUND result=<pass|fail> artifact=$ARTIFACT_DIR/s2-verify-round-${ROUND}.md" --artifact $ARTIFACT_DIR/s2-verify-round-${ROUND}.md
EOF

hive send checker "阶段 2 verify：执行 verify task $ARTIFACT_DIR/s2-verify-task.md，完成时仅用其中的 Done Command 回传。"
```

发完后 **idle 等消息**。

### 收到 verify done 消息时

- `result=pass` → kill fixer + checker → 进入阶段 3
- `result=fail` 且 round < 5 → round++，发下轮 fix task → idle
- `result=fail` 且 round >= 5 → kill fixer + checker → 进入阶段 3（标记修复未完成）

### 清理

```bash
hive kill fixer
hive kill checker
```
