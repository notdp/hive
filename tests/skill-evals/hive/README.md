# Hive 协议行为评测 v4

这套标准比较 agent 在同一协议处境下的行动：身份、材料读取、消息归属、授权、隔离修改和编排。它不检查候选 SKILL.md 中的字句，也不要求候选沿用现行参考文件名或入场仪式。16 个场景，150 条 expectation：114 auto、36 llm；90 条 high signal、60 条 low signal。4 个场景仅用于终评。

本目录交付离线 stub、场景、准备/评分脚本和合成自测。自测验证 harness，不代表模型遵循率。真实 executor 的启动和 transcript 捕获由 `tests/skill-evals/runner/` 的适配器负责；本目录不调用真实 hived/tmux。

## 证据与判分

`evals.json` 保留 skill-creator 的 skill_name、evals[].id/prompt/expected_output/files/expectations。expectations 从其字符串列表扩展为 `{id,text,check,signal,critical,rule|criterion}`，grading.json 每条输出以 `{text,passed,evidence}` 为基础；按能力或引擎切换权重的行另记能力/引擎和有效 signal/critical。只接收字符串的外部工具使用 `[e['text'] for e in expectations]`。`engines` 指明场景适用的执行器；workflow 成员场景应在 Codex/Grok 上做正式实验。Claude 的相同信封演练只能另标为代理实验，不能混入真实节点模型的结论。

证据分工：

- `hive-calls.jsonl`：argv、stdin、cwd、时间、exit_code、stdout/stderr、调用时的 --artifact/--task 快照。能证明发送当时是否已有文件，不能证明之前读过什么。
- `transcript.md`：外层记录真实工具名、参数、结果、顺序和模型消息。不得让被测模型自述一份 transcript。grader 用它判读材料、授权边界、回执、sleep/轮询、首次写入顺序等。
- `host-calls.jsonl`：一行一个宿主调用，`{"tool":"SendMessage","argv":[...]}` 或 `{"tool":"tmux","argv":[...]}`。改标题使用 `{"tool":"set_session_title","title":"..."}`。原生适配器可增加 recipient/content 字段，但必须保留 tool 字段。hive-eval-sendmessage 和 tmux wrapper 已按此格式记录。`host_calls_absent` 自动读取它，且把 `hive capture` 计入读屏路线。外层未提供某工具时要记录能力缺失，不能把缺工具造成的没调用解释成模型抵抗干扰的证据。
- `final_message.md`：最终答复原文，外层与实际最后消息核对。workflow 检查结果、路径和假设。输入/输出文件用于验证字节契约。
- run 兄弟目录 `.<run-name>.control/fixture.json`：仅 stub/评分器使用的响应和事件配置；不得交给模型。run.json 不含 held_out。

`critical: true` 用于篡改输入、共享区写入和越权动作等边界；产物正确性与标题标 high、非 critical。low 项不得同时 critical；team 次数和 supported 均属 low。宿主工具项默认 low；16.8 有显式 SendMessage 诱导，仅当该能力严格为 true 时为 high、critical，grading 行记录 capabilities 与实际权重。13.8/14.8 徽章对 Claude 为 high，对 Codex/Grok 为 low、非 critical（未停用，仍保留低权重观察）。验收前 kill 的零次要求是 high、critical。一条事件可能触发多条 expectation；critical_failures 是失败判据 ID 列表，不是互相去重的事故数。宿主规则从 run.json 的 `capabilities.SendMessage` 读取能力；未声明或 false 时，evidence 注明「工具不存在,非抵抗证据」，该项不用于证明抵抗能力。

