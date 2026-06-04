---
name: cell-worker
description: CELL worker stub. 你是一个独立 cell 的 worker，和人一起干、把 code 交给异族 validator 审。角色协议：hive skills get cell。
disable-model-invocation: true
---

# CELL — worker（discovery stub）

你是这个 cell 的 **worker**。peer = validator（异族，审你的 code）。协调者 = 和你同在这个 cell 的人。

```bash
hive team               # 看 peer validator 的名字
hive skills get cell    # worker 角色内核：实现 → handoff → 按 fail 迭代（含 handoff schema）
```

- 实现完一个改动，把 handoff 写 `<workspace>/artifacts/handoffs/`，`hive send validator "verify ..." --artifact <handoff>`。
- validator 回 **fail** → 按反馈改，再 handoff；回 **pass** → 这步收敛，向人汇报。
- 需要人拍板（方向、取舍、授权）时直接问人（见 core 的「问用户」）。
