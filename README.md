# ow2-victory-notifier

OW2 勝敗カウンター (`ow2-victory-counter`) の SSE `/events` を購読し、**Nightbot 経由で Twitch / YouTube ライブチャットに勝敗結果を自動投稿**するツールです。

投稿は Nightbot の HTTP API (`POST /1/channel/send`) のみを使います。Twitch IRC や YouTube Data API は使いません。Google API クォータ申請・OAuth 検証は不要です。

## 前提

- `ow2-victory-counter` が起動しており、`http://127.0.0.1:3000/events` で SSE を配信していること
- 投稿したいプラットフォームごとに Nightbot アカウントを所有 (Twitch と YouTube 両方なら 2 アカウント)
  - Nightbot の 1 つの channel は 1 プラットフォーム (Twitch **または** YouTube) に固定されます。両方に同時送信したい場合は、それぞれの OAuth で Nightbot にサインアップして 2 アカウントを用意し、本ツールも 2 プロセス並列で起動します (後述)。

## セットアップ (アカウントごとに繰り返す)

ここでは Twitch 側を `--account twitch`、YouTube 側を `--account youtube` として 2 アカウント運用する例を示します。1 アカウントしか使わない場合は `--account` を省略すると `default` が使われます。

### 1. Nightbot にサインアップ

