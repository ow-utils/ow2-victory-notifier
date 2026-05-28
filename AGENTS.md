# Repository Guidelines

## 原則

- 応答やドキュメント、コミットメッセージ、コメントは日本語を使用する
- 本プロジェクトは ow2-victory-counter の SSE /events を購読し、Nightbot 経由で Twitch / YouTube ライブチャットに投稿する独立ツール
- 投稿は Nightbot HTTP API のみ。Twitch IRC・YouTube Data API は使わない

## ディレクトリー

- `src/` Rust ソース
- `config.example.toml` 設定テンプレート (実体 config.toml は git 管理外)
- `credentials-{account}.toml` 認証情報は OS の config dir 配下に保存 (リポジトリ外)。account ごとに別ファイル

## コミット

- `type: summary` 形式 (例: `feat: add nightbot oauth authenticate`)
- 1 コミット 1 責務
