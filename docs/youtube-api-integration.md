# ow2-victory-notifier — YouTube Data API v3 連携設計書

本ドキュメントは YouTube API Services Audit and Quota Extension 申請の添付資料として作成された、`ow2-victory-notifier` (以下「本ツール」) の YouTube Data API v3 連携に関する設計・実装ドキュメントです。

- **リポジトリ**: <GitHub URL>
- **ライセンス**: AGPL-3.0-or-later
- **作成日**: 2026-05-23
- **対象 API**: YouTube Data API v3

---

## 1. プロジェクト概要

本ツールは、Overwatch 2 の勝敗カウンター (`ow2-victory-counter`) が配信する Server-Sent Events (`/events`) を購読し、検出された試合結果 (勝ち / 負け / 引き分け) を、配信者本人の Twitch チャット および YouTube Live チャットへ自動投稿する Rust 製コマンドラインツールです。

- **対象ユーザー**: 本ツール開発者=配信者本人 **1 名のみ**
- **配布形態**: GitHub 上の OSS としてソースコード公開。SaaS 提供・バイナリ配布・ホスティング型サービスの提供は行いません
- **動作環境**: 配信者本人の PC (Linux / macOS / Windows) 上のローカルプロセスとして常駐
- **収益化**: なし (個人の趣味用途)

## 2. システム構成

```
┌──────────────────────┐    SSE     ┌─────────────────────────┐
│ ow2-victory-counter  │ ─────────▶ │ ow2-victory-notifier    │
│ (画面認識で勝敗検出) │ /events    │ (本ツール / 常駐 CLI)   │
└──────────────────────┘            └─────────────┬───────────┘
                                                  │
                                  ┌───────────────┴───────────────┐
                                  │                               │
                                  ▼                               ▼
                       ┌────────────────────┐         ┌──────────────────────┐
                       │ Twitch IRC         │         │ YouTube Data API v3  │
                       │ (chat:edit/read)   │         │ (liveChat messages)  │
                       └────────────────────┘         └──────────────────────┘
```

両連携は完全に独立しており、YouTube 連携は `config.toml` の `youtube_enabled` フラグで個別に有効/無効化できます。

## 3. 認証フロー (OAuth 2.0)

### 3.1 認可方式

- **使用するフロー**: OAuth 2.0 Device Authorization Grant (Device Code Flow)
- **`grant_type`**: `urn:ietf:params:oauth:grant-type:device_code`
- **OAuth クライアントタイプ**: Google Cloud Console 上で **「TVs and Limited Input devices」** として作成
- **要求スコープ**: `https://www.googleapis.com/auth/youtube` のみ
- **認可エンドポイント**: `https://oauth2.googleapis.com/device/code`
- **トークンエンドポイント**: `https://oauth2.googleapis.com/token`

### 3.2 認証フロー詳細

```
[1] ユーザーが `ow2-victory-notifier auth youtube --client-id ... --client-secret ...` を実行
       │
       ▼
[2] 本ツール → Google: POST /device/code
       (client_id, scope=https://www.googleapis.com/auth/youtube)
       │
       ▼
[3] Google → 本ツール: { device_code, user_code, verification_url, ... }
       │
       ▼
[4] 本ツールがターミナルに verification_url と user_code を表示
       │
       ▼
[5] ユーザーがブラウザで verification_url を開き、自分の Google アカウントで
    user_code を入力し、scope への同意を行う
       │
       ▼
[6] 本ツールが POST /token を polling (グラント完了まで)
       │
       ▼
[7] Google → 本ツール: { access_token, refresh_token, expires_in }
       │
       ▼
[8] 本ツールがトークンをローカルファイル (0600) に保存
```

該当実装: `src/youtube.rs::authenticate` (lines 88-188)

### 3.3 トークンのリフレッシュ