投稿したいプラットフォームの OAuth で [Nightbot](https://nightbot.tv/) にサインアップします (Twitch 用なら Twitch アカウントで、YouTube 用なら Google アカウントで)。

### 2. Nightbot を対象 channel に Join させる

[Nightbot ダッシュボード](https://nightbot.tv/) 右上の **Join** ボタンを押して Bot を自分のチャンネルに参加させます。Join していないと送信が視聴者に届きません。

> ⚠️ **YouTube の場合**: Nightbot は配信が **live + public** の状態でのみ自動 join します。`check` で `joined=false` が出たら、まず実際の配信状態 (live で公開設定か) を確認してください。

### 3. Nightbot OAuth アプリの作成

[Nightbot OAuth Applications](https://nightbot.tv/account/applications) で新規アプリを作成し、以下を設定します。

- **Redirect URI**: `http://127.0.0.1:8123/callback`
  - `config.toml` で `[nightbot] callback_port` を変更している場合はそのポートに合わせる
- **Scopes**: `channel` と `channel_send` の両方
  - 本ツールは送信用 (`channel_send`) に加えて、`check` で `GET /1/channel` (`channel` スコープ必須) を叩いて join 状態を確認します

作成後、画面に表示される **Client ID** と **Client Secret** を控えます。

### 4. 設定ファイルの準備

```sh
cp config.example.toml config.toml
```

`callback_port` を変更したい場合のみ `config.toml` の `[nightbot]` セクションを編集します (通常はデフォルトの 8123 で問題ありません)。

### 5. 認証

**推奨**: client_secret は環境変数経由で渡します (シェル履歴・`ps`・`/proc/{pid}/cmdline` に残らない)。

```sh
export NIGHTBOT_CLIENT_SECRET='<コピーした Client Secret>'
cargo run -- auth nightbot --account twitch \
    --client-id <コピーした Client ID> \
    --client-secret-env NIGHTBOT_CLIENT_SECRET
```

表示された URL をブラウザで開き、Nightbot の承認画面で `channel` と `channel_send` の 2 スコープを許可します。

`--client-secret <SECRET>` で直接渡すこともできますが、シェル履歴と `ps` に残るため非推奨です (試験用途のみ)。

### 6. 通知ループの起動

```sh
cargo run -- run --account twitch
```

## 両プラットフォーム同時運用

`--account twitch` と `--account youtube` を **別プロセス** で並列起動します:

```sh
cargo run -- run --account twitch &
cargo run -- run --account youtube &
```

> ⚠️ **同じ `--account` を 2 プロセスで同時起動しないこと**。両方が並行 refresh して片方が `invalid_grant` を踏み、credentials が壊れます。本ツールは fs2 advisory lock で同一 account の二重起動を検出し、2 つ目のプロセスは起動時に明示エラー終了します。

## チャットでの発言者名

両プラットフォームとも `Nightbot` 名義で投稿されます (Nightbot API 経由のため)。視聴者から見て自動通知と本人発言が区別しやすくなります。

## 認証情報の保存先

アカウントごとにファイルが分かれます。

- Linux: `$XDG_CONFIG_HOME/ow2-victory-notifier/credentials-{account}.toml` (未設定時は `~/.config/...`)
- macOS: `~/Library/Application Support/ow2-victory-notifier/credentials-{account}.toml`
- Windows: `%APPDATA%\ow2-victory-notifier\credentials-{account}.toml`

Unix ではファイルパーミッションを `0o600` に設定します。Windows では OS の ACL に依存します。書き込みは tempfile による atomic replace で行うため、プロセスクラッシュ / SIGKILL でファイル破損する可能性は低いですが、電源断レベルの耐性は best-effort です (unix では親ディレクトリ fsync まで実施)。

同じ account の二重起動を防ぐため `credentials-{account}.lock` という sidecar ファイルも作られます (advisory flock、プロセス終了で OS が自動解放)。

## Nightbot 依存についての注意

本ツールは投稿経路を Nightbot に完全に委ねています。次のいずれかが起きると投稿が止まります。

- Nightbot 側の障害 (API ダウン / レート制限)
- アカウント停止 / Join 解除
- Nightbot 側 OAuth アプリの設定変更 (Redirect URI の不一致など)

### refresh_token の失効と延命

Nightbot の **refresh_token は「最後の使用から 60 日」で失効** します (access_token は約 30 日)。本ツールは SSE イベント (= 試合終了通知) を受けたタイミングで access_token の残り時間が 60 秒以下のとき限り refresh するため、**60 日以上 SSE イベントが発生しないと refresh_token が更新されません**。

長期間配信が途絶える運用では、**60 日以内に 1 回以上** 次のいずれかを実行して refresh_token を rotate してください。

```sh
cargo run -- check --account <name> --force-refresh
```

`--force-refresh` を付けると期限判定を迂回して無条件で refresh + 保存します。フラグ無しの `check` は access_token が残っている間は refresh を発火しないため、延命にはなりません。

#### `run` 常駐中は `check --force-refresh` 不可

同一 account の二重起動を fs2 advisory lock で防いでいるため、`run --account <name>` が走っている間に別プロセスで `check --account <name> --force-refresh` を起動すると **lock 競合で即終了** します。延命手順は次のいずれかです:

- **(推奨)** `run` を一時停止 (`kill` / `systemctl stop`) → `check --account <name> --force-refresh` を実行 → `run` を再開
- 長期間配信予定が無いなら `run` を止めておき、60 日以内に 1 回 `check --account <name> --force-refresh` のみ実行

### 障害時の切り分け手順

1. `cargo run -- check --account <name>` で疎通確認 (SSE + Nightbot API + Join 状態)
2. [Nightbot ダッシュボード](https://nightbot.tv/) でアカウント・Join 状態を確認
3. 必要なら `auth nightbot --account <name> ...` で再認証

## 旧版 (Twitch IRC / YouTube Data API) からの移行

旧 Twitch IRC / YouTube Data API 経路は廃止のため、旧 `credentials.toml` は使えなくなりました。

1. 旧 `…/ow2-victory-notifier/credentials.toml` を手動削除
2. 旧 `config.toml` の `[platforms]` / `[twitch]` / `[youtube]` セクションを削除し、`config.example.toml` を参考に新フォーマットへ書き換え
3. 本 README のセットアップ手順を最初から実施して Nightbot で再認証

## ライセンス

GNU Affero General Public License v3.0 またはそれ以降 (AGPL-3.0-or-later)。詳細は [LICENSE](LICENSE) を参照してください。
