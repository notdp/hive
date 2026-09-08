读取 {{FILES}}/inventory.tsv，读完运行 hive-eval-checkpoint inventory-read。按编号字典序把 state=active 的 id 写到 {{RUN}}/outputs/active-ids.txt，每行一个编号。输入文件只读。final 写清单和绝对路径。
