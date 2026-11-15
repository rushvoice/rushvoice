fn main() {
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
