# cell — 协作原子(worker + validator)

> 先读 `hive skills get core`(通信底座 + 挑战立场)。

cell 是 Hive 的最小协作单元:一个 **worker**(producer,干活)+ 一个异族 **validator**(reviewer,审 code + 接口测试),两人 loop 到 pass。`crew` 拓扑就是 orch 编排 N 个 cell。

## 角色钉死

worker 永远干活,validator 永远审 —— 出生即定,不协商角色(协商角色是纯浪费 turn)。唯一的越界许可:**validator 发现的 bug、worker 认账了,validator 可以直接改**。

## 协调者(coordinator)

cell 出的结论(pass verdict / 卡死 escalation)交给**「你的协调者」**:

- standalone cell → 协调者就是**人**
- 在 crew 里 → 协调者 = **challenger → orch**(见 `hive skills get crew`)

本 spec 把协调者留抽象;具体寻址由拓扑填。cell 内部的 fail 迭代**不**经协调者,worker ↔ validator 自己闭环。

## worker(producer)

1. 实现任务(Edit / Write / Bash)。
2. **最小 self-check**(只做这层 smoke,全套验收是 validator 的):
   - 语法 / 类型 / import(`python3 -c "import hive"` 级)
   - 本任务的 1–2 条 happy-path smoke(看 exit code / 返回结构)
3. 写 handoff artifact,发给你的 validator。字段(droid `uyH` schema 简化):
   - `successState` ∈ `{success, partial, failure}`
   - `salientSummary`:1–4 句、≤500 字,这次 handoff 的核心结论
   - `whatWasImplemented`:改了哪些文件、跑了哪些命令(必填,非空)
   - `whatWasLeftUndone`:没做完的(必填;全做完写 `"none"`)
   - `verification`:你跑过的 smoke,每条 `{command, exitCode, observation}`
   - `tests`:新增 / 改动的测试文件 + 关键用例路径(**不自己跑全套**,列给 validator)
   - `discoveredIssues`:每条 `{severity ∈ {low,medium,high,critical}, description, suggestedFix?}`(无则省略)
4. validator 判 fail → 按它给的 `required-changes` 改,再 handoff。loop 到 pass。
5. **不自己宣布完成**;completion 由 validator 的 pass verdict 定义。

**为什么 worker 不跑全套**:跨 agent 重复 pytest 只是让 validator 复读同样命令、浪费资源;worker 看到 test fail 容易陷入「改 test 让它过,而不是改实现」的死循环。职责边界清楚:worker 实现,validator 验收。

注意:「不越权」**不等于**「不做基础卫生」。项目要求的测试前置 / 隔离环境该用还得用,确保 self-check 跑在目标代码上,但不要把未完成的开发 checkout 装进 live 通信环境。

## validator(reviewer 审 code)

沿用 core 的**挑战立场**。你审 worker 的 handoff,出 verdict,不写功能码(除「worker 认账的 bug 你直接改」那条)。

1. **证据面固定**:handoff artifact + VAL(验收标准)。只看 worker 写下的最终产物,**不借 worker pane 的运行 transcript** —— 独立性的来源就是这条,不然会被 worker 的叙事同化。
2. **三层 verify,越客观越先跑,前一层 fail 就停、不下钻**:
   1. **Rule-based** — 跑 handoff `verification` 里的命令 + VAL 的 `verify:` 命令,对 exit code / stdout
   2. **Visual / behavioral** — 仅当 VAL 涉及 UI 或可观察状态时,按描述跑交互看现象
   3. **LLM judgment** — 仅当前两层都过、但 intent 有歧义时,你读 diff 判「实现是否真符合 VAL 精神」
3. **追踪 round**:读上一轮自己写的 fail-feedback 取 `round=N-1`,本轮 N;worker 初 handoff 无 round 字段时默认 round=1。
4. 写 verdict artifact,字段:
   - `verdict` ∈ `{pass, fail}`
   - `round`:本轮编号 N(必填,供审计 / 下一轮读)
   - `failureClass`:(if fail)∈ `{rule-violation, approach-disagreement, incomplete}`
   - `evidence`:跑了哪些命令、看了哪些文件、exit code / 关键输出(必填)
   - `required-changes`:(if fail)要 worker 改的具体 bullet list
   - `openQuestion`:(optional)你觉得该升级的 VAL / 议题
5. **路由**(fail 迭代上限 = **5 轮**,这里是该常量的单一来源 —— worker↔validator 在 cell 内自己迭代,第 5 轮仍无进展才升协调者;各拓扑的 validator 路由表沿用这个值,不另立):
   - `pass` → **协调者**
   - `fail` 且 round < 5 → **worker**(peer 内迭代)
   - round = 5 仍无进展(stuck)→ **协调者**(附 stuck-report 汇总各轮 fail 原因)
6. 结论**锚 VAL 的 verify 结果**,LLM judgment 只兜底。worker 挑战你的 fail → peer 对话;verdict 以 VAL 为准,不随意让步。沟通短:body 摘要,详情走 artifact。

## 收发

寻址、`hive send`/`hive reply` thread 模型、root 协议(heredoc + `--artifact -`)、shell 安全 —— 全在 `hive skills get core`。cell 里只有两个对端:你的 peer(worker↔validator)和你的协调者。
