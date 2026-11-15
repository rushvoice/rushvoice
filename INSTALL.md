# RushVoice の導入手順 / Installation

この文書は、人がそのまま読んで進める手順と、AI エージェント（Claude Code・Codex・ChatGPT など）に読ませて代わりに進めてもらう手順の両方を兼ねています。
AI に頼むときは「`https://github.com/rushvoice/rushvoice/blob/main/INSTALL.md` を読んで、この PC に RushVoice を入れて」と伝えてください。

- 対応 OS: Windows 10 / 11（x64）、macOS 13 以降（Apple Silicon・Intel）
- 必要なもの: マイク、インターネット接続、次のどちらか
  - [Groq](https://console.groq.com/keys) の API キー（無料枠あり。料金は自分の Groq アカウントにかかる）
  - RushVoice アカウント（有料版。メールアドレスに届く確認コードでログイン）

---

## 1. 人が進める手順

### Windows

1. [Releases の最新版](https://github.com/rushvoice/rushvoice/releases/latest) から `RushVoice_<版>_x64-setup.exe` をダウンロードする。
2. ダブルクリックで開く。コード署名をしていないため「Windows によって PC が保護されました」と出たら、**このリポジトリの Releases から入手したファイルであることを確かめてから**「詳細情報」→「実行」を選ぶ。
3. インストールが終わると RushVoice が起動する。
4. 「設定」→「API キー」に Groq の API キーを貼り付けて保存する（RushVoice アカウントで使うときは、設定画面からログインする）。
5. メモ帳などを開き、ホットキー **Ctrl + Shift + 無変換** を押して話し、もう一度押すと、カーソル位置に文字が入る。ホットキーは「設定」で変えられる。

### macOS

1. [Releases の最新版](https://github.com/rushvoice/rushvoice/releases/latest) から `RushVoice_<版>_universal.dmg` をダウンロードする。
2. dmg を開き、RushVoice を「アプリケーション」フォルダへドラッグする。
3. Apple の公証を受けていないため、初回は「開発元を検証できません」と出る。次のどちらかで開く。
   - Finder で「アプリケーション」の RushVoice を右クリック →「開く」→「開く」
   - 開けないときは「システム設定」→「プライバシーとセキュリティ」を下へスクロールし、RushVoice の「このまま開く」を押す
4. 起動すると次の許可を求められるので、両方とも許可する。
   - **マイク**
   - **アクセシビリティ**（ホットキーと貼り付けに使う）。「システム設定」→「プライバシーとセキュリティ」→「アクセシビリティ」で RushVoice をオンにし、RushVoice を一度終了して開き直す
5. 「設定」でホットキーを決める（Mac のキーボードには「無変換」が無いため）。
6. 「設定」→「API キー」に Groq の API キーを貼り付けて保存する（RushVoice アカウントで使うときは、設定画面からログインする）。
7. メモなどを開き、ホットキーを押して話し、もう一度押すと、カーソル位置に文字が入る。

### うまく動かないとき

| 症状 | 確かめること |
|---|---|
| 文字が入らない | 設定の API キーが保存されているか。Mac はアクセシビリティの許可と、許可後の再起動 |
| 録音されない・無音になる | OS のマイクの許可。使うマイクを「設定」で選ぶ |
| ホットキーが反応しない | 他のアプリと同じキーになっていないか。「設定」で別のキーにする |

解決しないときは [Issue](https://github.com/rushvoice/rushvoice/issues/new/choose) へ。ログや画面に API キーや個人情報が写っていないか確かめてから貼ってください。

---

## 2. AI エージェントが進める手順 / Instructions for AI agents

You are installing RushVoice on the user's computer. Follow these steps in order. Ask the user before anything marked **[user]**.

**Rules**
- Download only from `https://github.com/rushvoice/rushvoice/releases`. Never from other sites.
- Verify the SHA-256 checksum against `SHA256SUMS.txt` from the same release before running anything.
- Never ask the user to paste their API key into the chat. The user enters it in the app's Settings screen themselves.
- Do not disable OS security features globally (no `spctl --master-disable`, no turning off SmartScreen or Defender).

### Step 1: Detect the OS
- Windows: `[System.Environment]::OSVersion` (PowerShell). Requires Windows 10/11 x64.
- macOS: `sw_vers -productVersion` and `uname -m`. Requires macOS 13+.

### Step 2: Download the latest release and verify it

Windows (PowerShell):
```powershell
$rel = Invoke-RestMethod https://api.github.com/repos/rushvoice/rushvoice/releases/latest
$asset = $rel.assets | Where-Object { $_.name -like "*_x64-setup.exe" } | Select-Object -First 1
$sums  = $rel.assets | Where-Object { $_.name -eq "SHA256SUMS.txt" }
$dir = Join-Path $env:TEMP "rushvoice-install"; New-Item -ItemType Directory -Force $dir | Out-Null
Invoke-WebRequest $asset.browser_download_url -OutFile (Join-Path $dir $asset.name)
Invoke-WebRequest $sums.browser_download_url  -OutFile (Join-Path $dir "SHA256SUMS.txt")
$expected = (Select-String -Path (Join-Path $dir "SHA256SUMS.txt") -Pattern ([regex]::Escape($asset.name))).Line.Split(" ")[0]
$actual = (Get-FileHash (Join-Path $dir $asset.name) -Algorithm SHA256).Hash.ToLower()
if ($expected -ne $actual) { throw "checksum mismatch" } else { "checksum ok" }
```

macOS (zsh):
```bash
set -e
DIR="$(mktemp -d)"; cd "$DIR"
API=https://api.github.com/repos/rushvoice/rushvoice/releases/latest
URL=$(curl -fsSL "$API" | grep -o '"browser_download_url": *"[^"]*_universal\.dmg"' | head -1 | cut -d'"' -f4)
SUMS=$(curl -fsSL "$API" | grep -o '"browser_download_url": *"[^"]*SHA256SUMS\.txt"' | head -1 | cut -d'"' -f4)
curl -fsSLO "$URL"; curl -fsSLO "$SUMS"
shasum -a 256 -c SHA256SUMS.txt --ignore-missing
```

### Step 3: Install

Windows: run the installer silently, then start the app.
```powershell
Start-Process -FilePath (Join-Path $dir $asset.name) -ArgumentList "/S" -Wait
Start-Process "$env:LOCALAPPDATA\RushVoice\app.exe"
```
The installed executable is named `app.exe` (the Start menu entry is "RushVoice"). If it is not at that path, search `$env:LOCALAPPDATA\RushVoice` and `$env:ProgramFiles\RushVoice`.

macOS: copy the app into /Applications and remove the download quarantine flag for this app only.
```bash
DMG=$(ls *_universal.dmg | head -1)
MNT=$(hdiutil attach "$DMG" -nobrowse | tail -1 | awk -F'\t' '{print $NF}')
cp -R "$MNT/RushVoice.app" /Applications/
hdiutil detach "$MNT"
xattr -dr com.apple.quarantine /Applications/RushVoice.app
open /Applications/RushVoice.app
```

### Step 4: Things only the user can do — explain them and wait
- **[user]** macOS: allow **Microphone** and **Accessibility** for RushVoice (System Settings → Privacy & Security), then quit and reopen RushVoice.
- **[user]** Open RushVoice → 設定 (Settings) → API キー, paste a Groq API key from https://console.groq.com/keys and save. Or sign in to a RushVoice account from Settings.
- **[user]** macOS: choose a hotkey in 設定 (the default Windows key 無変換 does not exist on Mac keyboards).

### Step 5: Verify
Ask the user to open a text editor, press the hotkey (Windows default: Ctrl + Shift + 無変換), speak, and press it again. Text should appear at the cursor. If not, use the troubleshooting table in section 1.

---

## 3. ソースからビルドする / Build from source

前提: Rust（rustup）・Node.js 20 以上・pnpm 9。Windows は MSVC ビルドツール、macOS は Xcode Command Line Tools。

```bash
git clone https://github.com/rushvoice/rushvoice.git
cd rushvoice
cargo install tauri-cli --version "^2" --locked
cd frontend && pnpm install
cd ../src-tauri
cargo tauri dev     # 開発用に起動
cargo tauri build   # インストーラーを作る
```

自分でビルドした版は、RushVoice アカウントのログインが無効で、自分の API キーで動きます。