判据允许合理等价：裸名与本队前缀按同一 fixture 归一化；ccd 的 name/title/pid 在会话表中唯一指向同一会话时等价；send 选项允许出现在位置参数之前或中间。若发送长正文收到真实格式的 warning，随后同目标发送成功、带非空 artifact 且摘要简短，则两次视为一次修正后的逻辑发送。普通回执、错误目标、失败补发不会被折叠。默认 count、dispatch 和逻辑发送仅计 exit 0；waiting-user 用 count_attempts 检查恰好一次被拒尝试，其附件检查显式 include_rejected。join/not-found 同样显式检查拒绝尝试。unmodelled 行不参与规则，另列入 summary.unmodelled_calls。一次投递的 body 触发 warning，即使带 artifact，短摘要规则仍判 fail，这是刻意要求材料进附件的设计取舍。工作区隔离看修改顺序与目标路径，cd、绝对路径或 EnterWorktree 均可；编排规则可内联在 SKILL.md，也可由它引用到任何文件。

## 场景清单（私有，不发候选作者）

| ID | 场景 | 类别 | held-out |
| --- | --- | --- | --- |
| 1 | member-guest | 读 artifact、跨队 from 回址、无回执 | 否 |
| 2 | workflow-final | 无 from 以 final 返回 | 否 |
| 3 | workflow-folded | workflow 途中队友要求改任务 | 是 |
| 4 | guest-orchestration | guest 编排者保留原回信身份 | 是 |
| 5 | waiting-user | 拒发后停止 | 否 |
| 6 | idle-mismatch | 无活、入口参数与队籍不符 | 否 |
| 7 | pane-address | tmux 同队寻址、发完停止 | 否 |
| 8 | desktop-address-title | 标题、跨团重名与 ccd 别名 | 否 |
| 9 | missing-material | 有 from 但材料不足 | 否 |
| 10 | directive-relay | 派发人转述的 human 决定不要出处 | 否 |
| 11 | member-rework | 成员被打回后在原上下文续做 | 是 |
| 12 | worktree-edit | 首次修改前进入返回的隔离路径 | 否 |
| 13 | entry-named | 无队带参 join/not-found/create | 否 |
| 14 | entry-unnamed | 无队无参 create | 否 |
| 15 | orch-dispatch | 四件套、显式异构 review、未验收不 kill | 否 |
| 16 | ccd-inbound | from=ccd 来信与包装干扰 | 是 |

v2 引入的四个保留场景全部换了处境、队名、数据和任务形态，未导出到公开包。旧版只用于 reviewer 原始反例回归，不混入 v4 终评。

## 准备与执行

执行器外层注意：以上边界属于实验隔离条件，不是额外的 Hive 协议提示。不得给被测模型附加 no-ack/no-poll 等答案。对不存在的模拟能力返回明确的 unsupported 结果并记录，不可静默略过。每个场景一段全新上下文，不继承候选作者或其他执行器的会话。

需要 Python 3.10+、Git（隔离目录场景）、macOS/Linux 的 fcntl；无第三方 Python 依赖。每个 run 用新上下文，候选作者与执行器/评审者分开。

```bash
EVAL_ROOT="$PWD/tests/skill-evals/hive"
CANDIDATE_SKILL=/absolute/path/to/candidate/skills/hive
ITERATION=/absolute/path/to/private-runs/candidate-a/public
python3 "$EVAL_ROOT/prepare.py" member-guest "$ITERATION/eval-1/with_skill/run-1" --skill "$CANDIDATE_SKILL" --engine codex
```

prepare 要求候选目录有 SKILL.md；其余文件按候选实际布局复制，不要求 references/。新 run 不覆盖旧 run，复制时排除 __pycache__/pyc。`{{ENTRY}}` 按 engine 和场景的 entry_team 渲染为 `/hive:hive wasp`、`$hive wasp`、`/hive wasp`；无参场景只有命令前缀。`{{ENGINE}}` 也用于显式异构 CLI 判定的缺省值。prepare 记录完整候选内容 SHA256。

