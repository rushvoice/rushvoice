# RushVoice の導入手順 / Installation

この文書は、人がそのまま読んで進める手順と、AI エージェント（Claude Code・Codex・ChatGPT など）に読ませて代わりに進めてもらう手順の両方を兼ねています。
AI に頼むときは「`https://github.com/rushvoice/rushvoice/blob/main/INSTALL.md` を読んで、この PC に RushVoice を入れて」と伝えてください。

- 対応 OS: Windows 10 / 11（x64）、macOS 13 以降（Apple Silicon・Intel）
- 必要なもの: マイクと、次のどれか1つ（アカウント登録やログインはありません）
  - [Groq](https://console.groq.com/keys) の API キー（無料枠あり。料金は自分の Groq アカウントにかかる・いちばん手軽）
  - [OpenAI](https://platform.openai.com/) の API キー
  - 手元のローカル LLM や社内サーバーなど、OpenAI 互換の接続先（書き方は README の「接続先」）

---

## 1. 人が進める手順

### Windows

1. [Releases の最新版](https://github.com/rushvoice/rushvoice/releases/latest) から `RushVoice_<版>_x64-setup.exe` をダウンロードする。
2. ダブルクリックで開く。コード署名をしていないため「Windows によって PC が保護されました」と出たら、**このリポジトリの Releases から入手したファイルであることを確かめてから**「詳細情報」→「実行」を選ぶ。
3. インストールが終わると RushVoice が起動する。
4. 「設定」→「接続先と API キー」で接続先（初めてなら Groq）を選び、API キーを貼り付けて保存する。「接続テスト」でつながるか確かめられる。
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
6. 「設定」→「接続先と API キー」で接続先（初めてなら Groq）を選び、API キーを貼り付けて保存する。「接続テスト」でつながるか確かめられる。
7. メモなどを開き、ホットキーを押して話し、もう一度押すと、カーソル位置に文字が入る。

### うまく動かないとき

| 症状 | 確かめること |
|---|---|
| 文字が入らない | 「接続先と API キー」が保存され、「接続テスト」が通るか。Mac はアクセシビリティの許可と、許可後の再起動 |
| 録音されない・無音になる | OS のマイクの許可。使うマイクを「設定」で選ぶ |
| ホットキーが反応しない | 他のアプリと同じキーになっていないか。「設定」で別のキーにする |

解決しないときは [Issue](https://github.com/rushvoice/rushvoice/issues/new/choose) へ。ログや画面に API キーや個人情報が写っていないか確かめてから貼ってください。

---

## 2. AI エージェントが進める手順 / Instructions for AI agents

You are installing RushVoice on the user's computer. Follow these steps in order. Steps marked **[user]** need the user's action or explicit approval: explain why, then wait.

**Rules**
- Download only from `https://github.com/rushvoice/rushvoice/releases`. Never from other sites.
- Verify the SHA-256 checksum against `SHA256SUMS.txt` from the same release before running anything. Stop if it does not match.
- Never ask the user to paste their API key into the chat. The user enters it in the app's Settings screen themselves.
- Do not disable OS security features (no `spctl --master-disable`, no turning off SmartScreen or Defender).

### Step 1: Detect the OS
- Windows: `[System.Environment]::OSVersion` (PowerShell). Requires Windows 10/11 x64.
- macOS: `sw_vers -productVersion` and `uname -m`. Requires macOS 13+.

### Step 2 (Windows): download, verify, install

Works in Windows PowerShell 5.1 and PowerShell 7.
```powershell
$ErrorActionPreference = 'Stop'
$rel   = Invoke-RestMethod -UseBasicParsing https://api.github.com/repos/rushvoice/rushvoice/releases/latest
$asset = @($rel.assets | Where-Object { $_.name -like '*_x64-setup.exe' })[0]
$sums  = @($rel.assets | Where-Object { $_.name -eq 'SHA256SUMS.txt' })[0]
if (-not $asset -or -not $sums) { throw 'installer or SHA256SUMS.txt not found in the latest release' }
$dir = Join-Path $env:TEMP 'rushvoice-install'
New-Item -ItemType Directory -Force $dir | Out-Null
$exe = Join-Path $dir $asset.name
Invoke-WebRequest -UseBasicParsing $asset.browser_download_url -OutFile $exe
Invoke-WebRequest -UseBasicParsing $sums.browser_download_url  -OutFile (Join-Path $dir 'SHA256SUMS.txt')
$lines = @(Get-Content (Join-Path $dir 'SHA256SUMS.txt') | Where-Object {
  $p = $_ -split '\s+', 2; $p.Count -eq 2 -and $p[1].TrimStart('*') -ceq $asset.name })
if ($lines.Count -ne 1) { throw 'checksum line not found exactly once' }
$expected = ($lines[0] -split '\s+')[0].ToLower()
$actual   = (Get-FileHash $exe -Algorithm SHA256).Hash.ToLower()
if ($expected -ne $actual) { throw 'checksum mismatch - do not run the installer' }
'checksum ok'
$p = Start-Process -FilePath $exe -ArgumentList '/S' -Wait -PassThru
if ($p.ExitCode -ne 0) { throw "installer exit code $($p.ExitCode)" }
$app = @("$env:LOCALAPPDATA\RushVoice\app.exe", "$env:ProgramFiles\RushVoice\app.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $app) { throw 'installed app not found' }
Start-Process $app
```
The installed executable is named `app.exe`; the Start menu entry is "RushVoice".

### Step 2 (macOS): download, verify, install

```bash
set -euo pipefail
DIR="$(mktemp -d)"; cd "$DIR"
curl -fsSL https://api.github.com/repos/rushvoice/rushvoice/releases/latest -o release.json
DMG_URL=$(grep -o '"browser_download_url": *"[^"]*_universal\.dmg"' release.json | head -1 | cut -d'"' -f4)
SUMS_URL=$(grep -o '"browser_download_url": *"[^"]*/SHA256SUMS\.txt"' release.json | head -1 | cut -d'"' -f4)
[ -n "$DMG_URL" ] && [ -n "$SUMS_URL" ] || { echo "release assets not found"; exit 1; }
DMG="${DMG_URL##*/}"
curl -fsSLo "$DMG" "$DMG_URL"
curl -fsSLo SHA256SUMS.txt "$SUMS_URL"
LINE=$(awk -v f="$DMG" '{n=$2; sub(/^\*/,"",n); if (n==f) print}' SHA256SUMS.txt)
[ "$(printf '%s\n' "$LINE" | grep -c .)" = "1" ] || { echo "checksum line not found exactly once"; exit 1; }
printf '%s\n' "$LINE" | shasum -a 256 -c -
```

Before copying, check for an existing install:
- If `/Applications/RushVoice.app` exists: **[user]** ask the user to quit RushVoice and confirm replacing it. Then remove the old app with `rm -rf /Applications/RushVoice.app` (only that path).

```bash
MNT="$DIR/mnt"; mkdir -p "$MNT"
hdiutil attach "$DMG" -nobrowse -readonly -mountpoint "$MNT"
trap 'hdiutil detach "$MNT" >/dev/null 2>&1 || true' EXIT
[ -d "$MNT/RushVoice.app" ] || { echo "RushVoice.app not found in dmg"; exit 1; }
cp -R "$MNT/RushVoice.app" /Applications/
```

The app is not notarized by Apple, so macOS will block the first launch. **[user]** Explain this and ask which way the user prefers:
1. The user opens it themselves: Finder → Applications → right-click RushVoice → Open (or System Settings → Privacy & Security → "Open Anyway"). Recommended.
2. With the user's explicit approval, remove the download flag for this one app only: `xattr -dr com.apple.quarantine /Applications/RushVoice.app`, then `open /Applications/RushVoice.app`.

### Step 3: Things only the user can do — explain them and wait
- **[user]** macOS: allow **Microphone** and **Accessibility** for RushVoice (System Settings → Privacy & Security), then quit and reopen RushVoice.
- **[user]** Open RushVoice → 設定 (Settings) → 接続先と API キー (Connection and API key). Choose Groq (simplest; key from https://console.groq.com/keys), OpenAI, or a custom OpenAI-compatible endpoint such as a local LLM, enter the key, save, and press 接続テスト (Test connection). There is no account or sign-in.
- **[user]** macOS: choose a hotkey in 設定 (the default Windows key 無変換 does not exist on Mac keyboards).

### Step 4: Verify
Ask the user to open a text editor, press the hotkey (Windows default: Ctrl + Shift + 無変換), speak, and press it again. Text should appear at the cursor. If not, use the troubleshooting table in section 1.

---

## 3. ソースからビルドする / Build from source

前提: Rust（rustup）・Node.js 20 以上・pnpm 9。Windows は MSVC ビルドツール、macOS は Xcode Command Line Tools。
1行ずつ実行してください（Windows PowerShell 5.1 でも動きます）。

```bash
git clone https://github.com/rushvoice/rushvoice.git
cd rushvoice
cargo install tauri-cli --version "^2" --locked
cd frontend
pnpm install
cd ../src-tauri
cargo tauri dev
```

インストーラーを作るときは、最後の行を `cargo tauri build` にします。公式版と同じく、自分の API キーかローカル LLM などの接続先で動きます。
