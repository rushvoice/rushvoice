<img src="src-tauri/icons/128x128.png" width="96" alt="RushVoice のアイコン">

# RushVoice

話した言葉を文字にして、いまカーソルがある場所へそのまま貼り付ける音声入力アプリです（Windows・macOS）。
ホットキーを押して話し、離すとメール・チャット・エディタなど、どのアプリにも文字が入ります。

*RushVoice is a push-to-talk voice input app for Windows and macOS. Speak, release the hotkey, and the transcribed text is pasted wherever your cursor is. English summary is below.*

## できること

- **音声入力**: ホットキーで録音 → 文字起こし → カーソル位置へ貼り付け
- **辞書**: 固有名詞や誤変換を登録して認識を補正
- **プロファイル**: アプリごとに文体（素のまま・ナチュラル・ていねい・カジュアル）を切り替え
- **履歴**: 過去の文字起こしを検索・コピー
- **有料版（ログイン）**: AI による文章の整え・議事録・セミナー字幕

## 使い方は2通り

| | 自分の API キー | RushVoice アカウント（有料） |
|---|---|---|
| 必要なもの | [Groq](https://console.groq.com/) の API キー（Groq の無料枠あり） | メールアドレス（確認コードでログイン） |
| 文字起こし・文章の整え・議事録・字幕 | 使える（料金は自分の Groq アカウントにかかる） | 使える（プランに含まれる） |
| 設定 | 設定画面にキーを入れる | 設定画面からログイン |

有料版のサーバー（ゲートウェイ）はこのリポジトリには含まれません。

**送られるデータ**: 録音した音声と文字起こしの文は、文字起こし・整えのために Groq（自分のキーのとき。設定によって OpenAI も）か、RushVoice のサーバー（ログインのとき）へ送られます。履歴・辞書・設定は端末の中に保存されます（ただし辞書の登録語の一部は、認識と整えの精度を上げるため音声と一緒に送られます）。

## インストール

[Releases](../../releases) から最新版をダウンロードしてください。詳しい手順（AI エージェントに読ませて導入してもらう手順を含む）は [INSTALL.md](INSTALL.md) にあります。

ダウンロードは、このリポジトリの Releases からだけにしてください（ほかのサイトの配布物は公式ではありません）。

- **Windows**: インストーラー（`.exe` / `.msi`）。コード署名をしていないため、初回に SmartScreen の警告が出ます。Releases から入手したファイルであることを確かめてから「詳細情報」→「実行」を選んでください。
- **macOS**: `.dmg`（Apple Silicon・Intel 両対応）。Apple の公証を受けていないため、初回は警告が出ます。アプリを「アプリケーション」に入れたあと、Finder で右クリック →「開く」を選ぶか、「システム設定」→「プライバシーとセキュリティ」から「このまま開く」を選んでください。

## ソースからビルド

前提: Rust（rustup）・Node.js 20 以上・pnpm 9。Windows は MSVC ビルドツール、macOS は Xcode Command Line Tools。

```bash
cargo install tauri-cli --version "^2" --locked
cd frontend && pnpm install
cd ../src-tauri
cargo tauri dev     # 開発用に起動
cargo tauri build   # 配布用にビルド
```

自分の Groq キーは、アプリの設定画面に入れると端末内で暗号化して保存されます。開発中は `.env.example` を `.env` にコピーして `GROQ_API_KEY` を入れることもできます（`.env` は平文なので共有しないでください）。

構成: Tauri 2（Rust）＋ React / TypeScript。OS ごとの処理は `src-tauri/src/*/windows_impl.rs`・`macos_impl.rs` に分かれています。

## 貢献・不具合の報告

- 不具合・要望: [Issues](../../issues)
- コードの貢献: [CONTRIBUTING.md](CONTRIBUTING.md)
- 脆弱性の報告: [SECURITY.md](SECURITY.md)（Issue には書かないでください）

## ライセンス

コードは [Apache License 2.0](LICENSE) です。Apache License は商標の使用を許諾しません。「RushVoice」の名前の扱いは [TRADEMARKS.md](TRADEMARKS.md) の商標の方針に従ってください。

---

## English

RushVoice is a push-to-talk voice input app built with Tauri 2 (Rust) and React.

- Hold the hotkey, speak, release: the text is pasted at your cursor in any app.
- Use all features (transcription, AI rewriting, meeting minutes, live subtitles) with your own [Groq](https://console.groq.com/) API key, billed to your Groq account (Groq has a free tier), or sign in to a paid RushVoice account instead. The paid gateway is not part of this repository.
- Audio and transcripts are sent to Groq (or OpenAI, if configured) with your own key, or to the RushVoice server when signed in. History, dictionary and settings are stored on your device; some dictionary terms are sent along with the audio to improve recognition.
- Build: `cargo install tauri-cli --version "^2" --locked`, `cd frontend && pnpm install`, then `cd ../src-tauri && cargo tauri dev`.
- Install: see [INSTALL.md](INSTALL.md) (includes step-by-step instructions for AI agents).
- License: Apache-2.0, which does not grant trademark rights. See TRADEMARKS.md for how to use the "RushVoice" name.
