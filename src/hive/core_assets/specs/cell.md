# cell — 协作原子(worker + validator)

> 先读 `hive skills get core`(通信底座 + 挑战立场)。

cell 是 Hive 的最小协作单元:一个 **worker**(producer,干活)+ 一个异构 **validator**(reviewer,先共定 plan+VAL,后审 code),两人 loop 到 pass。`crew` 拓扑就是 orch 编排 N 个 cell。

## 角色钉死

worker 永远干活,validator 永远审 —— 出生即定,不协商角色(协商角色是纯浪费 turn)。唯一的越界许可:**validator 发现的 bug、worker 认账了,validator 可以直接改**。

## 协调者(coordinator)

cell 出的结论(pass verdict / 卡死 escalation)交给**「你的协调者」**:

- standalone cell → 协调者就是**人**
- 在 crew 里 → 协调者 = **challenger → orch**(见 `hive skills get crew`)

本 spec 把协调者留抽象;具体寻址由拓扑填。cell 内部的 fail 迭代**不**经协调者,worker ↔ validator 自己闭环。

## worker(producer)

1. **先收敛 plan+VAL,再动手**(主 checkout 纯文本,不开 worktree):
   - 你出 **plan 草案**(拆解 / 方案 / 风险)发 validator;validator 挑 plan 并**主笔 VAL**(可执行的验收命令 / 断言)。主笔对偶是独立性的来源:方案由干活的人设计,考题由监考的人出 —— 你不给自己定验收标准。
   - **plan 与 VAL 绑定定稿**:收敛产物是一个包,同时锁定;之后任何一边要改,两边一起审、留记录,不允许各漂各的。
   - 收敛上限沿用 cell 的 **5 轮**(单一常量,见 validator 路由),到限收敛不了 → 升协调者。
   - **轻任务一回合化**:小修可以把 plan 草案和 **VAL 建议**压在一条消息里(`plan: 直接修;VAL 建议: pytest xxx 过`);validator 原样确认或改写,**确认后的 VAL 才算定稿** —— 流程定形态不定重量,但最终验收标准仍不由你定。
   - 定稿包发协调者一份快照,**默认不阻塞**继续开干;human 随时可叫停,派活时说明「plan 先过我」则升级为阻塞 gate。
   - crew 里 VAL 已由 orch 随任务发到:此步退化为「你出 plan 草案 + validator 对照已有 VAL 挑 plan」,不重写 VAL;VAL 本身有漏报给 validator → challenger 链路。
2. **以 worktree 为始**:plan 定稿后跑 `hive worktree start <feature>`(feature 名 = branch 名,唯一标识这件事),stdout 给路径;然后**自己进入** —— claude 用 `EnterWorktree path=<路径>`,codex / droid 用 `cd <路径>`。base 自动解析(crew → 集成分支;standalone → default branch;解析不出会硬失败要 `--base`)。`start` 报 `needs-rebase`(exit 1)→ 进 worktree rebase 到它提示的 base,再跑一次 `start`。
3. 实现任务(Edit / Write / Bash)。
4. **最小 self-check**(只做这层 smoke,全套验收是 validator 的):
   - 语法 / 类型 / import(`python3 -c "import hive"` 级)
   - 本任务的 1–2 条 happy-path smoke(看 exit code / 返回结构)
5. 写 handoff artifact,发给你的 validator。字段(droid `uyH` schema 简化):
   - `successState` ∈ `{success, partial, failure}`
   - `salientSummary`:1–4 句、≤500 字,这次 handoff 的核心结论
   - `whatWasImplemented`:改了哪些文件、跑了哪些命令(必填,非空)
   - `whatWasLeftUndone`:没做完的(必填;全做完写 `"none"`)
   - `verification`:你跑过的 smoke,每条 `{command, exitCode, observation}`
   - `tests`:新增 / 改动的测试文件 + 关键用例路径(**不自己跑全套**,列给 validator)
   - `discoveredIssues`:每条 `{severity ∈ {low,medium,high,critical}, description, suggestedFix?}`(无则省略)
6. validator 判 fail → 按它给的 `required-changes` 改,再 handoff。loop 到 pass。
7. **不自己宣布完成**;completion 由 validator 的 pass verdict 定义。
8. **以 PR 收束**(validator pass 后):按你环境约定的 commit/PR 流程开 PR —— standalone cell 是 feature → default branch;crew 里是 sub-PR → 集成分支,**显式 `--base <集成分支>`**,开完跑 `gh pr view --json baseRefName` 确认落基正确(`start` 写的 `gh-merge-base` config 只是漏传时的兜底)。PR / push 是不可逆外部副作用,须经 human 授权。
9. **退场**:先离开 worktree(claude `ExitWorktree action=keep`,codex / droid `cd` 回主 checkout),再 `hive worktree done <feature>` —— 只删 worktree,**branch 留给 PR 生命周期**。`done --force` 会丢未提交工作:只有协调者 / human 明确 abandon 这条 feature 才用,并先核对它输出的 status 摘要。