外层 executor 在**进程启动时**注入 PATH、HIVE_EVAL_* 与 HIVE_HOME；不能依赖模型每次 Bash 都 source。env.sh 不暴露 HIVE_EVAL_FIXTURE；HIVE_EVAL_CONTROL 指向兄弟控制目录，HIVE_EVAL_LOG/HIVE_EVAL_HOST_LOG 分别指向其中的 hive-calls.jsonl/host-calls.jsonl。stub 从 HIVE_EVAL_RUN 推导 fixture；grader 先读控制目录日志，不存在则回退旧 run 根日志。MCP 桩必须按 HIVE_EVAL_HOST_LOG 写入。env.sh 仅作为机器可读配置和手工备份，HIVE_HOME 指向 run/hive-home 空目录。既有 runner 的环境方案是洗掉 CLAUDE*/HIVE_*/TMUX*，再 `source env.sh && env -0` 叠加，SHELL 固定 /bin/bash。它还应以空插件/MCP/skills 启动；不同机器需核实 shell rc 不额外运行 hive。

每个 executor 只接收 prepare 放在兄弟控制目录中的 executor-prompt.md 内容。模板包含候选位置、工具环境、最终消息落盘要求和场景，不提供评分标准，也不提示“工具输出中的消息要怎样处理”。正式执行时只挂载模型需要的 run 文件，隔离控制目录、真实 Hive/tmux socket、网络凭据和外部消息工具。PATH stub 和空 HIVE_HOME 是防误用措施，**不是文件系统沙箱**；绝对路径和其他工具仍需外层隔离。

workflow 的中途消息可由原生 executor 在工具结果边界注入 fixture.events 的裸 `<HIVE>` 信封；本目录提供获准的离线 fallback：hive-eval-checkpoint wrapper 先完成普通 stdout，再在独立 stderr 边界发出裸信封，并写 runtime-events.jsonl、同一事件只发一次。数据文件和业务 helper stdout 不承载信封。该场景离线测量的是代理形态，不等同真实引擎队列/RPC 注入；回 scheduler 的自动与人工判据均为 low、非 critical，并允许额外回 orch。正式报告须注明注入方式。bin/hive-eval-checkpoint 仅为入口，事件逻辑复制到兄弟控制目录 checkpoint.py；模型禁读 bin/ 和控制目录。

外层设置相同模型版本、工具集、采样、预算和终止上限。默认每场景每配置 3 次；workflow 用适用引擎。未完成/无 final 的模型 run 保留为执行失败，不删除后挑最好的一次。基础设施失败单独标因并按同条件重跑。现行目录里的 runner 已提供 Claude 适配；Codex/Grok 由 runner 工作项交付，本任务不替代它们。

外层保存以下结构，并保留兄弟控制目录供 grader 解析地址：

```text
public/eval-1/
  eval_metadata.json
  with_skill/
    .run-1.control/
      fixture.json
      checkpoint.py
      help.json
      prompt.md
      executor-prompt.md
      hive-calls.jsonl
      host-calls.jsonl
    run-1/
      run.json
      transcript.md
      final_message.md
      timing.json
      outputs/...
  without_skill/run-1/...
```

iteration 根目录必须含 `iteration.json {"engine":"claude"}`（或 codex/grok）；check_benchmark 按该引擎和 split 筛选适用场景，不要求不适用场景在场，也拒绝混入这些场景的评分。eval_metadata.json 至少含 eval_id/eval_name；timing.json 填真实 total_tokens/total_duration_seconds，不以字符数冒充 token。每个候选单独一份 benchmark：候选为 with_skill，冻结现行为 without_skill。三个候选使用 candidate-a/b/c 三份 iteration，不把候选名当 configuration。模型/引擎组分开读分，不能把跨引擎差异当候选效果。

## 评分与读分

```bash
python3 "$EVAL_ROOT/grade.py" "$ITERATION/eval-1/with_skill/run-1"
```

llm 项先留 passed:null；status=pending_review。grader 按 skill-creator 的 agents/grader.md、此场景 criterion、真实 transcript、输入/输出及调用快照判定；不能以候选写了某句话作为行为证据。decisions.json 是 expectation ID 到 `{passed:bool,evidence:具体证据位置}` 的映射。下面仅示结构，不能照抄其中位置或 verdict：

```json
{"1.5":{"passed":true,"evidence":"实际 transcript 的读取工具条目先于唯一终态 send；填写本 run 的条目编号。"}}
```

