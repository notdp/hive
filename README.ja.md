# Hive

> CLI エージェント向けの tmux ファーストな協調ランタイム — `claude` / `codex` / `grok` の各メンバーはそれぞれ自前のエンジンで動き、エンジン固有のネイティブ配信経路で `<HIVE>` メッセージをやり取りし、単一のレジストリを真実の層として共有します。

[English](README.md) · [简体中文](README.zh-CN.md) · **日本語**

_この README は英語版が正となります。翻訳は canonical 版に対して遅れることがあります。_

## Hive とは

Hive はエージェント向けのランタイムであり、人間が手で叩く CLI ではありません。チームの実体はレジストリ上の名簿（`$HIVE_HOME/state/teams/` にチームごと 1 つの JSON）とメンバーごとのエンジンであり、tmux ウィンドウはその上に `hive attach` が描き出す任意の表示層にすぎません。タスクの割り当て、メッセージ送信、ランタイム状態の確認といった日々の作業はエージェントセッション内で行われ、コマンドを発行するのもエージェントです。

人間側の入口はプラグインスキル `/hive:hive [team]` です。引数なしなら状況に応じて作成または参加、チーム名を渡せばそのチームに参加し、存在しなければ作成します。

人間側に残るのは一部のコマンドだけです: プラグインのインストール、セッショントランスクリプトの閲覧 (`hive view`)、ポップアップエディタ (`hive cvim` / `hive vim`)、分割 fork、そしてローカル開発のセットアップ。

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

CLI はプラグインの `SessionStart` フックに任せず、先に自分で入れてください。このフック (`plugins/hive/scripts/bootstrap.py`) の仕事は 2 つ、CLI の収束と Claude 側 marketplace 自動更新の有効化です。ところが収束のほうは今も `pipx install git+https://github.com/notdp/hive` を叩きます (`bootstrap.py:114`)。これは Rust 移行前の経路で、リポジトリにはもう `pyproject.toml` がないためバイナリは生成されず、後半の処理にも到達しません。PATH 上に 0.10.1 以上の `hive` が既にある場合だけ `already meets minimum` を返して何もインストールせず先へ進みます。それが唯一収束する経路です。

必要な環境:

- `tmux` — `hive cvim` / `hive vim` のポップアップに 3.2 以上。`hive view` がテーマ判定に使う素の OSC 11 背景色クエリも、pane から応答が返るのは 3.2 以上です (`crates/hive/src/view_theme.rs:349`)
- ビルド用の Rust ツールチェイン
- `python3` — `hive flow run` はインタプリタを exec してスクリプトを走らせ、対になる `hive.flow` クライアントは埋め込み済みです (`crates/hive/src/cli/rest.rs:1094`)。notify のポップアップも python のヒアドキュメントです (`crates/hive/src/notify_ui.rs:242`)
- 少なくとも 1 つのエージェント CLI: `claude` / `codex` / `grok`

## エージェントセッションで起動する

```bash
# 初回のみ: シェルの rc に eval "$(hive shell-init zsh)" を追加
# tmux 内で hive のランチャーからエージェントを起動
$ hclaude      # もしくは: hcodex / hgrok

# エージェントセッションで以下を入力:
/hive:hive
```

スキルがロードされると、エージェントは `hive create` を実行して現在の pane をそのチームの orch に据え、タスクに応じて `hive spawn` でメンバーを増やします。これ以降はあなたがエージェントと話し、エージェントがチームを回します。

この一連の流れに tmux は必須ではありません。tmux の外で `hive create` すれば headless なチームが登録され、表示層がなければ `hive spawn` は pane を持たないエンジンだけのメンバーを立ち上げます — 受信も報告も kill も通常どおりです。ウィンドウは後から `hive attach <team>` でメンバー 1 人につき 1 pane として実体化できます。

## オペレータ向けコマンド

人間が実行することの多いコマンド:

```bash
# プラグイン
hive plugin enable notify --plain # hived のアイドル監視トグル (手動の `hive notify` はどちらでも使える)
hive plugin list --plain          # 人間向け表示 (既定の出力は JSON)

# 読み取り専用のトランスクリプトミラー
hive view <session-id>            # Claude セッションをライブ追従。キー入力はどこにも届かない

# ポップアップエディタ (tmux 3.2+)
hive cvim                         # 直前の assistant メッセージを vim で編集して送り返す
hive vim                          # 空のバッファで書いてエージェント pane に送る

# 現在のエージェントセッションを分割 pane にフォーク
hive fork                         # 分割方向を自動判定
hive vfork                        # 垂直分割
hive hfork                        # 水平分割
```

