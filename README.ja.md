# Hive

> CLI エージェント向けの tmux ベース協調ランタイム — `claude` と `codex` が、インライン `<HIVE>` メッセージ、配信トラッキング、ハンドオフスレッドを通じて会話します。

[English](README.md) · [简体中文](README.zh-CN.md) · **日本語**

_この README は英語版が正となります。翻訳は canonical 版に対して遅れることがあります。_

## Hive とは

Hive はエージェント向けのランタイムであり、人間が手で叩く CLI ではありません。日々の協調作業 — メッセージの送信、スレッドへの返信、タスクのハンドオフ、配信の追跡 — はすべてエージェントセッション内で行われ、コマンドを発行するのもエージェントです。人間にとっての日常的な主な入口は `/hive` で、これを打つとエージェントに Hive スキルがロードされ、チームが自動でブートストラップされます。

プラグインの有効化、スキルのドリフト確認、ポップアップエディタ (`hive cvim` / `hive vim`)、ローカル開発セットアップ — この範囲のコマンドだけは人間側に残ります。

## インストール

```bash
# Hive CLI
pipx install git+https://github.com/notdp/hive.git

# Hive スキル (Claude Code / Codex 向け)
npx skills add https://github.com/notdp/hive -g --all
```

必要な環境:

- `tmux` (`hive cvim` / `hive vim` のポップアップヘルパーには 3.2 以上が必要)
- Python 3.11 以上
- 少なくとも 1 つのエージェント CLI: `claude` / `codex`

## エージェントセッションで起動する

```bash
# tmux 内で好きなエージェントを起動
$ claude       # もしくは: codex

# エージェントセッションで以下を入力:
/hive
```

スキルがロードされ、エージェントが `hive init` を実行して現在の tmux ウィンドウをチームにバインドし、異なるモデルファミリのアイドル peer と自動ペアリングします — 既存の peer があればアタッチ、なければ新しい pane を起動します。これ以降はあなたがエージェントと話し、エージェントが peer と話します。

## オペレータ向けコマンド

人間が実行することの多いコマンド:

```bash
# プラグイン
hive plugin enable notify         # 人間向けデスクトップ通知
hive plugin enable code-review    # マルチエージェント code review ワークフロー
hive plugin list

# 診断
hive doctor --skills              # アップグレード後のスキルドリフトを確認

# ポップアップエディタ (tmux 3.2+)
hive cvim                         # tmux ポップアップエディタ
hive vim                          # 単一 pane 版

# 現在のエージェントセッションを分割 pane にフォーク
hive fork                         # 分割方向を自動判定
hive vfork                        # 垂直分割
hive hfork                        # 水平分割
```

Claude Code / Codex 内から叩く場合はシェルエスケープで: `!hive cvim`、`!hive vfork`、`!hive fork` など。

`hive fork` をキーボードショートカットにバインドすると tmux と相性が良くなります。例 (macOS 上の Ghostty + tmux) — Cmd+Shift+F で現在の pane をフォークします。お使いのターミナルに合わせてキーは変更してください:

```
# ~/.config/ghostty/config
keybind = cmd+shift+f=text:\x1bf

# ~/.tmux.conf
bind -n M-f run-shell -b 'hive fork --pane "#{pane_id}"'
```

それ以外の `hive send` / `hive reply` / `hive team` / `hive doctor <agent>` / `hive handoff` などは、エージェントが呼び出す前提で設計されています。自分で実行しても動きますが、ハッピーパスではなくデバッグ・応用パスです。

## アップグレード

```bash
pipx upgrade hive           # CLI をアップグレード
npx skills update hive -g   # スキルをアップグレード (GitHub 経由インストール限定)
```

CLI とスキルは独立してアップグレードされます。CLI をアップグレードしてもスキルは自動で更新されません。スキルが古いままだと、エージェント pane から `hive` コマンドを実行したときに stderr で警告が出て、`hive doctor --skills` で差分が確認できます。

ローカルチェックアウトの場合、`skills update` ではインストールを更新できません — 下の「コントリビュータ向け」節を参照してください。

## コントリビュータ向け

GitHub 経由ではなく、現在のチェックアウトからインストール:

```bash
python3 -m pip install -e .
npx skills add "$PWD" -g --all     # ローカルチェックアウトは `skills update` で追跡されません; 更新時はこれを再実行
PYTHONPATH=src python -m pytest tests/ -q
```

完全なポスト編集ワークフロー (install + skill 更新 + plugin 再有効化) およびリポジトリ規約は [AGENTS.md](AGENTS.md) を参照。

## ドキュメント

- [`docs/runtime-model.md`](docs/runtime-model.md) — ランタイムフィールドの意味論 (`busy`, `inputState`, `turnPhase`)
- [`docs/transcript-signals.md`](docs/transcript-signals.md) — Claude のトランスクリプト解析ルール
- [`skills/hive/SKILL.md`](skills/hive/SKILL.md) — エージェントの挙動 / prompt 契約 (Hive スキルがランタイムでロード)

## ライセンス

[GPL-3.0-or-later](LICENSE) © 2026 notdp
