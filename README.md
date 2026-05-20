# ow2-victory-notifier

OW2 勝敗カウンター (`ow2-victory-counter`) の SSE `/events` を購読し、Twitch / YouTube のライブチャットに勝敗結果を自動投稿するツールです。

## 前提

- `ow2-victory-counter` が起動しており、`http://127.0.0.1:3000/events` で SSE を配信していること

## セットアップ

### 1. Twitch アプリ登録

[Twitch Developer Console](https://dev.twitch.tv/console/apps) でアプリを登録します。

- **Client Type**: Public
- **OAuth Redirect URLs**: `http://localhost` (ダミーで可)
- 作成後、**Client ID** を控えておきます

### 2. Google OAuth クライアント作成

[Google Cloud Console](https://console.cloud.google.com/) で OAuth クライアントを作成します。

- **Type**: TVs and Limited Input devices
- **Client ID** と **Client Secret** を控えておきます
- **YouTube Data API v3** を有効化します

### 3. 設定ファイルの準備

```sh
cp config.example.toml config.toml
```

`config.toml` を編集して各項目を設定します。

### 4. Twitch 認証 (Device Code Flow)

```sh
cargo run -- auth twitch --client-id <TWITCH_CLIENT_ID>
```

表示された URL にアクセスしてコードを入力し、認証を完了してください。

### 5. YouTube 認証

```sh
cargo run -- auth youtube --client-id <GOOGLE_CLIENT_ID> --client-secret <GOOGLE_CLIENT_SECRET>
```

### 6. 常駐起動

```sh
cargo run -- run
```

## 認証情報の保存先

認証情報は OS の設定ディレクトリに保存されます。

- Linux: `$XDG_CONFIG_HOME/ow2-victory-notifier/credentials.toml` (未設定時は `~/.config/ow2-victory-notifier/credentials.toml`)
- macOS: `~/Library/Application Support/ow2-victory-notifier/credentials.toml`
- Windows: `%APPDATA%\ow2-victory-notifier\credentials.toml` (通常は `C:\Users\<ユーザー名>\AppData\Roaming\ow2-victory-notifier\credentials.toml`)

Unix 系ではファイルパーミッションを `0o600` に設定して所有者のみが読めるようにしますが、Windows ではこの設定は行われません。必要に応じてファイルの ACL を手動で制限してください。

`credentials.toml` はリポジトリ外で管理され、git には含まれません。

## ライセンス

GNU Affero General Public License v3.0 またはそれ以降 (AGPL-3.0-or-later)。詳細は [LICENSE](LICENSE) を参照してください。
