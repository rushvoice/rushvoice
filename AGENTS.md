# AGENTS.md — RushVoice

読み手は、このリポジトリを触る AI エージェント（Claude Code・Codex など）と人の開発者です。

## これは何か
話した言葉を文字にしてカーソル位置へ貼り付ける Windows / macOS アプリ。Tauri 2（Rust・`src-tauri/`）＋ React / TypeScript（`frontend/`）。
文字起こしは Groq Whisper（利用者の API キー）か、RushVoice アカウントでログインしたときは有料版のゲートウェイ経由。ゲートウェイはこのリポジトリに含まれない。

## 構成
| 役目 | 場所 |
|---|---|
| 録音・文字起こし・貼り付け | `src-tauri/src/recorder.rs`・`transcribe.rs`・`clipboard/` |
| 幻聴（ハルシネーション）対策 | `halu.rs`・`prompt_sanitize.rs` |
| 文章の整え（LLM 後処理） | `llm_postprocess.rs`・`profile_db.rs` |
| ログイン・有料機能の判定 | `gateway_auth.rs` |
| 議事録・字幕 | `meeting*.rs`・`subtitle*.rs` |
| OS ごとの処理 | `*/windows_impl.rs`・`*/macos_impl.rs` |
| 保存先 | `paths.rs`（Windows `%APPDATA%\RushVoice`・macOS `~/Library/Application Support/RushVoice`） |

## 作業の決まり
- ビルド・テストは `CLAUDE.md`。変更は PR で出し、squash でマージする。
- API キー・トークン・ゲートウェイの接続先 URL をコードに書かない（接続先はビルド時に `RUSHVOICE_GATEWAY_BASE_URL` か Git 管理外の `src-tauri/gateway_url.txt` から入る）。
- 実在の人名・会社名・社内の URL・開発者の PC のパスを、コード・コメント・テストデータに入れない。
- ログイン・課金の判定に関わる変更は、メンテナーの確認を受ける。
