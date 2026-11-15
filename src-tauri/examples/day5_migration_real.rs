//! Day 5 後半 設定マイグレーション 実機検証 example
//!
//! 実行:
//!   cargo run --example day5_migration_real
//!
//! 動作:
//! 1. std::env::temp_dir() に ダミー v1 config.json を作成
//! 2. migrate_v1_to_v2() で SQLite + secrets.encrypted.json 生成
//! 3. 生成された SQLite から hotkey/dictionary/fillers/transcribeModel を読み出し
//! 4. 生成された secrets.encrypted.json から API キーを復号
//! 5. 元の値と一致するか比較し OVERALL: PASS / FAIL を出力
//! 6. 検証後にディレクトリごとクリーンアップ

use std::env;
use std::fs;

use app_lib::config_db::ConfigDb;
use app_lib::crypto::load_secrets;
use app_lib::migration::migrate_v1_to_v2;

fn main() {
    let tmp_root = env::temp_dir().join("rushvoice_day5_migration_real");
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(&tmp_root).expect("mkdir");

    let v1_path = tmp_root.join("v1_config.json");
    let v2_db_path = tmp_root.join("config.db");
    let v2_secrets_path = tmp_root.join("config.secrets.encrypted.json");

    let v1_json = r#"{
      "openAiApiKey": "sk-day5_migration_test",
      "groqApiKey": "gsk_day5_migration_test",
      "startKey": "RightCtrl",
      "stopKey": "RightCtrl",
      "holdToTalkKey": "RightCtrl",
      "transcribeModel": "gpt-4o-mini-transcribe",
      "language": "ja",
      "dictionary": {
        "アクアボイス": "Aqua Voice",
        "タイプレス": "Typeless",
        "スーパーウィスパー": "Superwhisper"
      },
      "fillers": ["えー", "あの", "うーん", "まあ"]
    }"#;
    fs::write(&v1_path, v1_json).expect("write v1");

    println!("[Day5-Migration] v1 source: {}", v1_path.display());
    println!("[Day5-Migration] v2 db:     {}", v2_db_path.display());
    println!("[Day5-Migration] v2 secrets: {}", v2_secrets_path.display());

    let report = match migrate_v1_to_v2(&v1_path, &v2_db_path, &v2_secrets_path) {
        Ok(Some(r)) => r,
        Ok(None) => {
            eprintln!("[Day5-Migration] FAIL: v1 absent (unexpected)");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("[Day5-Migration] FAIL migrate: {:?}", e);
            std::process::exit(1);
        }
    };

    println!(
        "[Day5-Migration] migrated: items_count={} keys_migrated={:?}",
        report.items_count, report.keys_migrated
    );

    // SQLite 検証
    let db = ConfigDb::open(&v2_db_path).expect("open db");
    let start_key = db.get_kv("start_key").expect("get").unwrap_or_default();
    let language = db.get_kv("language").expect("get").unwrap_or_default();
    let transcribe_model = db
        .get_kv("transcribe_model")
        .expect("get")
        .unwrap_or_default();
    let dict_len = db.select_dictionary().expect("select").len();
    let fillers = db.select_fillers().expect("select");

    let kv_ok = start_key == "RightCtrl"
        && language == "ja"
        && transcribe_model == "gpt-4o-mini-transcribe";
    let dict_ok = dict_len == 3;
    let fillers_ok = fillers.len() == 4;

    // secrets 検証
    let bundle = load_secrets(&v2_secrets_path)
        .expect("load secrets")
        .expect("Some bundle");
    let groq_ok = bundle.groq_api_key.as_deref() == Some("gsk_day5_migration_test");
    let openai_ok = bundle.openai_api_key.as_deref() == Some("sk-day5_migration_test");

    println!(
        "[Day5-Migration] settings_kv  = {} (start_key={}, language={}, model={})",
        if kv_ok { "PASS" } else { "FAIL" },
        start_key,
        language,
        transcribe_model
    );
    println!(
        "[Day5-Migration] dictionary   = {} (count={})",
        if dict_ok { "PASS" } else { "FAIL" },
        dict_len
    );
    println!(
        "[Day5-Migration] fillers      = {} (count={})",
        if fillers_ok { "PASS" } else { "FAIL" },
        fillers.len()
    );
    println!(
        "[Day5-Migration] groq secret  = {}",
        if groq_ok { "PASS" } else { "FAIL" }
    );
    println!(
        "[Day5-Migration] openai secret= {}",
        if openai_ok { "PASS" } else { "FAIL" }
    );

    if let Some((src, ts, cnt)) = db.latest_migration_log().expect("log") {
        println!(
            "[Day5-Migration] migration_log: source={} migrated_at={} items_count={}",
            src, ts, cnt
        );
    }

    let _ = fs::remove_dir_all(&tmp_root);
    println!("[Day5-Migration] cleanup: dir removed");

    if kv_ok && dict_ok && fillers_ok && groq_ok && openai_ok {
        println!("[Day5-Migration] OVERALL: PASS");
    } else {
        eprintln!("[Day5-Migration] OVERALL: FAIL");
        std::process::exit(1);
    }
}
