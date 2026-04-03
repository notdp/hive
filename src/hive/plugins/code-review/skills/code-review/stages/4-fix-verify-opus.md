# 阶段 4: 修复 - Opus

根据共识 artifact 修复已确认的问题，并运行 request 中约定的 validators。

```mermaid
flowchart TD
    Start([开始]) --> S1[1. 读取 fix list]
    S1 --> S2[2. 修改代码]
    S2 --> S3[3. 运行 validators]
    S3 --> S4[4. 写 fix artifact]
    S4 --> S5[5. status-set done]
```

## 规则

- 只修复共识中标记为 `Fix` 的问题
- 尽量保持改动最小、聚焦
- 必须运行相关 validators，并在 artifact 里记录命令与结果
- 若无法完整修复，明确写出残留项

## 输出 artifact 模板

```markdown
# Fix Round N

## Fixed
- C1: ...

## Validators
- command: ...
- result: pass/fail

## Remaining
- ...
```

## 回传

```bash
hive status-set done "fix round complete"           --task code-review           --activity fix-round-done           --meta stage=s4           --meta role=fix           --meta round=<n>           --meta artifact=/tmp/hive-xxx/artifacts/s4-fix-round-<n>.md
```
