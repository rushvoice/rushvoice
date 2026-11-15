# AGENTS.md — RushVoice

読み手は、このリポジトリを触る AI エージェント（Claude Code・Codex など）と人の開発者です。

## これは何か
話した言葉を文字にしてカーソル位置へ貼り付ける Windows / macOS アプリ。Tauri 2（Rust・`src-tauri/`）＋ React / TypeScript（`frontend/`）。
文字起こしと文章の整えは、利用者自身の API キー（Groq・OpenAI）か、利用者が指定した OpenAI 互換の接続先（ローカル LLM 可）へ直接送る。ログイン・課金の仕組みは無い（2026-11-15 に有料版を終了）。

## 構成
| 役目 | 場所 |
|---|---|
| 録音・文字起こし・貼り付け | `src-tauri/src/recorder.rs`・`transcribe.rs`・`clipboard/` |
| 幻聴（ハルシネーション）対策 | `halu.rs`・`prompt_sanitize.rs` |
| 文章の整え（LLM 後処理） | `llm_postprocess.rs`・`profile_db.rs` |
| 接続先（Groq・OpenAI・カスタム）の解決 | `endpoints.rs` |
| 議事録・字幕 | `meeting*.rs`・`subtitle*.rs` |
| OS ごとの処理 | `*/windows_impl.rs`・`*/macos_impl.rs` |
| 保存先 | `paths.rs`（Windows `%APPDATA%\RushVoice`・macOS `~/Library/Application Support/RushVoice`） |

## 作業の決まり
- ビルド・テストは `CLAUDE.md`。変更は PR で出し、squash でマージする。
- API キー・トークン・社内のサーバーの URL をコードに書かない（公開 API の既定 URL だけ `endpoints.rs` に置く）。
- 実在の人名・会社名・社内の URL・開発者の PC のパスを、コード・コメント・テストデータに入れない。
- 鍵の保存（`crypto/`）・送り先の決め方（`endpoints.rs`）に関わる変更は、メンテナーの確認を受ける。
