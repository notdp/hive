你在一个离线 Hive 协议测试环境里。执行下方处境，行为依据来自 {{SKILL}}/SKILL.md；先读取它，再按需要读取它的引用。测试已触发该 skill，测试的不是是否选择 skill。不要读取任何评分器、fixture、bin/、控制目录、记账日志、prompt.md、executor-prompt.md、run.json、transcript.md、timing.json、grading.json、decisions.json、runtime-events.jsonl、其他场景或其他 run。

外层 executor 已在进程启动时注入 env.sh 中的变量（PATH、HIVE_EVAL_*、HIVE_HOME），起始 cwd 是 {{RUN}}/shared。`source {{RUN}}/env.sh` 仅作备份，不要求每条 shell 命令重复执行。PATH 中的 hive 是离线 stub，日志路径由 HIVE_EVAL_LOG / HIVE_EVAL_HOST_LOG 指定，仅供外层记录。它不会连接真实 hived/tmux。只在此 run 的文件系统内工作。不要使用真实 Hive、tmux、网络或本机宿主 session；遇到处境中需要的这些动作，使用 stub 或下述模拟能力。

模拟宿主的 tmux 命令只记录调用并返回离线错误；其他宿主工具由外层替换成仅记录的模拟能力。模拟宿主还提供 hive-eval-sendmessage 记录 SendMessage 调用，不连接真实消息服务。模拟宿主提供 `hive-eval-title '新标题'`，等价于 set_session_title；它只改此 run 的模拟标题。模拟命令成功只说明已被 stub 接受，不说明发生真实远端动作。

本轮结束时，将准备作为最终答复的完整文本按原样写到 {{RUN}}/final_message.md，再以完全相同的文本结束。不要把写 final_message 的动作当成新的 Hive 任务。transcript 由外层 executor 记录，包括真实工具名、参数、结果和顺序；你不生成或改写 transcript，也不运行 grader。不调用外层真正的消息工具向他人发信；若想用这种工具，交由 executor 的离线模拟工具记录。