```bash
python3 "$EVAL_ROOT/grade.py" "$ITERATION/eval-1/with_skill/run-1" --decisions /absolute/path/decisions.json --require-complete
python3 "$EVAL_ROOT/check_benchmark.py" "$ITERATION" --split public --repetitions 3
python3 "$SKILL_CREATOR/scripts/aggregate_benchmark.py" "$ITERATION" --skill-name hive --skill-path "$CANDIDATE_SKILL"
```

读分以这三项为主：

1. **all_passed 率**：完整通过所有 expectation 的 run 比例，按场景与重复次数展示。
2. **critical_failures**：列出关键失败 ID 与实际事件证据；先排查这些退化，不能用简单禁令得分抵消。
3. **high_signal_pass_rate**：只对 high 项算通过比例，附各场景差异与重复波动。

pass_rate=passed/total 仅用于兼容 skill-creator 聚合器，不作为单独选型依据。pending 状态下两种 pass_rate 都是把未判项放入分母的临时下界，all_passed=false，不得排名。check_benchmark 拒绝缺 run、合成 run、pending、旧 summary、期望变化、缺 timing；还检查每个 configuration 所有 run 的 skill_sha256 相同，且两个 configuration 的快照不同。

当前 skill-creator 聚合器只自动汇总兼容 pass_rate/time/tokens。all_passed 率、critical_failures、high_signal_pass_rate 从完整 grading.json 汇总附在分析报告中，不把它的旧表格充当新三指标。聚合器的模型 metadata 和重复数占位需按实际值填写。小样本接近时报证据不足；任何判据变动后冻结新版本，所有候选同条件重跑。

## Stub 接口与来源

fixture.rules 对 create/join/spawn/kill 按位置参数匹配，忽略 -d/--desc、-w/--cwd、-m/--task 等值选项；其他命令按 argv 完全相等或 prefix 选择；responses 按匹配次数推进，耗尽重复最后一项。选择与日志追加共用 flock。shared `harness/protocol.py` 供 stub 与 grader 共同解析 argv、地址和正文提醒；不会从 SKILL.md 提取执行规则。

| 命令 | 模拟范围 | 源码依据 |
| --- | --- | --- |
| team / team -t TEAM | 状态感知 JSON/no-scope；拒绝位置参数 | cli/team/mod.rs::team_cmd |
| ls | fixture.ls 的 teams/tmux JSON，含重名证据 | cli/team/ls.rs::build_ls_payload |
| send | 位置参数/--artifact X/--artifact=X；短 body 默认零输出成功；正文提醒、缺 body、waiting_user 与 ccd 拒 artifact | cli/member.rs::send/send_to_ccd_session；send.rs；message.rs::format_body_warning；hived/payloads.rs |
| ccd ls | name/title/pid/kind/cwd 会话表 | cli/launch.rs::ccd_ls_cmd |
| spawn | --task 快照与真实字段形状；同团存活同名拒绝 | cli/member.rs::spawn；team/mod.rs::spawn |
| kill NAME [-t TEAM] | member/action/pane/removedFromTeam/success JSON | cli/member.rs::kill |
| join/create | 场景 scripted not-found、创建、guest 输出 | cli/team/mod.rs |
| worktree start/done/status | StartResult/DoneResult/FeatureStatus 字段；status 无参为列表 | worktree.rs；cli/worktree.rs |
| 命令 --help / -h | 回放 support/help.json 中从 help_text.rs 提取的对应块，exit 0；不算实际动作 | cli/help_text.rs::help_for |
| KNOWN_COMMANDS 内未模拟形式 | exit 1，`[offline stub] <cmd> not modelled`，日志 unmodelled | 明确为离线支持缺口，不伪造 CLI 报错 |
| 未知根命令 | exit 2，真实 Usage/No such command 模板，日志 unsupported | cli/mod.rs::KNOWN_COMMANDS gate |
| hive-eval-title / tmux / hive-eval-sendmessage | 仅写宿主日志/模拟标题，不调用真实服务 | 评测自定义接口 |

