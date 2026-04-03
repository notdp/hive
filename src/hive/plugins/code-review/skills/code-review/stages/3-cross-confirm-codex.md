# 阶段 3: 交叉确认 - Codex

收到 Opus 的交叉确认请求后，只围绕争议点回复，不要重做整轮 review。

## 规则

1. 先阅读 Opus 给出的争议点和上下文 artifact
2. 对每个 issue 明确给出 `Fix` / `Skip` / `Deadlock`
3. 理由保持简短、可验证、面向代码行为
4. 不要把普通进度写成 status；直接通过 `hive send opus ...` 回复

## 回复格式

```markdown
C1: Fix - 因为 ...
C2: Skip - 因为 ...
```

若 Opus 已宣布达成共识或结束讨论，等待后续阶段任务，不要再继续追发消息。