`hive view` が描画するのは `~/.claude/projects/*/<session-id>.jsonl` です (`crates/hive/src/transcript_view.rs:44`)。対話的な Claude セッションにアタッチできる pty はありません — `claude attach` は job 専用です — が、トランスクリプトはターンの進行に合わせて 1 イベントずつ追記されるため、そのファイルを忠実に描くものがそのままライブミラーになり、構造上こちらから打ち返すことはできません。tty 上では ratatui のページャとして動きます: `↑↓` でブロック選択、`←→` で折りたたみ、`Enter` で全画面表示、`Ctrl+o` で密度切り替え、`/` でコマンドパレット (`/theme`、`/view`、`/find`、`/quit`)、`q` で終了。パイプやリダイレクト経由ではプレーンな ANSI ストリームに退化します (`transcript_view.rs:1622`)。テーマは `HIVE_VIEW_THEME=light|dark|auto`、次に `view.theme` 設定、どちらもなければ自動判定で、判定できなければ light に落ちます (`view_theme.rs:281`)。

`hive attach` はこれを自動で選びます。claude メンバーの sessionId に対応する bg job レコードがない場合、それは対話的なセッション (デスクトップの `ccd`、join で取り込まれたセッション) です。ここで resume するとフォークされた job が生まれ、そのメンバー宛ての配信を奪ってしまうため、該当メンバーの pane には resume ではなく `hive view` が割り当てられます (`crates/hive/src/cli/rest.rs:1268`)。代償ははっきりしています: その pane は読み取り専用です。配信自体は影響を受けません — job レコードが無いという同じ判定により、`hive send` は pane ではなく稼働中の対話セッションへ配送されます (`crates/hive/src/agent.rs:759`) — が、そのメンバーに打ち込めるのはセッションを保持しているアプリだけです。

Claude Code / Codex 内から叩く場合はシェルエスケープで: `!hive cvim`、`!hive vfork`、`!hive fork` など。

`hive fork` をキーボードショートカットにバインドすると tmux と相性が良くなります。例 (macOS 上の Ghostty + tmux) — Cmd+Shift+F で現在の pane をフォークします。お使いのターミナルに合わせてキーは変更してください:

```
# ~/.config/ghostty/config
keybind = cmd+shift+f=text:\x1bf

# ~/.tmux.conf
bind -n M-f run-shell -b 'hive fork --pane "#{pane_id}"'
```

それ以外の `hive send` / `hive team` / `hive spawn` / `hive doctor <agent>` などは、エージェントが呼び出す前提で設計されています。自分で実行しても動きますが、ハッピーパスではなくデバッグ・応用パスです。

## アップグレード

CLI はコミット済みのチェックアウトを再ビルドして更新します:

```bash
git pull && cargo install --path crates/hive
```

プラグインの manifest バージョンは CLI のバージョンに固定されているので、リリース 1 回でプラグイン更新も一緒に出ます。Claude Code 側は、bootstrap フックが `extraKnownMarketplaces.hive` を `autoUpdate: true` で書き込んだ時点から marketplace が自動更新されます。ただし `FORCE_AUTOUPDATE_PLUGINS` なしで `DISABLE_AUTOUPDATER` が設定されているとこの書き込みはスキップされ、その場合は `claude plugin update hive@hive` を手動で叩く必要があります。Codex は add した時点の marketplace スナップショットを保持し、自分では更新しません — `codex plugin marketplace upgrade hive` を実行してください。

## コントリビュータ向け

```bash
cargo nextest run                 # Rust スイート全体
python -m pytest tests/e2e -q     # target/debug/hive に対する tmux ブラックボックス
```

nextest は好みではなく必須です。テストは環境変数を自由に書き換えるため、1 プロセスを共有する素の `cargo test` では互いに汚染し合います。

live 環境へインストールしたら毎回、インストール後の受け入れスイートを流します。CLI ごとに実メンバーを 1 つ起動し、ユニットテストからは見えないオラクル (返信の同一性、`capture-pane -e` で読む pane の色、nonce の因果、headless claude による意味論的な検死) を検証します:

```bash
HIVE_ACCEPTANCE=1 HIVE_ACCEPTANCE_CLIS=claude,codex,grok python -m pytest tests/acceptance -q
```

グローバルに入っている `hive` バイナリは、稼働中のエージェント伝送路そのものです。Hive 自体を開発している間はコミット済みのチェックアウトに留め、チームが使用中の汚れた worktree から `cargo install` しないこと。プラグインの実体化や hived の挙動を伴う手動検証には、使い捨ての `HIVE_HOME` / `CLAUDE_HOME` / `CODEX_HOME` と一時的な team/window を使い、稼働中チームの hived には触れません。リポジトリ規約は [AGENTS.md](AGENTS.md) を参照。

## ドキュメント

- [`docs/runtime-model.md`](docs/runtime-model.md) — レジストリと表示層のアイデンティティ、CLI ごとのネイティブランタイム源、`busy` / `inputState` / `turnPhase`
- [`docs/daemon-control-socket.md`](docs/daemon-control-socket.md) — Claude supervisor daemon の制御プロトコル。その `op:"reply"` が hive の配信本線
- [`plugins/hive/skills/hive/SKILL.md`](plugins/hive/skills/hive/SKILL.md) — `/hive:hive` がエージェントに読み込ませる協調プロトコル

## ライセンス

[GPL-3.0-or-later](LICENSE) © 2026 notdp
