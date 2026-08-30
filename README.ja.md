# Hive

> CLI エージェント向けの tmux ファーストな協調ランタイム — `claude` / `codex` / `grok` の各メンバーはそれぞれ自前のエンジンで動き、エンジン固有のネイティブ配信経路で `<HIVE>` メッセージをやり取りし、単一のレジストリを真実の層として共有します。

[English](README.md) · [简体中文](README.zh-CN.md) · **日本語**

_この README は英語版が正となります。翻訳は canonical 版に対して遅れることがあります。_

## Hive とは

Hive はエージェント向けのランタイムであり、人間が手で叩く CLI ではありません。チームの実体はレジストリ上の名簿（`$HIVE_HOME/state/teams/` にチームごと 1 つの JSON）とメンバーごとのエンジンであり、tmux ウィンドウはその上に `hive attach` が描き出す表示層にすぎません。

この境界は実装で担保されています。tmux の外で `hive create` すれば headless なチームが登録され、表示層がなければ `hive spawn` は pane を持たないエンジンだけのメンバーを立ち上げます — 受信も報告も kill も通常どおりです。ウィンドウは後から `hive attach <team>` でメンバー 1 人につき 1 pane として実体化できます。tmux 側に真実は一切ないので、ウィンドウを閉じても失うものはありません。

タスクの割り当て、メッセージ送信、ランタイム状態の確認といった日々の作業はエージェントセッション内で行われ、コマンドを発行するのもエージェントです。人間側の入口はプラグインスキル `/hive:hive [team]` です。引数なしなら状況に応じて作成または参加、チーム名を渡せばそのチームに参加し、存在しなければ作成します。人間側に残るのは一部のコマンドだけです: プラグインのインストール、セッショントランスクリプトの閲覧 (`hive view`)、ポップアップエディタ (`hive cvim` / `hive vim`)、分割 fork、そしてローカル開発のセットアップ。

## インストール

Hive は単一の Rust バイナリで、チェックアウトからビルドします:

```bash
git clone https://github.com/notdp/hive.git
cd hive
cargo install --path crates/hive
```

このリポジトリは両 CLI 向けのプラグイン marketplace でもあります。プラグインが配るのは、エージェントに協調プロトコルを教えるスキルです:

```bash
# Claude Code
claude plugin marketplace add notdp/hive
claude plugin install hive@hive

# Codex
codex plugin marketplace add https://github.com/notdp/hive.git
codex plugin add hive@hive
```

CLI は先に自分で入れてください。プラグインの `SessionStart` フックは代わりに入れてくれそうに見えて、実際には入りません。収束のほうは今も `pipx install` でこのリポジトリを叩きますが、これは Rust 移行前の経路で、以降このリポジトリに `pyproject.toml` はありません。よってバイナリは生成されず、フック後半の処理（Claude 側 marketplace 自動更新の有効化）にも到達しません。PATH 上に十分新しい `hive` が既にある場合だけ、何もインストールせずそのまま先へ進みます。それが唯一収束する経路です。

必要な環境:

- `tmux` 3.2 以上 — `hive cvim` / `hive vim` のポップアップのため。加えて `hive view` がテーマ判定に使う素の OSC 11 背景色クエリに pane が応答するのも 3.2 からです
- ビルド用の Rust ツールチェイン
- `python3` — `hive flow run` はインタプリタを exec してスクリプトを走らせ、対になる `hive.flow` クライアントは埋め込み済みです。notify のポップアップも python のヒアドキュメントです
- 少なくとも 1 つのエージェント CLI: `claude` / `codex` / `grok`

## エージェントセッションで起動する

```bash
# 初回のみ: シェルの rc に eval "$(hive shell-init zsh)" を追加
# tmux 内で hive のランチャーからエージェントを起動
$ hclaude      # もしくは: hcodex / hgrok

# エージェントセッションで以下を入力:
/hive:hive
```

エージェントは現在の pane をそのチームの orch に据え、タスクに応じてメンバーを spawn します。これ以降はあなたがエージェントと話し、エージェントがチームを回します。

## `hive fork` をキーにバインドする

ターミナルのキーバインドはシェルコマンドを直接実行できないため、バインドは生の
エスケープバイトを送り、それを tmux が受け取る。macOS + Ghostty なら
Cmd+Shift+F が ESC f を送り、tmux が fork を実行する:

```
# ~/.config/ghostty/config
keybind = cmd+shift+f=text:\x1bf

# ~/.tmux.conf
bind -n M-f run-shell -b 'hive fork --pane "#{pane_id}"'
```

`-b` は必須で、付けないと fork の実行中 tmux サーバーがブロックする。`--pane`
も同様 — バインドは pane の外から発火するため、自動検出では誤った pane を拾う。

## トランスクリプトミラーが読み取り専用である理由

対話的な Claude セッションにアタッチできる pty はありません — `claude attach` は job 専用です — が、トランスクリプトはターンの進行に合わせて 1 イベントずつ追記されるため、そのファイルを忠実に描くものがそのままライブミラーになり、構造上こちらから打ち返すことはできません。`hive view` がそれです。

claude メンバーの sessionId に対応する bg job レコードがない場合、それは対話的なセッション (デスクトップの `ccd`、join で取り込まれたセッション) であり、`hive attach` はその pane にこのミラーを自動で割り当てます。ここで resume するとフォークされた job が生まれ、そのメンバー宛ての配信を奪ってしまうからです。代償ははっきりしています: その pane は読み取り専用です。配信自体は影響を受けません — job レコードが無いという同じ判定により、`hive send` は pane ではなく稼働中の対話セッションへ配送されます — が、そのメンバーに打ち込めるのはセッションを保持しているアプリだけです。

## アップグレード

```bash
git pull && cargo install --path crates/hive
```

プラグインの manifest バージョンは CLI のバージョンに固定されているので、リリース 1 回でプラグイン更新も一緒に出ます。Claude Code 側は、bootstrap フックが `extraKnownMarketplaces` のエントリを書き込んだ時点から marketplace が自動更新されます。ただし `FORCE_AUTOUPDATE_PLUGINS` なしで `DISABLE_AUTOUPDATER` が設定されているとこの書き込みはスキップされ、その場合は `claude plugin update hive@hive` を手動で叩く必要があります。Codex は add した時点の marketplace スナップショットを保持し、自分では更新しません — `codex plugin marketplace upgrade hive` を実行してください。

## 開発

グローバルに入っている `hive` バイナリは、稼働中のエージェント伝送路そのものです。Hive 自体を開発している間はコミット済みのチェックアウトに留め、チームが使用中の汚れた worktree から `cargo install` しないこと。プラグインの実体化や hived の挙動を伴う手動検証には、使い捨ての `HIVE_HOME` / `CLAUDE_HOME` / `CODEX_HOME` と一時的な team を使い、稼働中チームの hived には触れません。テストレーンとリポジトリ規約は [AGENTS.md](AGENTS.md) を参照。

## ドキュメント

- [`docs/runtime-model.md`](docs/runtime-model.md) — レジストリと表示層のアイデンティティ、CLI ごとのネイティブランタイム源、`busy` / `inputState` / `turnPhase`
- [`docs/daemon-control-socket.md`](docs/daemon-control-socket.md) — Claude supervisor daemon の制御プロトコル。その `op:"reply"` が hive の配信本線
- [`plugins/hive/skills/hive/SKILL.md`](plugins/hive/skills/hive/SKILL.md) — `/hive:hive` がエージェントに読み込ませる協調プロトコル

## ライセンス

[GPL-3.0-or-later](LICENSE) © 2026 notdp
