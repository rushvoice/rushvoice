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
- **文章の整え・議事録・セミナー字幕**: AI で話し言葉を整え、会議の記録や翻訳字幕にする

## 接続先（自分のキー・ローカル LLM）

アカウント登録やログインはありません。設定画面の「接続先と API キー」で、次のどれかを選びます。
**文字起こし**と**文章の整え・字幕翻訳・議事録整形**は、別々の接続先にできます（例: 文字起こしは手元の whisper.cpp、整えは Groq）。

| 接続先 | 必要なもの | 備考 |
|---|---|---|
| Groq | [Groq](https://console.groq.com/) の API キー | 無料枠あり。既定はこれ |
| OpenAI | [OpenAI](https://platform.openai.com/) の API キー | Groq と両方入れると、Groq が遅いときに OpenAI が並走する |
| カスタム（OpenAI 互換） | ベース URL・モデル名（キーは任意） | 手元のローカル LLM や、社内のサーバーなど |

キーは端末の中で暗号化して保存されます（Windows は DPAPI、macOS はキーチェーン）。

### カスタム接続先の書き方

ベース URL は OpenAI の SDK の `base_url` と同じく **`/v1` まで**を書きます。アプリが末尾に `/audio/transcriptions`・`/chat/completions` を付けて呼びます。`http://localhost` も使え、キーが空なら認証ヘッダーを送りません。設定画面の「接続テスト」は `{ベース URL}/models` に問い合わせます。

**文章の整え（chat/completions）の例**

| サーバー | ベース URL | モデル名の例 |
|---|---|---|
| [Ollama](https://ollama.com/) | `http://localhost:11434/v1` | `qwen2.5:7b`（`ollama list` に出る名前） |
| [LM Studio](https://lmstudio.ai/) | `http://localhost:1234/v1` | 読み込んだモデルの名前 |

```bash
ollama pull qwen2.5:7b   # 例。日本語の指示を守れる程度の大きさのモデルをおすすめします
ollama serve
```

**文字起こし（audio/transcriptions）の例**

| サーバー | ベース URL | モデル名 |
|---|---|---|
| [whisper.cpp](https://github.com/ggml-org/whisper.cpp) の `whisper-server` | `http://127.0.0.1:8080/v1` | 空でよい（起動時に読み込んだモデルを使う） |
| faster-whisper 系の OpenAI 互換サーバー（[speaches](https://github.com/speaches-ai/speaches) など） | `http://localhost:8000/v1` | サーバーの指定どおり（例: `Systran/faster-whisper-large-v3`） |

```bash
# whisper.cpp の例（/v1/audio/transcriptions で受けるよう指定する）
whisper-server -m models/ggml-large-v3-turbo.bin --host 127.0.0.1 --port 8080 --inference-path /v1/audio/transcriptions
```

注意:

- 通常の録音では、互換性のためカスタム接続先には `response_format=json` で頼みます。議事録・セミナー字幕は区間の時刻と言語が要るため `verbose_json` に対応したサーバーが必要です（whisper.cpp server・speaches は対応）。
- 整えの結果が元の文より大きく伸びた・文が消えた・依頼文が答えに変わったときは、どの接続先でも整える前の文に戻します。接続先につながらないときも、整えずにそのまま貼り付けます。
- 429（利用上限）は、Groq では短い待ちなら 1 回だけ再試行し、日次の上限に当たったら小型モデルへ切り替えます。ほかの接続先では整えずに貼り付けます。
- 整文のモデルは候補を複数持ち、前から順に使います。Groq は `openai/gpt-oss-120b` → `qwen/qwen3.8-27b` → `openai/gpt-oss-20b`、OpenAI は `gpt-4o-mini` → `gpt-4.1-mini` が既定です。接続先の `/models` に載っていない候補は飛ばし、送ったときに「モデルが無い・廃止された」と返されたら次の候補でやり直します。日次の上限に当たったときも次の候補へ進みます。
- 候補がすべて使えなかったときは、整える前の文を貼り、「整文のモデルの更新が必要です」と画面に出します。設定の「モデル名」に提供元の一覧にある名前をカンマ区切りで入れれば、アプリの更新を待たずに直せます。
- 候補の一覧は、このリポジトリの [`models.json`](models.json) からも読みます（起動時に 1 日 1 回まで。候補がすべて使えなかったときは 10 分空けて取り直し）。提供元がモデルを廃止したら `models.json` を直せば、アプリを更新しなくても次の起動から新しい候補で動きます。優先順は「設定の『モデル名』＞ `models.json` ＞ アプリに組み込んだ既定値」。`models.json` に書けるのはモデル名だけで、送り先やキーは変わりません。取れない・形が違うときは使わず、前に取れた一覧か既定値で動きます。
- `models.json` を取りに行かせたくないときは、設定の `model_catalog_auto_update` を `0` にします（ローカルだけで使う場合など。カスタム接続先には元から当てません）。

**送られるデータ**: 録音した音声と文字起こしの文は、選んだ接続先（Groq・OpenAI・カスタムで指定したサーバー）へ送られます。ローカルのサーバーを選べば、端末の外へは出ません。履歴・辞書・設定は端末の中に保存されます（辞書の登録語の一部は、認識と整えの精度を上げるため音声と一緒に接続先へ送られます）。モデル候補の一覧（`models.json`）を GitHub から読むときは、一覧を受け取るだけで、音声・文・キーは送りません。

### ローカルだけで使う（ネット無し・音声を外へ出さない）

文字起こしを whisper.cpp（または faster-whisper 系のサーバー）、整文を Ollama（または LM Studio）にすると、音声も文章も PC の外へ出ず、ネットが無くても音声入力できます。

1. whisper.cpp の `whisper-server` と Ollama を起動する（上の例のとおり）
2. 設定の「接続先と API キー」で、文字起こし・整文の両方を「カスタム」にし、ベース URL とモデル名を入れる（キーは空のまま）
3. 「接続テスト」で両方がつながることを確かめる

負担の目安（実測ではなく一般的な目安。モデルの大きさと GPU の有無で大きく変わります）:

| | 目安 |
|---|---|
| ディスク | 3.5〜7GB（文字起こしモデル 0.5〜1.6GB＋整文モデル 2〜5GB） |
| メモリ | 使っている間 6〜8GB。Ollama は既定で 5 分使わないとモデルを外す |
| GPU | 7B 級の整文モデルは VRAM 6GB でぎりぎり。GPU が無いと CPU で動くが数倍遅い |
| 速さ | GPU ありで文字起こし 1〜2 秒・整文 2〜4 秒ほど（クラウドの Groq はどちらも 1 秒未満） |
| 仕上がり | 手元で動く 3B〜8B 級は、クラウドの大きなモデルより整文の質が落ちる |

自分の環境に合わせて調整するときの手がかり:

- **速さを優先**: 文字起こしは `ggml-small`・`ggml-base`、整文は 3B 級（例: `qwen2.5:3b`）へ。整文を使わない（プロファイルを「そのまま」）のがいちばん速い
- **質を優先**: 文字起こしは `ggml-large-v3-turbo`、整文は 7B〜14B 級へ（VRAM 8GB 以上が目安）
- **片方だけローカル**: 音声だけ外へ出したくないなら文字起こしだけローカル、整文はクラウド、という組み合わせもできる
- 整文のガード（伸長・欠落・依頼の答え化を元の文へ戻す）と、つながらないときに元の文を貼る動きは、ローカルでも同じく効く

### 以前の有料版（RushVoice アカウント）を使っていた方へ

有料版は 2026 年 11 月に終了しました。更新後の初回起動で、保存されていたログイン情報は自動で消えます。設定の「接続先と API キー」で自分のキーか接続先を入れれば、すべての機能をこれまでどおり使えます。

## インストール

[Releases](../../releases) から最新版をダウンロードしてください。詳しい手順（AI エージェントに読ませて導入してもらう手順を含む）は [INSTALL.md](INSTALL.md) にあります。

ダウンロードは、このリポジトリの Releases からだけにしてください（ほかのサイトの配布物は公式ではありません）。

- **Windows**: インストーラー（`.exe` / `.msi`）。コード署名をしていないため、初回に SmartScreen の警告が出ます。Releases から入手したファイルであることを確かめてから「詳細情報」→「実行」を選んでください。
- **macOS**: `.dmg`（Apple Silicon・Intel 両対応）。Apple の公証を受けていないため、初回は警告が出ます。アプリを「アプリケーション」に入れたあと、Finder で右クリック →「開く」を選ぶか、「システム設定」→「プライバシーとセキュリティ」から「このまま開く」を選んでください。

## ソースからビルド

前提: Rust（rustup）・Node.js 20 以上・pnpm 9。Windows は MSVC ビルドツール、macOS は Xcode Command Line Tools。

```bash
cargo install tauri-cli --version "^2" --locked
cd frontend
pnpm install
cd ../src-tauri
cargo tauri dev     # 開発用に起動
cargo tauri build   # 配布用にビルド
```

API キーは、アプリの設定画面に入れると端末内で暗号化して保存されます。開発中は `.env.example` を `.env` にコピーして `GROQ_API_KEY` を入れることもできます（`.env` は平文なので共有しないでください）。

構成: Tauri 2（Rust）＋ React / TypeScript。OS ごとの処理は `src-tauri/src/*/windows_impl.rs`・`macos_impl.rs` に分かれています。

## 貢献・不具合の報告

- 不具合・要望: [Issues](../../issues)
- コードの貢献: [CONTRIBUTING.md](CONTRIBUTING.md)
- 脆弱性の報告: [SECURITY.md](SECURITY.md)（Issue には書かないでください）

## 質問・相談の場

- ユーザー同士で教え合う場: [LINE オープンチャット「RushVoice ユーザーの会」](https://line.me/ti/g2/UlauFOXYsGIiKB8XncZICRId5FoqnG3JIYzOYw)（使い方の質問・作り変えた設定の共有・要望）
- 1対1の相談・会社での導入の相談: LINE 公式アカウント `@935mjmle`
- お知らせ: X [@RushVoiceApp](https://x.com/RushVoiceApp)

どの場でも、API キー・パスワード・個人情報は書かないでください（画面の写真にも写らないように）。設定やプロフィールを共有するときは、書き出したファイルや画面からキー・接続先の URL・トークンを消してからにしてください。
オープンチャットでの運営の発言は「RushVoice 運営」の名前だけです。運営がキー・パスワード・お金を求めたり、個別のトークへ誘ったりすることはありません。そうした書き込みは無視して、運営へ知らせてください。
不具合は再現手順を添えて [Issues](../../issues) へ。

## ライセンス

コードは [Apache License 2.0](LICENSE) です。Apache License は商標の使用を許諾しません。「RushVoice」の名前の扱いは [TRADEMARKS.md](TRADEMARKS.md) の商標の方針に従ってください。

---

## English

RushVoice is a push-to-talk voice input app built with Tauri 2 (Rust) and React.

- Hold the hotkey, speak, release: the text is pasted at your cursor in any app.
- No account or sign-in. All features (transcription, AI rewriting, meeting minutes, live subtitles) run with your own [Groq](https://console.groq.com/) or OpenAI API key (Groq has a free tier), or with any OpenAI-compatible server, including local ones.
- Transcription and rewriting can use different endpoints. In Settings → "接続先と API キー", choose **Custom** and enter a base URL up to `/v1` (the app appends `/audio/transcriptions` or `/chat/completions`), a model name and an optional key. Examples: Ollama `http://localhost:11434/v1` (model e.g. `qwen2.5:7b`), LM Studio `http://localhost:1234/v1`, whisper.cpp server `http://127.0.0.1:8080/v1`, speaches / faster-whisper servers `http://localhost:8000/v1`. Meeting minutes and subtitles need a transcription server that supports `verbose_json`.
- The same safety checks apply to every endpoint: if the rewritten text grows too much, drops sentences or turns a request into an answer, the original transcript is pasted instead.
- Audio and transcripts are sent only to the endpoints you choose; with a local server nothing leaves your machine. History, dictionary and settings are stored on your device; some dictionary terms are sent along with the audio to improve recognition.
- Community (Japanese): LINE OpenChat "RushVoice ユーザーの会" https://line.me/ti/g2/UlauFOXYsGIiKB8XncZICRId5FoqnG3JIYzOYw , news on X [@RushVoiceApp](https://x.com/RushVoiceApp). Never post API keys or personal data there.
- The former paid RushVoice account was discontinued in November 2026; saved sign-in data is removed on first launch after updating.
- Build: `cargo install tauri-cli --version "^2" --locked`, `cd frontend && pnpm install`, then `cd ../src-tauri && cargo tauri dev`.
- Install: see [INSTALL.md](INSTALL.md) (includes step-by-step instructions for AI agents).
- License: Apache-2.0, which does not grant trademark rights. See TRADEMARKS.md for how to use the "RushVoice" name.