`access_token` の有効期限切れ時、`POST https://oauth2.googleapis.com/token` を `grant_type=refresh_token` で叩いて再発行します。実装: `src/youtube.rs::refresh` (lines 191-219)

## 4. 使用する YouTube Data API v3 エンドポイント

本ツールが呼び出すエンドポイントは **2 つのみ** です。

### 4.1 `liveBroadcasts.list`

| 項目             | 値                                                                       |
| ---------------- | ------------------------------------------------------------------------ |
| HTTP             | `GET https://www.googleapis.com/youtube/v3/liveBroadcasts`               |
| クエリパラメータ | `part=snippet`, `broadcastStatus=active`, `mine=true`                    |
| 認可             | `Authorization: Bearer <access_token>`                                   |
| 用途             | 認証済みアカウント本人の現在アクティブな配信に紐づく `liveChatId` を取得 |
| 頻度             | **配信開始時に 1 回のみ** (キャッシュし、配信終了まで再取得しない)       |
| ユニット消費     | 1 units / 回                                                             |
| 実装箇所         | `src/youtube.rs::get_active_live_chat_id` (lines 222-261)                |

`mine=true` を指定しているため、**認証アカウント本人の配信** のみが対象です。他のチャンネルの情報は取得しません。

### 4.2 `liveChatMessages.insert`

| 項目             | 値                                                                                                                   |
| ---------------- | -------------------------------------------------------------------------------------------------------------------- |
| HTTP             | `POST https://www.googleapis.com/youtube/v3/liveChat/messages`                                                       |
| クエリパラメータ | `part=snippet`                                                                                                       |
| 認可             | `Authorization: Bearer <access_token>`                                                                               |
| リクエストボディ | `{ "snippet": { "liveChatId": "...", "type": "textMessageEvent", "textMessageDetails": { "messageText": "..." } } }` |
| 用途             | 認証済みアカウント本人の配信ライブチャットへ、Overwatch 2 の試合結果を 1 件投稿                                      |
| 頻度             | **試合終了 1 件につき 1 回**。配信中で平均 5〜20 回程度                                                              |
| ユニット消費     | 50 units / 回                                                                                                        |
| 実装箇所         | `src/youtube.rs::send_message` (lines 263-310)                                                                       |

## 5. 投稿されるメッセージ内容

投稿されるテキストは Overwatch 2 の勝敗結果のみで、`config.toml` のテンプレートに基づいて生成されます。例:

```
WIN 3-2
LOSE 1-3
DRAW 2-2
勝利！ 通算 5勝 2敗
```

- 文字数は通常 30 文字以内
- URL・画像・広告・宣伝・自動応答・スパム的な内容は **一切含みません**
- 視聴者宛のメンション (`@username`) は使用しません
- 投稿頻度は試合終了時のみで、1 試合の所要時間 (10〜20 分) に対して 1 件以下のため、Live Chat の per-minute レート制限を超過することはありません

## 6. 取り扱うデータと保存

### 6.1 API から取得するデータ

| データ                | 用途                                     | 保存                                               |
| --------------------- | ---------------------------------------- | -------------------------------------------------- |
| `liveChatId` (文字列) | `liveChatMessages.insert` 呼び出しに使用 | **メモリ上のキャッシュのみ**。プロセス終了時に破棄 |

`liveBroadcasts.list` のレスポンスに含まれる `snippet` の他フィールド (タイトル、説明、スケジュール時刻等) は読み捨て、保存しません。

### 6.2 認証データの保存

| データ                                        | 保存場所                                                                                                                                                                                                      | 形式 | パーミッション                          |
| --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---- | --------------------------------------- |
| `access_token`, `refresh_token`, `expires_at` | `~/.config/ow2-victory-notifier/credentials.toml` (Linux) <br> `~/Library/Application Support/ow2-victory-notifier/credentials.toml` (macOS) <br> `%APPDATA%\ow2-victory-notifier\credentials.toml` (Windows) | TOML | Unix 系は `0600` (所有者のみ読み書き可) |

