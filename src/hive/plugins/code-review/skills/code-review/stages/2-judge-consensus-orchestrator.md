# 阶段 2: 判断共识 - Orchestrator

## 概述

分析 Opus 和 Codex 的审查结果，判断是否达成共识。

## 任务

读取阶段 1 的两个 artifact，判断：

- 是否都没有问题
- 是否发现相同/相近问题
- 是否结论有实质分歧

## 判断结果

| 结果 | 条件 | 下一阶段 |
| ---- | ---- | -------- |
| `both_ok` | 双方都没发现值得修的问题 | → 阶段 5 |
| `same_issues` | 双方发现相同/相近问题 | → 阶段 4 |
| `divergent` | 一方发现问题，另一方不同意，或 issue 集合差异明显 | → 阶段 3 |

## 执行

```bash
CTX_JSON=$(hive current)
WORKSPACE=$(printf '%s' "$CTX_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("workspace",""))')

hive status-set busy "stage-2" --task code-review --activity judge-consensus

# 人工分析 opus-r1.md 与 codex-r1.md 后记录结果
printf '%s' 'both_ok' > "$WORKSPACE/state/s2-result"   # 或 same_issues / divergent
```

然后进入对应阶段。
