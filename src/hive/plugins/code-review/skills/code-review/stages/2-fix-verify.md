# 阶段 2: 修复 / 验证

本文件同时包含 fixer 和 checker 两个角色的指令。根据 Orchestrator 发来的任务判断自己的角色。

---

## Fixer

根据 confirmed findings 修复问题，运行 validators。

### 规则

- 只修复 confirmed findings 中列出的问题
- 保持改动最小、聚焦
- 必须运行 task 中指定的 validator commands
- 若无法完整修复，明确写出残留项

### 输出 artifact

```markdown
# Fix Round N

## Fixed
- C1: 修复描述

## Validators
- command: PYTHONPATH=src python -m pytest tests/ -q
- result: pass / fail

## Remaining
- C3: 未能修复的原因
```

### 回传

用 task 中的 Done Command 通知 orchestrator：

```bash
hive send orch "fix done round=N artifact=<artifact path>" --artifact <artifact path>
```

**只发这一条，不要发其他消息。**

---

## Checker

验证 fixer 的修复是否解决了全部 confirmed findings。

### 检查点

- 每个 confirmed finding 是否真正解决
- validator 结果是否可信
- 是否引入新的明显问题
- 是否还有未完成的 fix item

### 输出 artifact

```markdown
# Verify Round N

## Verdict
pass / fail

## Per-Finding
- C1: ✅ fixed
- C2: ✅ fixed
- C3: ❌ not fixed — 原因

## Notes
- ...
```

### 回传

用 task 中的 Done Command 通知 orchestrator：

```bash
hive send orch "verify done round=N result=<pass|fail> artifact=<artifact path>" --artifact <artifact path>
```

**只发这一条，不要发其他消息。** `result` 只能是 `pass` 或 `fail`。
