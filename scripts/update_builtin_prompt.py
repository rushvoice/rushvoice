"""ビルトインプロファイルの後処理プロンプトを profile.db へ反映する。

なぜ必要か:
  ビルトインプリセット (src-tauri/src/profile_db.rs の BUILTIN_PRESETS) は
  「未投入時のみ INSERT」する設計で、起動時に既存行を上書きしない
  (ユーザーが編集した内容を毎回踏みつぶさないため)。
  そのためコード側のプロンプトを直しても、既にインストール済みの PC には反映されない。
  コードと DB は反映経路が違う (教訓 L602)。

使い方:
    python scripts/update_builtin_prompt.py --check          # 現在値の確認だけ
    python scripts/update_builtin_prompt.py --apply          # 反映する

注意:
  profile.db は WAL モード。バックアップは -wal / -shm も一緒に取らないと
  中身が空のコピーになる (実際に発生済み)。本スクリプトは3点セットで退避する。
"""

from __future__ import annotations

import argparse
import os
import shutil
import sqlite3
import sys
import time
from datetime import datetime

# コード側 (profile_db.rs) と一致させること。片方だけ変えると再び食い違う。
TARGETS: dict[str, str] = {
    # 2026-07-27 圧縮: 抽象語(敬語レベル)は守られないので具体例だけ残す。
    # 「常体は常体のまま/敬体は敬体のまま」の説明は例で足りるため削除 (75字→46字)。
    "natural": (
        "文体・内容を変えない(『しといて』を『してください』にしない)。"
        "発話にない情報を追加しない。"
    ),
}


def db_path() -> str:
    return os.path.join(os.environ["APPDATA"], "RushVoice", "profile.db")


def backup(path: str) -> str:
    stamp = datetime.now().strftime("%Y%m%d_%H%M%S")
    made = []
    for suffix in ("", "-wal", "-shm"):
        src = path + suffix
        if os.path.exists(src):
            dst = f"{path}.{stamp}.bak{suffix}"
            shutil.copy2(src, dst)
            made.append(os.path.basename(dst))
    print(f"backup: {', '.join(made)}")
    return stamp


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--apply", action="store_true", help="実際に更新する")
    ap.add_argument("--check", action="store_true", help="現在値を表示するだけ")
    args = ap.parse_args()

    path = db_path()
    if not os.path.exists(path):
        print(f"NG: profile.db が見つかりません: {path}")
        return 1

    conn = sqlite3.connect(path)
    try:
        rows = {
            k: v
            for k, v in conn.execute(
                "SELECT builtin_key, llm_postprocess_prompt FROM profiles"
                " WHERE builtin_key IS NOT NULL"
            )
        }

        changed = []
        for key, new_prompt in TARGETS.items():
            current = rows.get(key)
            if current is None:
                print(f"skip: builtin_key={key} は DB に存在しません")
                continue
            if current == new_prompt:
                print(f"same: {key} は既に最新です")
                continue
            changed.append(key)
            print(f"--- {key} ---")
            print(f"  現在: {current}")
            print(f"  新規: {new_prompt}")

        if not changed:
            print("更新対象はありません。")
            return 0
        if not args.apply:
            print("\n(--apply を付けると実際に更新します)")
            return 0

        backup(path)
        # created_at / updated_at は INTEGER (unix 秒)。
        # SQLite は動的型付けなので datetime('now') を入れても書けてしまうが、
        # Rust 側は i64 として読むため行のデシリアライズが失敗し、
        # プロファイル一覧が丸ごと空になる (= ホーム画面の選択欄が消える)。
        # 2026-07-27 に実際に発生させた事故。必ず整数を入れること。
        now = int(time.time())
        for key in changed:
            conn.execute(
                "UPDATE profiles SET llm_postprocess_prompt = ?,"
                " updated_at = ? WHERE builtin_key = ?",
                (TARGETS[key], now, key),
            )
        conn.commit()

        # 反映確認 (書いたつもりで書けていない事故を防ぐ)
        for key in changed:
            got = conn.execute(
                "SELECT llm_postprocess_prompt FROM profiles WHERE builtin_key = ?",
                (key,),
            ).fetchone()[0]
            status = "OK" if got == TARGETS[key] else "NG"
            print(f"{status}: {key} を更新しました")
            if status == "NG":
                return 1

        # 型の健全性チェック: 1 行でも TEXT が混ざると Rust 側の読み込みが落ち、
        # プロファイル一覧が空になる (ホーム画面の選択欄が消える)。
        bad = conn.execute(
            "SELECT builtin_key FROM profiles"
            " WHERE typeof(created_at) != 'integer' OR typeof(updated_at) != 'integer'"
        ).fetchall()
        if bad:
            print(f"NG: 日時列の型が不正な行があります: {[b[0] for b in bad]}")
            return 1
        print("OK: 日時列の型はすべて integer")
        print("\n完了。プロンプトは発話ごとに読み直されるため、再起動は不要です。")
        return 0
    finally:
        conn.close()


if __name__ == "__main__":
    sys.exit(main())