**为什么 worker 不跑全套**:跨 agent 重复 pytest 只是让 validator 复读同样命令、浪费资源;worker 看到 test fail 容易陷入「改 test 让它过,而不是改实现」的死循环。职责边界清楚:worker 实现,validator 验收。

注意:「不越权」**不等于**「不做基础卫生」。项目要求的测试前置 / 隔离环境该用还得用,确保 self-check 跑在目标代码上,但不要把未完成的开发 checkout 装进 live 通信环境。

## validator(reviewer 审 plan + code)

沿用 core 的**挑战立场**。你先审 worker 的 plan 并主笔 VAL,后审 handoff 出 verdict;不写功能码(除「worker 认账的 bug 你直接改」那条)。

1. **plan 阶段(开干前)**:worker 发来 plan 草案 → 你挑它(拆解对不对、风险漏没漏),并**主笔 VAL** —— 可执行的验收命令 / 断言,能真证伪,不写「考虑更多边界」这类空话。plan 与 VAL **绑定定稿**(一个包同时锁定);收敛上限同 5 轮,到限升协调者。standalone 如此;crew 里 VAL 由 orch 随任务发到,你对照它挑 plan 即可,VAL 本身的漏走你 → challenger 链路反馈。
2. **证据面固定**:handoff artifact + VAL(验收标准)。只看 worker 写下的最终产物,**不借 worker pane 的运行 transcript** —— 独立性的来源就是这条,不然会被 worker 的叙事同化。
3. **验收对象在 worker 的 worktree 里**:`hive worktree status <feature>` 拿路径;只读进入(claude `EnterWorktree path=<路径>`,codex / droid `cd`),或不进去直接 `git -C <路径> diff / log`。不在 worker 的 worktree 里写东西。
4. **三层 verify,越客观越先跑,前一层 fail 就停、不下钻**:
   1. **Rule-based** — 跑 handoff `verification` 里的命令 + VAL 的 `verify:` 命令,对 exit code / stdout
   2. **Visual / behavioral** — 仅当 VAL 涉及 UI 或可观察状态时,按描述跑交互看现象
   3. **LLM judgment** — 仅当前两层都过、但 intent 有歧义时,你读 diff 判「实现是否真符合 VAL 精神」
5. **追踪 round**:读上一轮自己写的 fail-feedback 取 `round=N-1`,本轮 N;worker 初 handoff 无 round 字段时默认 round=1。
6. 写 verdict artifact,字段:
   - `verdict` ∈ `{pass, fail}`
   - `round`:本轮编号 N(必填,供审计 / 下一轮读)
   - `failureClass`:(if fail)∈ `{rule-violation, approach-disagreement, incomplete}`
   - `evidence`:跑了哪些命令、看了哪些文件、exit code / 关键输出(必填)
   - `required-changes`:(if fail)要 worker 改的具体 bullet list
   - `openQuestion`:(optional)你觉得该升级的 VAL / 议题
7. **路由**(fail 迭代上限 = **5 轮**,这里是该常量的单一来源 —— plan 收敛与 fail 迭代都用它;worker↔validator 在 cell 内自己迭代,第 5 轮仍无进展才升协调者;各拓扑的 validator 路由表沿用这个值,不另立):
   - `pass` → **协调者**
   - `fail` 且 round < 5 → **worker**(peer 内迭代)
   - round = 5 仍无进展(stuck)→ **协调者**(附 stuck-report 汇总各轮 fail 原因)
8. 结论**锚 VAL 的 verify 结果**,LLM judgment 只兜底。**VAL 是底线不是天花板**:VAL 之外抓到的真问题照样 fail(`failureClass` 标清楚);实现中发现 VAL 本身错 / 漏 → 双方同意才升级,且 **plan 与 VAL 绑定同步改、留记录**,不允许单边漂移。worker 挑战你的 fail → peer 对话;verdict 以 VAL 为准,不随意让步。沟通短:body 摘要,详情走 artifact。

## 收发

寻址、`hive send`/`hive reply` thread 模型、root 协议(heredoc + `--artifact -`)、shell 安全 —— 全在 `hive skills get core`。cell 里只有两个对端:你的 peer(worker↔validator)和你的协调者。