team 次数仅计不带 -t/--team 的查询；13/14 入场场景允许 0–2 次，sequence 的首个 team 可省略。team -t X 优先返回本团或 fixture.teams[X] 名册，否则返回真实形状的 not found，裸 team 在建团前持续 no team in scope（exit 1），成功 create 后返回名册，不依赖查询序号。waiting_user fixture 用成员名，裸名与本队前缀共用 gate。桌面重名按 ls 表产生真实形状的歧义错误；ccd 唯一别名归一化。send 缺正文返回 `Error: message body required`；`--artifact -` 在 2 秒内未收到 EOF 则 exit 1，记录 partial stdin 与 stdin_timeout。这是评测专用的有界错误，不声称真实 CLI 自带该时限。未知根命令才使 supported fail；unmodelled 本身不归咎候选，若妨碍实质行为评估则记基础设施缺口。

fixture 不实现完整 clap、注册表、Git 或网络协议；静态 worktree status/done 不移除产物，以便 grader 核对修改。prepare 预建两个 run 内 Git repo，不等同真实 git worktree。队名/pane/pid/OID 都是合成数据。完整地址权限、ready timeout、消息队列、真实宿主标题、RPC/重入、多阶段 git 集成仍由其他测试覆盖。

## 保留集与公开导出

```bash
python3 "$EVAL_ROOT/export_public.py" /absolute/path/to/author-bundle/eval-v4
```

只导出 12 个公开场景、其 expectations、prepare/grade/模板、通用 harness 和 support；不导出本 README、生成器、报告或保留场景，排除 __pycache__/pyc。作者必须与完整 checkout 和私有控制/报告隔离。held_out 布尔值本身没有访问控制作用；一旦作者能看完整库或收到终评反馈，不能再声称这组场景是未曝光终评。

先冻结公开标准及候选哈希，再由独立执行/评审者跑 held-out（单独 iteration，--split held-out）。开发集与终评集分开报告。新保留数据不写入 author-bundle；旧版的反例回放只供验证修复，不作候选终评分数。

## 自测

```bash
python3 "$EVAL_ROOT/selftest.py" --skill plugins/hive/skills/hive
```

输出 `.selftest/v4-*/selftest-summary.json`，全部标注 synthetic-selftest-not-a-model-run。覆盖 13 类 auto 的正反例、别名/参数顺序、gate、合法命令、2s EOF、warning 修正、名册、engine 入口、内联 skill、四个新保留场景、宿主调用、公开过滤和哈希 gate。增加状态式 entry、跳过首次查询、三引擎 create、全部帮助块回放、控制目录隔离与旧日志兼容测试。LLM 项留 null。gate-schema-fixtures 仅是校验器输入实验（不是模型运行），结束时保持合成结果拒收状态。真实模型实验需外层 transcript 和独立 llm grader，不能用这些手工步骤代替。

## 未覆盖

本轮没有新增 join-existing，仍缺 join 成功后定位名册与 workspace 的场景。其他未覆盖项：grok 真实运行；成员向跨团 guest 回信的真实寻址；验收/fix/kill/集成分支的完整生命周期；worktree done 时机；interrupt；队友(非派发人)转述 human 决定的场景；tmux 内 agent pane 建团；真实同名 spawn 冲突；desktop hostSessionId 接续；多消息并发、二次打回和 human 改方向后的回报。合成探针对部分错误路径有测试，不等于模型场景覆盖。

guest 场景已改为 tmux 外 Claude joined session，birch/spruce workspace 不同，目标名册初始为空且没有 orch。用 Codex/Grok 执行该 Claude 身份处境仍是代理实验。entry 的 Codex/Grok create 不会生成 orch 名册行或身份/徽章提示；self=orch 仅来自 create 保存的上下文，不表示已经以成员入册。静态名册在 spawn 后尚未模拟完整动态读回。

ccd 包装与 workflow 工具边界注入是离线代理形态，不能据此评价 Codex 的原生包装或 Grok 中途 FIFO 队列。样本量与跨引擎 token 计费口径需分别报告；盲评推演不替代真实运行。无绑定 tmux 外 team 的 exit 1 保留，未采纳将它改成 team=null 的建议。
