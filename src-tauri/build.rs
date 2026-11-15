/// ゲートウェイ base URL をビルド時に埋め込む (2026-07-27)。
///
/// 以前は gateway_auth.rs に URL を直書きしていたが、追跡ファイルに内部 URL を
/// 置かない方針 (L518) に反し、将来の OSS 公開の障害にもなる。
///
/// 優先順位: 環境変数 RUSHVOICE_GATEWAY_BASE_URL → `gateway_url.txt` (Git 管理外)。
/// どちらも無ければ未設定のままビルドは通り、実行時にゲートウェイログインが
/// 無効化されるだけにする (URL を知らない開発者でもビルドできる状態を保つ)。
fn embed_gateway_base_url() {
    println!("cargo:rerun-if-env-changed=RUSHVOICE_GATEWAY_BASE_URL");
    println!("cargo:rerun-if-changed=gateway_url.txt");

    let from_env = std::env::var("RUSHVOICE_GATEWAY_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let url = from_env.or_else(|| {
        std::fs::read_to_string("gateway_url.txt")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });

    match url {
        Some(u) => println!(
            "cargo:rustc-env=RUSHVOICE_GATEWAY_BASE_URL={}",
            u.trim_end_matches('/')
        ),
        None => println!(
            "cargo:warning=RUSHVOICE_GATEWAY_BASE_URL が未設定です。ゲートウェイログインは \
             無効になります (BYOK のみ動作)。src-tauri/gateway_url.txt を用意してください。"
        ),
    }
}

fn main() {
    embed_gateway_base_url();

    // macOS: AVFAudio フレームワークをリンク (mic_permission/macos.rs から
    // AVAudioApplication.requestRecordPermissionWithCompletionHandler: を呼び出すため)。
    // 他 OS では cfg ガードで no-op。
    #[cfg(target_os = "macos")]
    {
        // AVAudioApplication 用 (iOS 系統 modern API)
        println!("cargo:rustc-link-lib=framework=AVFAudio");
        // AVCaptureDevice.requestAccessForMediaType: 用 (macOS 標準ルート)
        println!("cargo:rustc-link-lib=framework=AVFoundation");
    }

    tauri_build::build()
}
