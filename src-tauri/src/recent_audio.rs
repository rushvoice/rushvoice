// 直近の録音を PC の中だけに残す (原因調べ用・2026-11-15)
//
// 文末に話していない「と」が付く・末尾に繰り返しが付く等の症状は、録音が無いと
// 「本当に言ったのか / 雑音を文字にしたのか」を確かめられない。そこで、設定で入れたときだけ
// 直近 KEEP_COUNT 件の録音を `<データフォルダ>/recent_audio/` に WAV で残す。
//
// - 既定は切 (外販予定のため、利用者の声を勝手に溜めない)。config.db の kv
//   `keep_recent_audio` = "1" のときだけ動く。
// - 外部へは送らない。古いものから自動で消し、件数を超えて溜まらない。
// - 残すのは増幅前・切り落とし前の音 (末尾に何が入っていたかを聞けるように)。
//   ファイル名に「実際に送った長さ」を入れる。

use std::path::{Path, PathBuf};

pub const ENABLE_KEY: &str = "keep_recent_audio";
pub const KEEP_COUNT: usize = 20;

pub fn dir() -> PathBuf {
    crate::paths::app_data_dir().join("recent_audio")
}

pub fn is_enabled() -> bool {
    crate::config_db::ConfigDb::open(&crate::config_db::default_config_db_path())
        .ok()
        .and_then(|db| db.get_kv(ENABLE_KEY).ok().flatten())
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// 設定が入っていれば録音を保存し、古い分を消す。失敗しても文字起こしは止めない。
pub fn save_if_enabled(samples_16k: &[f32], sent_len: usize) {
    if !is_enabled() || samples_16k.is_empty() {
        return;
    }
    let name = format!(
        "{}_len{}ms_sent{}ms.wav",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        samples_16k.len() * 1000 / 16_000,
        sent_len * 1000 / 16_000
    );
    match save_to(&dir(), &name, samples_16k) {
        Ok(p) => log::info!("[recent_audio] saved {}", p.display()),
        Err(e) => log::warn!("[recent_audio] save failed: {}", e),
    }
}

fn save_to(dir: &Path, name: &str, samples_16k: &[f32]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    std::fs::write(&path, crate::recorder::encode_wav_pcm16(samples_16k, 16_000, 1))?;
    prune(dir, KEEP_COUNT)?;
    Ok(path)
}

/// `.wav` を名前順 (= 日時順) に並べ、新しい `keep` 件より古いものを消す。
fn prune(dir: &Path, keep: usize) -> std::io::Result<()> {
    let mut wavs: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map_or(false, |x| x.eq_ignore_ascii_case("wav")))
        .collect();
    wavs.sort();
    if wavs.len() > keep {
        for p in &wavs[..wavs.len() - keep] {
            let _ = std::fs::remove_file(p);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_newest_files() {
        let d = std::env::temp_dir().join(format!("rv_recent_audio_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for i in 0..5 {
            save_to(&d, &format!("2026111{}-000000_x.wav", i), &[0.0; 1600]).unwrap();
        }
        std::fs::write(d.join("note.txt"), "keep me").unwrap();
        prune(&d, 3).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "20261112-000000_x.wav",
                "20261113-000000_x.wav",
                "20261114-000000_x.wav",
                "note.txt"
            ]
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
