# 阶段 4: 分发修复验证 - Orchestrator

## 概述

Kill 阶段 3 的 verifier，spawn 1 个 fixer + 1 个 checker 执行修复-验证循环。

```mermaid
flowchart TD
    Start([开始]) --> Kill[kill S3 verifier pane]
    Kill --> Spawn[spawn fixer + checker]
    Spawn --> Fix[fixer 修复]
    Fix --> Verify[checker 验证]
    Verify --> Result{通过?}
    Result -->|是| S5([阶段 5])
    Result -->|否| Check{轮数 < 5?}
    Check -->|是| Fix
    Check -->|否| S5
```

## Kill 旧 verifier

```bash
hive kill verifier-a
hive kill verifier-b
hive kill verifier-c
```

## Spawn

```bash
hive status-set busy --task code-review --activity launch-fix-verify

hive spawn fixer --cli droid --model custom:Claude-Opus-4.6-0 --workflow code-review
hive spawn checker --cli droid --model custom:GPT-5.4-1 --workflow code-review

hive layout main-vertical
hive team
```

## 修复-验证循环

### 发送修复任务

```bash
ROUND=1
printf '%s' "$ROUND" > "$WORKSPACE/state/s4-round"

cat > "$WORKSPACE/artifacts/s4-fix-task.md" <<EOF
# Fix Task (Round $ROUND)

修复以下 confirmed findings：
(粘贴 $WORKSPACE/artifacts/s3-confirmed.md 内容)

Validator Commands:
(从 request artifact 中的 Validator Commands)

Output Artifact: $WORKSPACE/artifacts/s4-fix-round-${ROUND}.md
Done Command: hive status-set done "fix complete" --task code-review --meta stage=s4 --meta role=fix --meta round=$ROUND --meta artifact=$WORKSPACE/artifacts/s4-fix-round-${ROUND}.md
EOF

hive send fixer "阶段 4 fix：执行 fix task $WORKSPACE/artifacts/s4-fix-task.md，完成时仅用其中的 Done Command 回传。"
```

### 等待修复完成

```bash
hive wait-status fixer --state done --meta stage=s4 --meta role=fix --meta round=$ROUND --timeout 3600
```

### 发送验证任务

```bash
cat > "$WORKSPACE/artifacts/s4-verify-task.md" <<EOF
# Verify Task (Round $ROUND)

验证 fixer 的修复是否解决了全部 confirmed findings。

Fix Artifact: $WORKSPACE/artifacts/s4-fix-round-${ROUND}.md
Confirmed Findings: $WORKSPACE/artifacts/s3-confirmed.md

Output Artifact: $WORKSPACE/artifacts/s4-verify-round-${ROUND}.md
Done Command: hive status-set done "verify complete" --task code-review --meta stage=s4 --meta role=verify --meta round=$ROUND --meta result=<pass|fail> --meta artifact=$WORKSPACE/artifacts/s4-verify-round-${ROUND}.md
EOF

hive send checker "阶段 4 verify：执行 verify task $WORKSPACE/artifacts/s4-verify-task.md，完成时仅用其中的 Done Command 回传。"
```

### 等待验证完成

```bash
hive wait-status checker --state done --meta stage=s4 --meta role=verify --meta round=$ROUND --timeout 3600
```

### 处理结果

- `result=pass` → 进入阶段 5
- `result=fail` 且 round < 5 → round++，重新发送修复任务（fixer 可在同一 session 继续）
- `result=fail` 且 round >= 5 → 进入阶段 5（标记修复未完成）