実装箇所: `src/credentials.rs`

### 6.3 取得・保存しないもの

本ツールは以下を **取得しません**:

- 視聴者のチャットコメント、視聴者名、視聴者数等の視聴者データ
- 自分以外のチャンネル / 配信の情報
- 動画メタデータ、再生回数、評価、コメント
- アナリティクスデータ
- 個人情報 (氏名、メール、誕生日等)

本ツールは以下を **送信しません**:

- ローカルに保存したトークン / 認証情報を外部のサーバー、第三者、解析サービスに送信することはありません
- テレメトリ・クラッシュレポート等の自動送信機能はありません

## 7. クォータ使用量の見積もり

### 7.1 典型的な配信 1 回あたりの消費

| 操作                                                   | 回数           | 単価     | 合計             |
| ------------------------------------------------------ | -------------- | -------- | ---------------- |
| `liveBroadcasts.list` (配信開始時の `liveChatId` 取得) | 1              | 1 unit   | 1 unit           |
| `liveChatMessages.insert` (試合終了ごと)               | 10 回 (典型値) | 50 units | 500 units        |
| **配信 1 回あたり合計**                                |                |          | **約 501 units** |

### 7.2 1 日あたりの消費

| シナリオ | 配信回数 / 日 | 試合数 / 配信 | 1 日合計       |
| -------- | ------------- | ------------- | -------------- |
| 通常     | 1             | 10            | 約 501 units   |
| 多め     | 2             | 20            | 約 2,002 units |
| 想定上限 | 3             | 30            | 約 4,503 units |

**いずれもデフォルト割り当ての 10,000 units/日の範囲に十分収まります。**

## 8. エラーハンドリング

- HTTP 4xx (4xx 系) を受信した場合は再試行せず、ログに記録してそのイベントの投稿をスキップします
- `liveChatId` が無効化されたと判断できる場合 (配信終了等) はキャッシュをクリアし、次の試合終了時に再取得を試みます
- `429` / `quotaExceeded` を受信した場合は即座にエラーを返し、ユーザーに対処を促します

実装箇所: `src/notifier.rs` (エラーハンドリングおよびリトライポリシー)

## 9. YouTube API Services Terms of Service への準拠

- API から取得したデータをサードパーティに転送・販売しません
- ユーザーデータをローカルで暗号化せずに長期保存することはなく、保存対象は認証トークンのみ (0600 で保護)
- ボット / 自動化された操作は、認証アカウント本人が自身の配信に投稿する目的に限定しており、視聴者を装ったコメントや大量投稿は行いません
- 本ツールの利用が YouTube API Services Terms of Service に違反する事象を発見した場合、開発者本人が責任をもって速やかに修正します

## 10. ソースコード参照

| 機能                          | ファイル             |
| ----------------------------- | -------------------- |
| OAuth 認証 (Device Code Flow) | `src/youtube.rs`     |
| トークン保存 / 読み込み       | `src/credentials.rs` |
| イベント購読・投稿ループ      | `src/notifier.rs`    |
| 設定ファイル定義              | `src/config.rs`      |
| エントリポイント / CLI 引数   | `src/main.rs`        |

全コードは <GitHub URL> で公開されており、本ドキュメントの記載内容と実装の整合性を確認可能です。

---

## 付録 A: スクリーンショット (添付予定)

- 認証コマンド実行時のターミナル出力 (`user_code` と `verification_url` の表示)
- ブラウザ上での Google OAuth 同意画面 (要求スコープが `youtube` のみであること)
- 実際の YouTube Live チャットへの投稿結果 (`WIN 3-2` 等の短いテキスト)
- `config.toml` のサンプル (機微情報をマスク済み)

## 付録 B: 連絡先

- 開発者: (氏名)
- メール: (連絡先メールアドレス)
- GitHub: <GitHub URL>
