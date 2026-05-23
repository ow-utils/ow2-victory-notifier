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

本ツールは Google OAuth2 **Device Code Flow** (`urn:ietf:params:oauth:grant-type:device_code`) でトークンを取得し、スコープ `https://www.googleapis.com/auth/youtube` を要求します。そのため OAuth クライアントの種類は **TVs and Limited Input devices** である必要があります (Web/Desktop クライアントでは device flow は通りません)。

[Google Cloud Console](https://console.cloud.google.com/) で以下の手順を実施してください。

#### 2-1. プロジェクトの作成 / 選択

画面上部のプロジェクトセレクタから既存プロジェクトを選択するか、「新しいプロジェクト」で作成します。以降の操作はすべて同じプロジェクト上で行います。

#### 2-2. YouTube Data API v3 の有効化

「APIとサービス」→「ライブラリ」で **YouTube Data API v3** を検索し、「有効にする」をクリックします。

有効化後、「APIとサービス → 割り当てとシステム上限」で `YouTube Data API v3` の `Queries per day` を確認してください。

> 🚨 **新規プロジェクトは初期クォータが 0 のことがあります**: 2024 年以降、Google は YouTube Data API v3 のクォータ運用を厳格化しており、**新規 GCP プロジェクトでは `Queries per day` が 0 で始まり、利用前に申請が必須**になるケースが一般化しています。0 のままだと初回 API 呼び出しが即 `quotaExceeded` で失敗します。
>
> その場合は [YouTube API Services - Audit and Quota Extension Form](https://support.google.com/youtube/contact/yt_api_form) から割り当て申請を提出してください。個人配信用途でも申請は通ります。フォームには使用エンドポイント (`liveChatMessages.insert`, `liveBroadcasts.list`) と想定リクエスト数を素直に記入します。審査は通常 数日〜2 週間。
>
> 申請が降りるまでの間は `config.toml` で `youtube_enabled = false` にして Twitch 側だけ動かす運用が現実的です。

> ⚠️ **クォータ上限に注意**: 割り当てが付与された場合、デフォルトのクォータは **10,000 units / プロジェクト / 日** で、太平洋時間 (PT) の 0:00 (日本時間 16:00 または 17:00、DST に依存) にリセットされます。
>
> 本ツールが呼ぶエンドポイントの消費量:
> - `liveChatMessages.insert` (勝敗投稿): **50 units / 回**
> - `liveBroadcasts.list` (liveChatId 取得): 1 unit / 回
>
> 実上限は **約 200 投稿/日** です。超過すると `quotaExceeded` エラーで投稿が失敗します。次のケースで早期に枯渇しやすいので注意してください:
> - 同じ GCP プロジェクトを他用途 (特に `search.list` は 100 units/回) と共有している
> - liveChatId が無効化された後に再取得が連続失敗し、毎イベントで `liveBroadcasts.list` が走るループに陥っている (notifier のログで `liveChatId 取得失敗` が連続していないか確認)
> - 動作検証で短時間に大量に投稿を発火させた
>
> 継続的に超えるようなら、本ツール専用に GCP プロジェクトを分離するか、Cloud Console の「APIとサービス → 割り当てとシステム上限」から増加申請してください (ただし `youtube` スコープのため Google の審査が必要)。

#### 2-3. OAuth 同意画面の構成

「APIとサービス」→「OAuth 同意画面」(Google Auth Platform の「ブランディング」「対象ユーザー」「データアクセス」) を構成します。

- **User Type**: **External** (個人 Google アカウントで使う場合)
- **アプリ名 / サポートメール / デベロッパー連絡先**: 任意の値で可
- **スコープ**: 「スコープを追加または削除」から `.../auth/youtube` (`See, edit, and permanently delete your YouTube videos, ratings, comments and captions`) を追加します。これは **機密 (sensitive) スコープ** です
- **テストユーザー**: 配信に使う Google アカウントのメールアドレスを追加します (公開ステータスが「テスト中」の間、ここに登録されていないアカウントは認証できません)
- **公開ステータス**: 個人利用の範囲では **「テスト中」のまま** で問題ありません

> ⚠️ **「テスト中」アプリで発行された refresh token は 7 日で失効します** ([Google OAuth 2.0 仕様](https://developers.google.com/identity/protocols/oauth2#expiration))。失効すると本ツールの自動更新も失敗するため、その都度 `cargo run -- auth youtube ...` で再認証してください。継続的に使う場合はアプリを「本番環境」に公開する必要がありますが、`youtube` スコープは機密スコープのため Google の検証 (verification) が要求されます。

#### 2-4. OAuth クライアント ID の作成

「APIとサービス」→「認証情報」→「+認証情報を作成」→「OAuth クライアント ID」を選択。

- **アプリケーションの種類 (Application type)**: **TVs and Limited Input devices**
- **名前**: 任意 (例: `ow2-victory-notifier`)

作成後に表示される **クライアント ID** と **クライアントシークレット** を控えておきます (後から「認証情報」画面で再確認可能)。これらを次のステップ 5 (`cargo run -- auth youtube --client-id ... --client-secret ...`) に渡します。

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

## チャットでの発言者名について

本ツールは認証に使用したアカウント自身としてチャットに投稿します。Bot 用の別名表示はできません。

- **Twitch**: 認証した Twitch アカウントの login_name で発言されます (IRC `NICK` に Helix `/users` で取得した login_name を渡しているため)。
- **YouTube**: OAuth トークンを発行した Google アカウントの YouTube チャンネル名で発言されます (`liveChatMessages.insert` では `authorChannelId` を指定せず、トークン発行元のチャンネルが自動的に投稿者になります)。

配信中の本アカウントで認証すると、視聴者のチャット欄に配信者本人の名前で勝敗通知が流れることになります。Bot らしく見せたい場合は、Twitch / YouTube それぞれで Bot 用のアカウント (および YouTube チャンネル) を別途用意し、そのアカウントで認証を行ってください。配信チャンネル側でモデレーター権限を与えるなどの運用が一般的です。

## 認証情報の保存先

認証情報は OS の設定ディレクトリに保存されます。

- Linux: `$XDG_CONFIG_HOME/ow2-victory-notifier/credentials.toml` (未設定時は `~/.config/ow2-victory-notifier/credentials.toml`)
- macOS: `~/Library/Application Support/ow2-victory-notifier/credentials.toml`
- Windows: `%APPDATA%\ow2-victory-notifier\credentials.toml` (通常は `C:\Users\<ユーザー名>\AppData\Roaming\ow2-victory-notifier\credentials.toml`)

Unix 系ではファイルパーミッションを `0o600` に設定して所有者のみが読めるようにしますが、Windows ではこの設定は行われません。必要に応じてファイルの ACL を手動で制限してください。

`credentials.toml` はリポジトリ外で管理され、git には含まれません。

## ライセンス

GNU Affero General Public License v3.0 またはそれ以降 (AGPL-3.0-or-later)。詳細は [LICENSE](LICENSE) を参照してください。
