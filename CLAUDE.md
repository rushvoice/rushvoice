@AGENTS.md

# CLAUDE.md — RushVoice

## ビルド・起動
- フロントエンドは **pnpm**（`pnpm-lock.yaml`。npm は使わない）
```
cd frontend && pnpm install
cd ../src-tauri && cargo build
cargo install tauri-cli --version "^2" --locked
cargo tauri dev
```
- 前提: Rust（rustup）・Node 20 以上・pnpm 9。Windows は MSVC ビルドツール、macOS は Xcode Command Line Tools
- テスト: `cd src-tauri && cargo test`／フロントの検査: `cd frontend && pnpm lint`

## 環境変数
`.env.example` を `.env` にコピーして `GROQ_API_KEY` を入れる（`.env` は Git 管理外）。

## CI: Mac のビルド
- `main` への push で GitHub Actions が macOS universal（Apple Silicon＋Intel）の DMG をビルドする（`.github/workflows/build-macos.yml`）。
- 署名は ad-hoc（`APPLE_SIGNING_IDENTITY="-"`）。⚠️ `APPLE_CERTIFICATE` などの署名用 secrets / env を workflow に足さない（空文字でも `security import` が失敗する）。
- `*.md` と `.claude/` だけの変更ではビルドしない。

## 作業ルール
1. 始める前と push の前に `git pull`
2. 変更は `gh pr create` で PR にする
3. 機密を commit しない（`.env` は `.gitignore` 済み。キーをコードに書かない）
4. OS 依存の処理は `cfg` で `windows_impl.rs` / `macos_impl.rs` に分ける
