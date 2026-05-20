# Repository Guidelines

## 原則

- 応答やドキュメント、コミットメッセージ、コメントは日本語を使用する
- 本プロジェクトは ow2-victory-counter の SSE /events を購読し Twitch/YouTube に投稿する独立ツール

## ディレクトリー

- `src/` Rustソース
- `config.example.toml` 設定テンプレート (実体 config.toml はgit管理外)
- `credentials.toml` 認証情報は OS の config dir 配下に保存 (リポジトリ外)

## コミット

- `type: summary` 形式 (例: `feat: add twitch device code flow`)
- 1コミット1責務
