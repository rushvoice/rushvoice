// glossary_csv.rs — v0.1.10 機能 E: 辞書 CSV インポート/エクスポート
//
// SSOT: 内部設計メモ
//
// 仕様 (確定・既存 glossary_db.rs スキーマ準拠):
//   - CSV 列: id, pattern, replacement, context, note, is_regex, created_at, updated_at
//   - UTF-8 BOM 付き / CRLF 改行 / RFC 4180 準拠クォート
//   - is_regex は CSV で "true" / "false" 表記
//   - export 並び順: created_at ASC, id ASC (既存 list() と一致)
//   - import 時 id 列は無視 (再採番)
//   - 重複判定 key = pattern (大文字小文字区別)
//   - 不正行はスキップして error_count をカウント

use std::io::{Read, Write};

use thiserror::Error;

use crate::glossary_db::{GlossaryDb, GlossaryEntry, GlossaryDbError};

#[derive(Debug, Error)]
pub enum CsvError {
    #[error("CSV error: {0}")]
    Csv(#[from] csv::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("DB error: {0}")]
    Db(#[from] GlossaryDbError),
    #[error("CSV header missing or invalid: expected 'id,pattern,replacement,context,note,is_regex,created_at,updated_at', got {0:?}")]
    HeaderInvalid(Vec<String>),
}

/// CSV ヘッダー行 (順序固定)
pub const CSV_HEADER: [&str; 8] = [
    "id",
    "pattern",
    "replacement",
    "context",
    "note",
    "is_regex",
    "created_at",
    "updated_at",
];

/// UTF-8 BOM (Excel 互換)
pub const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// DB から全 entry を取得し CSV として writer に書き出す。
///
/// 出力:
///   - 先頭に UTF-8 BOM
///   - CRLF 改行
///   - 1 行目はヘッダー
///   - 順序は created_at ASC, id ASC (GlossaryDb::list() と一致)
///
/// 戻り値は書き出した行数 (= 全 entry 数)。
pub fn export_to_writer<W: Write>(db: &GlossaryDb, mut writer: W) -> Result<usize, CsvError> {
    // BOM
    writer.write_all(UTF8_BOM)?;

    // RFC 4180 + CRLF + 必要時クォート (Quoting::Necessary は csv crate のデフォルト)
    let mut wtr = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(writer);

    wtr.write_record(CSV_HEADER)?;

    let entries = db.list()?;
    let count = entries.len();
    for e in &entries {
        wtr.write_record([
            e.id.map(|i| i.to_string()).unwrap_or_default(),
            e.pattern.clone(),
            e.replacement.clone(),
            e.context.clone().unwrap_or_default(),
            e.note.clone().unwrap_or_default(),
            (if e.is_regex { "true" } else { "false" }).to_string(),
            e.created_at.to_string(),
            e.updated_at.to_string(),
        ])?;
    }
    wtr.flush()?;
    Ok(count)
}

/// reader から CSV を読み込み GlossaryEntry の Vec を構築する。
///
/// 仕様:
///   - 先頭 BOM (UTF-8) はスキップ
///   - ヘッダーは CSV_HEADER と完全一致のみ受理 (不一致時は HeaderInvalid Err)
///   - id 列は読み飛ばす (再採番される)
///   - pattern (trim 後) が空の行はエラーとして error_count++ してスキップ
///   - replacement は空でも許容 (削除ルール・recorder::apply_glossary_replacements 参照)
///   - is_regex は "true"/"1" を true・それ以外 false (空文字も false)
///   - created_at / updated_at の数値変換失敗時はエラー扱い (行をスキップ・error_count++)
///   - 列数不足/超過の行はエラーとして error_count++ してスキップ
///   - **CSV 内重複**: 同一 pattern が複数行あった場合は「後勝ち」で集約 (重複行は error_count に含めない)
///     後勝ちにする理由: ユーザーが手作業で末尾行に修正版を追記するパターンが想定されるため、
///     最新の意図を反映するのが直感的。preview 段階で前段に出た行は無視される。
///
/// 戻り値: (有効行の entries (CSV 内重複排除済), エラー行数)
pub fn parse_csv<R: Read + 'static>(reader: R) -> Result<(Vec<GlossaryEntry>, usize), CsvError> {
    // BOM 自動 skip + flexible 列数許容 (列数不足は手動チェック)
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(strip_bom_reader(reader));

    // ヘッダー検証
    let headers = rdr.headers()?.clone();
    let header_vec: Vec<String> = headers.iter().map(|s| s.to_string()).collect();
    if header_vec.len() != CSV_HEADER.len()
        || !header_vec.iter().zip(CSV_HEADER.iter()).all(|(a, b)| a == b)
    {
        return Err(CsvError::HeaderInvalid(header_vec));
    }

    let mut entries: Vec<GlossaryEntry> = Vec::new();
    let mut error_count: usize = 0;

    for result in rdr.records() {
        match result {
            Ok(record) => {
                if record.len() != CSV_HEADER.len() {
                    log::warn!(
                        "[glossary_csv] skip row: column count mismatch (got {}, expected {})",
                        record.len(),
                        CSV_HEADER.len()
                    );
                    error_count += 1;
                    continue;
                }
                // 列: 0=id (無視), 1=pattern, 2=replacement, 3=context, 4=note, 5=is_regex, 6=created_at, 7=updated_at
                //
                // Codex review P2 #2 (Day 30): empty replacement は「削除用ルール」として有効。
                // recorder::apply_glossary_replacements で空 replacement は pattern を削除する用途で使われており
                // (テスト patch3_apply_glossary_replacements_delete_via_empty_replacement 参照)、
                // export → import の round-trip でこれらの削除ルールが失われないよう、
                // empty pattern のみエラー扱い・empty replacement は許容する。
                //
                // Codex review P2 #3 (Day 30): pattern の whitespace は意図的な可能性 (先頭/末尾スペース
                // を含む削除ルール等)。export 側は trim せず raw 出力するため、round-trip でズレないよう
                // empty 判定だけ trim 後に行い、保存値は raw を保持する。
                let pattern_raw = record.get(1).unwrap_or("").to_string();
                let replacement = record.get(2).unwrap_or("").to_string();
                if pattern_raw.trim().is_empty() {
                    log::warn!(
                        "[glossary_csv] skip row: empty pattern (replacement={:?})",
                        replacement
                    );
                    error_count += 1;
                    continue;
                }
                let pattern = pattern_raw;
                let context_str = record.get(3).unwrap_or("");
                let context = if context_str.is_empty() {
                    None
                } else {
                    Some(context_str.to_string())
                };
                let note_str = record.get(4).unwrap_or("");
                let note = if note_str.is_empty() {
                    None
                } else {
                    Some(note_str.to_string())
                };
                let is_regex = parse_bool(record.get(5).unwrap_or(""));
                let created_at = match record.get(6).unwrap_or("").trim().parse::<i64>() {
                    Ok(v) => v,
                    Err(_) if record.get(6).unwrap_or("").trim().is_empty() => 0,
                    Err(e) => {
                        log::warn!(
                            "[glossary_csv] skip row: created_at parse failed: {:?} ({:?})",
                            record.get(6),
                            e
                        );
                        error_count += 1;
                        continue;
                    }
                };
                let updated_at = match record.get(7).unwrap_or("").trim().parse::<i64>() {
                    Ok(v) => v,
                    Err(_) if record.get(7).unwrap_or("").trim().is_empty() => 0,
                    Err(e) => {
                        log::warn!(
                            "[glossary_csv] skip row: updated_at parse failed: {:?} ({:?})",
                            record.get(7),
                            e
                        );
                        error_count += 1;
                        continue;
                    }
                };

                entries.push(GlossaryEntry {
                    id: None, // 再採番
                    pattern,
                    replacement,
                    context,
                    note,
                    is_regex,
                    created_at,
                    updated_at,
                });
            }
            Err(e) => {
                log::warn!("[glossary_csv] skip row: csv parse error: {:?}", e);
                error_count += 1;
            }
        }
    }

    // Codex review P2 (Day 30): CSV 内同一 pattern の重複は「後勝ち」で集約。
    // 出現順を保ちつつ最後の entry で上書きする。
    // 注: HashMap だと順序が崩れるため、出現順インデックスを別管理。
    let mut dedup_entries: Vec<GlossaryEntry> = Vec::with_capacity(entries.len());
    let mut index_of: std::collections::HashMap<String, usize> =
        std::collections::HashMap::with_capacity(entries.len());
    let mut dedup_count = 0usize;
    for e in entries.into_iter() {
        match index_of.get(&e.pattern) {
            Some(&idx) => {
                // 重複: 後勝ちで上書き
                dedup_entries[idx] = e;
                dedup_count += 1;
            }
            None => {
                index_of.insert(e.pattern.clone(), dedup_entries.len());
                dedup_entries.push(e);
            }
        }
    }
    if dedup_count > 0 {
        log::info!(
            "[glossary_csv] CSV intra-file dedup: {} rows collapsed (last-wins)",
            dedup_count
        );
    }

    Ok((dedup_entries, error_count))
}

/// "true" / "1" → true・空 / "false" / "0" / それ以外 → false
fn parse_bool(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1")
}

/// UTF-8 BOM (3 bytes 0xEF 0xBB 0xBF) を skip して残りを返す Read アダプタ。
/// 先頭 3 バイトが BOM でなければそのまま読み戻す。
fn strip_bom_reader<R: Read + 'static>(mut reader: R) -> Box<dyn Read> {
    let mut head = [0u8; 3];
    let mut read = 0usize;
    // 3 バイト読みきる (EOF まで埋め切る試み・read_exact は EOF 時 Err なので手動)
    while read < 3 {
        match reader.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => break,
        }
    }
    if read == 3 && head == [0xEF, 0xBB, 0xBF] {
        Box::new(reader)
    } else {
        // BOM ではなかった → head の有効バイトを巻き戻す
        let prefix = head[..read].to_vec();
        Box::new(std::io::Cursor::new(prefix).chain(reader))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glossary_db::GlossaryDb;
    use std::env;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn tmp_db(name: &str) -> PathBuf {
        let p = env::temp_dir()
            .join(format!("rushvoice_csv_test_{}", name))
            .join("glossary.db");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
        p
    }

    fn sample(pattern: &str, replacement: &str) -> GlossaryEntry {
        GlossaryEntry {
            id: None,
            pattern: pattern.to_string(),
            replacement: replacement.to_string(),
            context: None,
            note: None,
            is_regex: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn test_export_empty_db_writes_bom_and_header_only() {
        let path = tmp_db("export_empty");
        let db = GlossaryDb::open(&path).expect("open");
        let mut buf = Vec::new();
        let n = export_to_writer(&db, &mut buf).expect("export");
        assert_eq!(n, 0);
        // BOM 確認
        assert_eq!(&buf[0..3], UTF8_BOM);
        // BOM 以降にヘッダー
        let s = std::str::from_utf8(&buf[3..]).expect("utf8");
        // CRLF 終端
        assert!(s.starts_with("id,pattern,replacement,context,note,is_regex,created_at,updated_at\r\n"));
    }

    #[test]
    fn test_export_with_entries_serializes_correctly() {
        let path = tmp_db("export_with");
        let db = GlossaryDb::open(&path).expect("open");
        let mut e1 = sample("ますえく", "マツエク");
        e1.note = Some("メモ".to_string());
        e1.is_regex = false;
        db.upsert(&e1).expect("insert");

        let mut e2 = sample("\"特殊\"", "クォート,カンマ");
        db.upsert(&e2).expect("insert");
        // 引数で参照しないようにダミー使用
        e2.id = None;

        let mut buf = Vec::new();
        let n = export_to_writer(&db, &mut buf).expect("export");
        assert_eq!(n, 2);

        let s = std::str::from_utf8(&buf[3..]).expect("utf8"); // BOM skip
        // クォート + カンマ含む値が正しくエスケープされている (RFC 4180)
        assert!(s.contains("\"\"\"特殊\"\"\","), "quote escape: {}", s);
        assert!(s.contains("\"クォート,カンマ\""), "comma quote: {}", s);
        // ますえく / マツエク は素のまま (UTF-8)
        assert!(s.contains("ますえく"));
        assert!(s.contains("マツエク"));
        // CRLF が使われている
        assert!(s.contains("\r\n"));
    }

    #[test]
    fn test_parse_csv_basic() {
        let csv_str = "id,pattern,replacement,context,note,is_regex,created_at,updated_at\r\n\
                       ,foo,bar,,,false,0,0\r\n\
                       ,baz,qux,メール,メモ,true,1700000000,1700000100\r\n";
        let (entries, errors) = parse_csv(Cursor::new(csv_str.as_bytes().to_vec())).expect("parse");
        assert_eq!(entries.len(), 2);
        assert_eq!(errors, 0);
        assert_eq!(entries[0].pattern, "foo");
        assert_eq!(entries[0].replacement, "bar");
        assert!(entries[0].context.is_none());
        assert!(entries[0].note.is_none());
        assert!(!entries[0].is_regex);
        assert_eq!(entries[1].pattern, "baz");
        assert_eq!(entries[1].context.as_deref(), Some("メール"));
        assert_eq!(entries[1].note.as_deref(), Some("メモ"));
        assert!(entries[1].is_regex);
        assert_eq!(entries[1].created_at, 1_700_000_000);
        assert_eq!(entries[1].updated_at, 1_700_000_100);
        // id は無視されている
        assert_eq!(entries[0].id, None);
        assert_eq!(entries[1].id, None);
    }

    #[test]
    fn test_parse_csv_strips_bom() {
        let mut csv_with_bom = Vec::new();
        csv_with_bom.extend_from_slice(UTF8_BOM);
        csv_with_bom.extend_from_slice(
            b"id,pattern,replacement,context,note,is_regex,created_at,updated_at\r\n\
              ,a,A,,,false,0,0\r\n",
        );
        let (entries, errors) = parse_csv(Cursor::new(csv_with_bom)).expect("parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(errors, 0);
        assert_eq!(entries[0].pattern, "a");
    }

    #[test]
    fn test_parse_csv_header_invalid() {
        let csv_str = "wrong,header,line\r\n,a,A,,,false,0,0\r\n";
        let err = parse_csv(Cursor::new(csv_str.as_bytes().to_vec())).expect_err("should fail");
        match err {
            CsvError::HeaderInvalid(v) => {
                assert_eq!(v, vec!["wrong", "header", "line"]);
            }
            other => panic!("expected HeaderInvalid, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_csv_skips_invalid_rows() {
        // empty pattern / column mismatch / bad timestamp の 3 件をエラーとしてスキップ
        let csv_str = "id,pattern,replacement,context,note,is_regex,created_at,updated_at\r\n\
                       ,,EMPTY_PATTERN,,,false,0,0\r\n\
                       ,short,row\r\n\
                       ,bad_ts,r,,,false,not_a_number,0\r\n\
                       ,good,row,,,false,0,0\r\n";
        let (entries, errors) = parse_csv(Cursor::new(csv_str.as_bytes().to_vec())).expect("parse");
        assert_eq!(entries.len(), 1, "only the good row should parse");
        assert_eq!(entries[0].pattern, "good");
        assert_eq!(errors, 3, "3 invalid rows expected");
    }

    #[test]
    fn test_parse_csv_preserves_pattern_whitespace() {
        // Codex review P2 (Day 30): pattern 先頭/末尾 whitespace は保持される
        // (削除ルール等で意図的に空白を含めるユースケースを尊重)
        let csv_str = "id,pattern,replacement,context,note,is_regex,created_at,updated_at\r\n\
                       ,\" abc \",REP,,,false,0,0\r\n\
                       ,\"  \",ONLY_SPACE,,,false,0,0\r\n";
        let (entries, errors) = parse_csv(Cursor::new(csv_str.as_bytes().to_vec())).expect("parse");
        // " abc " は OK (whitespace 含み)・"  " は trim 後 empty なので error
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pattern, " abc ", "whitespace must be preserved");
        assert_eq!(errors, 1, "only-whitespace pattern must be error");
    }

    #[test]
    fn test_parse_csv_dedup_intra_file_last_wins() {
        // Codex review P2 (Day 30): CSV 内同一 pattern は最後の行が勝つ
        let csv_str = "id,pattern,replacement,context,note,is_regex,created_at,updated_at\r\n\
                       ,foo,FIRST,,,false,0,0\r\n\
                       ,bar,B,,,false,0,0\r\n\
                       ,foo,SECOND,,,false,0,0\r\n\
                       ,foo,THIRD,,,false,0,0\r\n";
        let (entries, errors) = parse_csv(Cursor::new(csv_str.as_bytes().to_vec())).expect("parse");
        assert_eq!(errors, 0);
        assert_eq!(entries.len(), 2, "foo は 1 件に集約・bar は 1 件");
        // 出現順は維持: foo (最初に出現した位置) → bar
        assert_eq!(entries[0].pattern, "foo");
        assert_eq!(entries[0].replacement, "THIRD", "last-wins");
        assert_eq!(entries[1].pattern, "bar");
        assert_eq!(entries[1].replacement, "B");
    }

    #[test]
    fn test_parse_csv_allows_empty_replacement_as_deletion_rule() {
        // Codex review P2 (Day 30): empty replacement は「削除ルール」として有効。
        // pattern → "" は recorder::apply_glossary_replacements で pattern 削除に使われる。
        let csv_str = "id,pattern,replacement,context,note,is_regex,created_at,updated_at\r\n\
                       ,削除対象,,,,false,0,0\r\n\
                       ,普通の行,置換先,,,false,0,0\r\n";
        let (entries, errors) = parse_csv(Cursor::new(csv_str.as_bytes().to_vec())).expect("parse");
        assert_eq!(errors, 0, "empty replacement must NOT be an error");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].pattern, "削除対象");
        assert_eq!(entries[0].replacement, "");
        assert_eq!(entries[1].pattern, "普通の行");
        assert_eq!(entries[1].replacement, "置換先");
    }

    #[test]
    fn test_round_trip_preserves_empty_replacement_deletion_rule() {
        // 削除ルール (replacement = "") も export → import で失われないこと
        let path = tmp_db("round_trip_empty_rep");
        let db = GlossaryDb::open(&path).expect("open");
        let mut e = sample("ますえぐ", "");
        e.note = Some("削除用".to_string());
        let _ = db.upsert(&e).expect("insert");
        let _ = db.upsert(&sample("普通の行", "置換先")).expect("insert");

        let mut buf = Vec::new();
        export_to_writer(&db, &mut buf).expect("export");
        let (parsed, errors) = parse_csv(Cursor::new(buf.clone())).expect("parse");
        assert_eq!(errors, 0);
        assert_eq!(parsed.len(), 2);
        // 削除ルール (replacement = "") が保持されている
        assert_eq!(parsed[0].pattern, "ますえぐ");
        assert_eq!(parsed[0].replacement, "");
        assert_eq!(parsed[0].note.as_deref(), Some("削除用"));
    }

    #[test]
    fn test_round_trip_export_import_preserves_data() {
        let path = tmp_db("round_trip");
        let db = GlossaryDb::open(&path).expect("open");
        let mut e = sample("a", "A");
        e.context = Some("メール".to_string());
        e.note = Some("コメント".to_string());
        e.is_regex = true;
        let _ = db.upsert(&e).expect("insert a");
        let _ = db.upsert(&sample("b", "B")).expect("insert b");

        let mut buf = Vec::new();
        export_to_writer(&db, &mut buf).expect("export");

        let (parsed, errors) = parse_csv(Cursor::new(buf.clone())).expect("parse");
        assert_eq!(errors, 0);
        assert_eq!(parsed.len(), 2);
        // 順序保証 (created_at ASC)
        assert_eq!(parsed[0].pattern, "a");
        assert_eq!(parsed[0].context.as_deref(), Some("メール"));
        assert_eq!(parsed[0].note.as_deref(), Some("コメント"));
        assert!(parsed[0].is_regex);
        assert_eq!(parsed[1].pattern, "b");
    }
}
