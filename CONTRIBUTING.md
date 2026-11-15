# 貢献のしかた / Contributing

RushVoice への貢献をありがとうございます。

## 始める前に
- 大きな変更は、先に Issue で相談してください。
- 不具合の報告には OS・アプリの版・再現手順・ログ（ログ内の個人情報は消してください）を書いてください。

## 開発の流れ
1. リポジトリを fork し、`main` からブランチを作る
2. 変更する（Rust は `src-tauri` で `cargo fmt` と `cargo clippy`、フロントは `frontend` で `pnpm lint`）
3. テスト: `src-tauri` で `cargo test`
4. Pull Request を出す。何を・なぜ変えたか、どの OS で確かめたかを書いてください

## 決まりごと
- API キー・トークン・個人情報をコミットしない（`.env` は Git 管理外）
- 実在の人名・会社名・社内の URL をコードやテストデータに入れない
- 外部からの PR で GitHub Actions を動かすには、メンテナーの承認が要ります（リポジトリの設定で必須にしています）

## ライセンス
PR を送った時点で、その貢献は Apache License 2.0 の下で提供されたものとみなします（LICENSE 第5条）。

---

Thanks for contributing! Please open an issue before large changes, run `cargo fmt`, `cargo clippy`, `cargo test` and `pnpm lint`, and never commit secrets or personal data. Contributions are licensed under Apache-2.0.
