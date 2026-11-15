"""辞書に「語彙ヒント」を追加する (pattern == replacement の登録)。

語彙ヒントとは:
  置換では直せない実在語を、候補として提示するためだけの登録
  (例「柏→柏」「店舗→店舗」)。単純置換ではないので、その語を普通に喋っても壊れない。
  Whisper prompt と、後処理 LLM の混同語ヒント (build_confusable_hint) の
  両方に供給される。

  ⚠️ 誤って聞こえる**側**の語は登録しないこと。
     例: 「店舗」と言ったのに「テンポ」と出る場合、登録するのは「店舗」だけ。
     「テンポ」を登録すると Whisper がそちらへ寄り、症状が悪化する。

使い方:
    python scripts/add_vocabulary_hint.py 店舗 --note "テンポへの誤変換対策"
    python scripts/add_vocabulary_hint.py 店舗 --context 業務用語 --dry-run

注意:
  glossary.db は WAL モード。バックアップは -wal / -shm ごと取る (L617)。
"""

from __future__ import annotations

import argparse
import os
import shutil
import sqlite3
import sys
import time
from datetime import datetime


def db_path() -> str:
    return os.path.join(os.environ["APPDATA"], "RushVoice", "glossary.db")


def backup(path: str) -> None:
    stamp = datetime.now().strftime("%Y%m%d_%H%M%S")
    made = []
    for suffix in ("", "-wal", "-shm"):
        src = path + suffix
        if os.path.exists(src):
            dst = f"{path}.{stamp}.bak{suffix}"
            shutil.copy2(src, dst)
            made.append(os.path.basename(dst))
    print(f"backup: {', '.join(made)}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("word", help="登録する正しい表記 (誤変換される側ではない)")
    ap.add_argument("--context", default="", help="分類 (例: 人名 / 地名 / 業務用語)")
    ap.add_argument("--note", default="", help="登録理由のメモ")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    word = args.word.strip()
    if not word:
        print("NG: 語が空です")
        return 1

    path = db_path()
    if not os.path.exists(path):
        print(f"NG: glossary.db が見つかりません: {path}")
        return 1

    conn = sqlite3.connect(path)
    try:
        dup = conn.execute(
            "SELECT pattern, replacement FROM glossary WHERE pattern = ?", (word,)
        ).fetchone()
        if dup:
            print(f"skip: 「{word}」は既に登録されています (pattern={dup[0]} → {dup[1]})")
            return 0

        print(f"追加: pattern=「{word}」 replacement=「{word}」 (語彙ヒント・置換しない)")
        if args.context:
            print(f"  context: {args.context}")
        if args.note:
            print(f"  note: {args.note}")
        if args.dry_run:
            print("\n(--dry-run のため実際には追加していません)")
            return 0

        backup(path)
        now = int(time.time())
        conn.execute(
            "INSERT INTO glossary (pattern, replacement, context, note, is_regex,"
            " created_at, updated_at) VALUES (?, ?, ?, ?, 0, ?, ?)",
            (word, word, args.context, args.note, now, now),
        )
        conn.commit()

        got = conn.execute(
            "SELECT pattern, replacement FROM glossary WHERE pattern = ?", (word,)
        ).fetchone()
        if not got or got[0] != word or got[1] != word:
            print("NG: 追加を確認できませんでした")
            return 1
        total = conn.execute("SELECT COUNT(*) FROM glossary").fetchone()[0]
        print(f"OK: 追加しました (辞書の総登録数 {total})")
        print("辞書は発話ごとに読み直されるため、再起動は不要です。")
        return 0
    finally:
        conn.close()


if __name__ == "__main__":
    sys.exit(main())
