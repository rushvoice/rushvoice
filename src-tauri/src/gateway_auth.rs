// gateway_auth.rs — Gateway ログイン (feature/gateway-login)
//
// 設計 SSOT: 内部設計メモ §A-2
// ゲートウェイ API (Cloudflare Workers・本番稼働):
//   - POST /auth/request-code  {"email"} → {"ok":true} (メールで 6 桁コード送付・10 分有効)
//   - POST /auth/verify        {"email","code"} → {"token":"<JWT>"}
//       失敗: 401 invalid_code / code_expired, 429 too_many_attempts
//   - GET  /v1/me  Authorization: Bearer <JWT>
//       → {"email", "entitlement": {"plan","status","current_period_end"}}
//
// 責務:
//   - Tauri コマンド 4 本 (request_code / verify_code / status / logout)
//   - JWT は既存 DPAPI 暗号化保存 (crypto::save_secrets の gateway_jwt フィールド) を流用
//   - 転写経路モード (config_db KV `transcribe_route_mode`) の自動切替
//     (ログイン成功 → "gateway" / ログアウト → "byok"・BYOK UI は社内向けに温存)
//   - 状態変更時は `gateway:auth-changed` を emit (L145: invoke 応答 + emit の二重通知)
//
// 機密: JWT / メールアドレスの実値をログに出さない (email はドメインのみ可)。

use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

use crate::config_db::{default_config_db_path, ConfigDb};
use crate::crypto::{default_secrets_path, load_secrets, save_secrets};

/// 本番ゲートウェイ (Cloudflare Workers)。
///
/// 2026-07-27: 内部 URL を追跡ファイルへ直書きしない方針 (L518) と、将来の OSS 公開に
/// 備えて**ビルド時注入**へ変更した。値は build.rs が環境変数
/// `RUSHVOICE_GATEWAY_BASE_URL` または Git 管理外の `src-tauri/gateway_url.txt` から埋め込む。
/// 実行時は `RUSHVOICE_GATEWAY_URL` 環境変数で更に上書きでき、dev で staging に向けられる。
///
/// 未注入 (None) のビルドではゲートウェイログインが使えず BYOK のみ動作する。
pub const DEFAULT_GATEWAY_BASE_URL: Option<&str> = option_env!("RUSHVOICE_GATEWAY_BASE_URL");

/// 転写経路モードの config_db KV キー。値: "byok" (default) | "gateway"。
pub const TRANSCRIBE_ROUTE_MODE_KEY: &str = "transcribe_route_mode";
pub const ROUTE_MODE_BYOK: &str = "byok";
pub const ROUTE_MODE_GATEWAY: &str = "gateway";

/// ログイン中メールアドレスの config_db KV キー (表示用・JWT とは別管理)。
pub const GATEWAY_EMAIL_KEY: &str = "gateway_email";

/// 直近に確認したプランの config_db KV キー (プラン別ゲートの即時判定用キャッシュ)。
///
/// UI ゲートのたびにネットワークを待つと画面が固まるため、`/v1/me` の結果をここへ写す。
/// 正は常にゲートウェイ側で、これはあくまで表示を速くするための写し。
pub const GATEWAY_PLAN_KEY: &str = "gateway_plan";

/// 認証状態変更イベント名 (payload: GatewayStatus)。
pub const AUTH_CHANGED_EVENT: &str = "gateway:auth-changed";

/// ゲートウェイ base URL (末尾スラッシュなし)。
///
/// 実行時の環境変数 → ビルド時注入 の順に見る。どちらも無ければ空文字を返し、
/// 呼出側 (is_gateway_configured) が「ゲートウェイ未設定」として扱う。
pub fn gateway_base_url() -> String {
    match std::env::var("RUSHVOICE_GATEWAY_URL") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => DEFAULT_GATEWAY_BASE_URL
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .unwrap_or_default(),
    }
}

/// ゲートウェイ URL が判明しているか (未注入ビルドでのログイン試行を防ぐ)。
pub fn is_gateway_configured() -> bool {
    !gateway_base_url().is_empty()
}

/// frontend へ返す認証状態。
/// state: "logged_out" | "active" | "expired" | "error"
///   - logged_out: JWT 未保存
///   - active:     /v1/me 200 (plan/status/期限あり)
///   - expired:    /v1/me 401/403 (JWT 失効・再ログイン必要)
///   - error:      ネットワーク等の一時エラー (JWT は保持したまま)
#[derive(Debug, Clone, Serialize)]
pub struct GatewayStatus {
    pub state: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub status: Option<String>,
    pub current_period_end: Option<String>,
    pub message: Option<String>,
}

impl GatewayStatus {
    pub fn logged_out() -> Self {
        GatewayStatus {
            state: "logged_out".into(),
            email: None,
            plan: None,
            status: None,
            current_period_end: None,
            message: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 純粋関数 (L294: 検証済み関数に分岐を混ぜず新関数に抽出・単体テスト対象)
// ---------------------------------------------------------------------------

/// /auth/verify の失敗レスポンスをユーザー向け日本語メッセージに変換する。
pub fn verify_error_message(status: u16, body: &str) -> String {
    match status {
        401 if body.contains("code_expired") => {
            "コードの有効期限が切れています（10分）。もう一度コードを送信してください。".to_string()
        }
        401 => "コードが正しくありません。メールの 6 桁コードを確認してください。".to_string(),
        429 => "試行回数の上限に達しました。しばらく待ってからやり直してください。".to_string(),
        s => format!("ログインに失敗しました (HTTP {})", s),
    }
}

/// /v1/me の 200 レスポンスから抽出した情報 (clippy::type_complexity 対策の名前付き struct)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeInfo {
    pub email: String,
    pub plan: Option<String>,
    pub status: Option<String>,
    pub current_period_end: Option<String>,
}

/// /v1/me の 200 レスポンス JSON から MeInfo を抽出する。
/// パース不能なら None (呼出側で error 扱い)。
pub fn parse_me_response(body: &str) -> Option<MeInfo> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let email = v.get("email")?.as_str()?.to_string();
    let ent = v.get("entitlement");
    let pick = |key: &str| -> Option<String> {
        ent.and_then(|e| e.get(key))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
    };
    Some(MeInfo {
        email,
        plan: pick("plan"),
        status: pick("status"),
        current_period_end: pick("current_period_end"),
    })
}

/// 購入可能なプラン ID (ゲートウェイ `/v1/billing/checkout` の `plan` と一致させる)。
pub const PURCHASABLE_PLANS: [&str; 2] = ["standard", "pro"];

/// アプリから購入を開始できるプランかどうか。
/// 未知の値をそのままゲートウェイへ投げて 400 を踏むのを防ぐ。
pub fn is_purchasable_plan(plan: &str) -> bool {
    PURCHASABLE_PLANS.contains(&plan)
}

/// `/v1/billing/checkout` の 200 レスポンスから Checkout URL を取り出す。
///
/// 決済ページへ誘導する値なので、**https 以外は採用しない**
/// (万一ゲートウェイ側が壊れた値を返しても、ブラウザで任意 URL を開かせない)。
pub fn parse_checkout_url(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let url = v.get("url")?.as_str()?.trim().to_string();
    if url.starts_with("https://") {
        Some(url)
    } else {
        None
    }
}

/// `/v1/billing/checkout` の失敗をユーザー向け日本語メッセージに変換する。
pub fn checkout_error_message(status: u16, body: &str) -> String {
    match status {
        401 | 403 => {
            "ログインの有効期限が切れています。設定画面から再度ログインしてください。".to_string()
        }
        400 if body.contains("invalid_plan") => {
            "選択されたプランが不明です。アプリを最新版に更新してください。".to_string()
        }
        502 => "決済サービスに接続できませんでした。時間をおいてやり直してください。".to_string(),
        s => format!("購入手続きの開始に失敗しました (HTTP {})", s),
    }
}

/// 上位プラン限定機能 (2026-07-28 決定): 議事録モード / セミナー字幕 / 優先サポート。
///
/// 判定規則:
///   - `None` (未ログイン = BYOK 運用) → 許可。社内利用と自前キー運用を止めないため。
///   - `pro`   → 許可
///   - `trial` → 許可 (14 日間フル機能トライアルという決裁の直接の帰結)
///   - `standard` / その他 → 不可
///
/// ⚠️ これは **UX 上のゲートであって安全境界ではない**。アプリはユーザーの手元で動くので、
/// 改造すれば迂回できる。課金の正当性はゲートウェイ側の entitlement 検証が担保する。
pub fn plan_allows_pro_features(plan: Option<&str>) -> bool {
    match plan {
        None => true,
        Some(p) => matches!(p.trim(), "pro" | "trial"),
    }
}

/// プラン別ゲートの判定結果 (frontend へ返す)。
#[derive(Debug, Clone, Serialize)]
pub struct ProFeatureAccess {
    /// 上位プラン限定機能を使えるか。
    pub allowed: bool,
    /// 判定に使ったプラン (未ログインなら None)。
    pub plan: Option<String>,
    /// ゲートウェイにログイン中か (false = BYOK 運用)。
    pub logged_in: bool,
}

/// メールアドレスの最低限の形式チェック (@ を含み前後が非空)。
pub fn is_plausible_email(email: &str) -> bool {
    let t = email.trim();
    match t.split_once('@') {
        Some((local, domain)) => !local.is_empty() && domain.contains('.') && !domain.starts_with('.'),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// 内部ヘルパー
// ---------------------------------------------------------------------------

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(crate::transcribe::USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP client build failed: {}", e))
}

fn set_kv_best_effort(key: &str, value: &str) {
    match ConfigDb::open(&default_config_db_path()) {
        Ok(db) => {
            if let Err(e) = db.set_kv(key, value) {
                log::warn!("[gateway-auth] set_kv({}) failed: {}", key, e);
            }
        }
        Err(e) => log::warn!("[gateway-auth] config_db open failed: {}", e),
    }
}

fn get_kv_opt(key: &str) -> Option<String> {
    ConfigDb::open(&default_config_db_path())
        .ok()
        .and_then(|db| db.get_kv(key).ok().flatten())
        .filter(|s| !s.is_empty())
}

/// 保存済み JWT を返す (未保存 / 復号失敗は None)。
fn load_saved_jwt() -> Option<String> {
    match load_secrets(&default_secrets_path()) {
        Ok(Some(bundle)) => bundle.gateway_jwt,
        Ok(None) => None,
        Err(e) => {
            log::warn!("[gateway-auth] load_secrets failed: {}; treating as logged_out", e);
            None
        }
    }
}

/// ゲートウェイが返す延長用トークンのヘッダー名 (rushvoice-gateway の index.ts と一致させる)。
pub const REFRESH_TOKEN_HEADER: &str = "X-RushVoice-Refresh-Token";

/// 延長用の新しいトークンを保存し直す (失敗しても今の転写は成立しているので警告だけ)。
///
/// ログアウト済みのとき (保存済み JWT が無い) は保存しない = ログアウトを勝手に取り消さない。
pub fn store_refreshed_jwt(jwt: &str) {
    if jwt.split('.').count() != 3 || load_saved_jwt().is_none() {
        return;
    }
    match store_jwt(Some(jwt)) {
        Ok(()) => log::info!("[gateway-auth] login token extended"),
        Err(e) => log::warn!("[gateway-auth] store refreshed token failed: {}", e),
    }
}

/// /v1/me の応答に延長用トークン (`token`) があれば取り出す。
pub fn parse_me_refresh_token(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("token")?.as_str().map(|s| s.to_string())
}

/// JWT を DPAPI 保存する (既存 BYOK キーは温存)。jwt=None で削除 (ログアウト)。
fn store_jwt(jwt: Option<&str>) -> Result<(), String> {
    let path = default_secrets_path();
    let existing = load_secrets(&path)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    save_secrets(
        &path,
        existing.groq_api_key.as_deref(),
        existing.openai_api_key.as_deref(),
        jwt,
    )
    .map_err(|e| e.to_string())
}

fn emit_auth_changed(app: &AppHandle, status: &GatewayStatus) {
    if let Err(e) = app.emit(AUTH_CHANGED_EVENT, status) {
        log::warn!("[gateway-auth] emit {} failed: {:?}", AUTH_CHANGED_EVENT, e);
    }
}

/// JWT で /v1/me を叩いて GatewayStatus を構築する。
async fn fetch_status_with_jwt(jwt: &str) -> GatewayStatus {
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => {
            return GatewayStatus {
                state: "error".into(),
                email: get_kv_opt(GATEWAY_EMAIL_KEY),
                plan: None,
                status: None,
                current_period_end: None,
                message: Some(e),
            }
        }
    };
    let url = format!("{}/v1/me", gateway_base_url());
    match client.get(&url).bearer_auth(jwt).send().await {
        Ok(resp) => {
            let http_status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if http_status.is_success() {
                if let Some(t) = parse_me_refresh_token(&body) {
                    store_refreshed_jwt(&t);
                }
                match parse_me_response(&body) {
                    Some(me) => {
                        // プラン別ゲートの即時判定用に写しを更新する
                        // (購入直後の反映もここで拾う)。
                        set_kv_best_effort(GATEWAY_PLAN_KEY, me.plan.as_deref().unwrap_or(""));
                        GatewayStatus {
                            state: "active".into(),
                            email: Some(me.email),
                            plan: me.plan,
                            status: me.status,
                            current_period_end: me.current_period_end,
                            message: None,
                        }
                    }
                    None => GatewayStatus {
                        state: "error".into(),
                        // (パース不能時はプランの写しを触らない = 直前の判定を維持)
                        email: get_kv_opt(GATEWAY_EMAIL_KEY),
                        plan: None,
                        status: None,
                        current_period_end: None,
                        message: Some("アカウント情報の取得結果を解釈できませんでした".into()),
                    },
                }
            } else if http_status.as_u16() == 401 || http_status.as_u16() == 403 {
                GatewayStatus {
                    state: "expired".into(),
                    email: get_kv_opt(GATEWAY_EMAIL_KEY),
                    plan: None,
                    status: None,
                    current_period_end: None,
                    message: Some(
                        "ログインの有効期限が切れています。再度ログインしてください。".into(),
                    ),
                }
            } else {
                GatewayStatus {
                    state: "error".into(),
                    email: get_kv_opt(GATEWAY_EMAIL_KEY),
                    plan: None,
                    status: None,
                    current_period_end: None,
                    message: Some(format!("アカウント情報の取得に失敗しました (HTTP {})", http_status.as_u16())),
                }
            }
        }
        Err(e) => GatewayStatus {
            state: "error".into(),
            email: get_kv_opt(GATEWAY_EMAIL_KEY),
            plan: None,
            status: None,
            current_period_end: None,
            message: Some(format!("接続失敗: {}", e)),
        },
    }
}

// ---------------------------------------------------------------------------
// Tauri コマンド
// ---------------------------------------------------------------------------

/// ワンタイムコードをメール送付する。
#[tauri::command]
pub async fn gateway_request_code(email: String) -> Result<(), String> {
    let email = email.trim().to_string();
    if !is_plausible_email(&email) {
        return Err("メールアドレスの形式が正しくありません".to_string());
    }
    // URL 未注入ビルド (OSS 配布物など) では、空 URL への接続失敗という
    // 分かりにくいエラーにせず、原因を明示して止める。
    if !is_gateway_configured() {
        log::warn!("[gateway-auth] gateway base URL is not configured in this build");
        return Err(
            "このビルドではアカウント機能を利用できません (ゲートウェイ未設定)".to_string(),
        );
    }
    let client = http_client()?;
    let url = format!("{}/auth/request-code", gateway_base_url());
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "email": email }))
        .send()
        .await
        .map_err(|e| format!("接続失敗: {}", e))?;
    let status = resp.status();
    if status.is_success() {
        log::info!("[gateway-auth] request-code sent (HTTP {})", status.as_u16());
        Ok(())
    } else if status.as_u16() == 429 {
        Err("送信回数の上限に達しました。しばらく待ってからやり直してください。".to_string())
    } else {
        log::warn!("[gateway-auth] request-code failed: HTTP {}", status.as_u16());
        Err(format!("コード送信に失敗しました (HTTP {})", status.as_u16()))
    }
}

/// コードを検証して JWT を取得・DPAPI 保存し、転写経路を gateway に自動切替する。
/// 成功時は最新の GatewayStatus を返し `gateway:auth-changed` を emit する。
#[tauri::command]
pub async fn gateway_verify_code(
    app: AppHandle,
    email: String,
    code: String,
) -> Result<GatewayStatus, String> {
    let email = email.trim().to_string();
    let code = code.trim().to_string();
    if !is_plausible_email(&email) {
        return Err("メールアドレスの形式が正しくありません".to_string());
    }
    if code.is_empty() {
        return Err("6 桁コードを入力してください".to_string());
    }

    let client = http_client()?;
    let url = format!("{}/auth/verify", gateway_base_url());
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "email": email, "code": code }))
        .send()
        .await
        .map_err(|e| format!("接続失敗: {}", e))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        log::warn!("[gateway-auth] verify failed: HTTP {}", status.as_u16());
        return Err(verify_error_message(status.as_u16(), &body));
    }

    let token = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(|s| s.to_string()))
        .ok_or_else(|| "トークンの取得に失敗しました (レスポンス形式不正)".to_string())?;

    // JWT を DPAPI 保存 (BYOK キー温存) + 経路自動切替 + メール記録
    store_jwt(Some(&token))?;
    set_kv_best_effort(TRANSCRIBE_ROUTE_MODE_KEY, ROUTE_MODE_GATEWAY);
    set_kv_best_effort(GATEWAY_EMAIL_KEY, &email);
    log::info!("[gateway-auth] login OK; transcribe route switched to gateway");

    // 保存直後に /v1/me で plan/期限を取得 (失敗しても login 自体は成立)
    let mut st = fetch_status_with_jwt(&token).await;
    if st.email.is_none() {
        st.email = Some(email);
    }
    emit_auth_changed(&app, &st);
    Ok(st)
}

/// 保存済 JWT で /v1/me を叩き、現在の認証状態を返す。JWT 未保存なら logged_out。
#[tauri::command]
pub async fn gateway_status() -> Result<GatewayStatus, String> {
    match load_saved_jwt() {
        Some(jwt) => Ok(fetch_status_with_jwt(&jwt).await),
        None => Ok(GatewayStatus::logged_out()),
    }
}

/// 上位プラン限定機能 (議事録 / セミナー字幕) を使えるかを即時に返す。
///
/// ネットワークを待たずに config_db の写しで判定する (frontend は別途
/// `gateway_status` を裏で呼んで写しを更新する)。
///
/// ログイン済でプランの写しが無い場合 (初回起動・取得失敗) は **許可側に倒す**。
/// 課金の正当性はゲートウェイ側で締めているので、ここで閉じると
/// 「圏外だから有料機能が使えない」という理不尽だけが残るため。
#[tauri::command]
pub fn gateway_pro_feature_access() -> Result<ProFeatureAccess, String> {
    let logged_in = load_saved_jwt().is_some();
    // 未ログイン (BYOK) なら plan の概念がないので None として扱う
    let plan = if logged_in {
        get_kv_opt(GATEWAY_PLAN_KEY)
    } else {
        None
    };
    Ok(ProFeatureAccess {
        allowed: plan_allows_pro_features(plan.as_deref()),
        plan,
        logged_in,
    })
}

/// 購入 (Stripe Checkout) を既定ブラウザで開き、開いた URL を返す。
///
/// 設計 SSOT: 内部設計メモ §A-3
/// 経路: 保存済 JWT → `POST /v1/billing/checkout {plan}` → `{"url":...}` → 既定ブラウザ。
///
/// Checkout URL は Stripe が発行する一時 URL なので frontend へ返してよい
/// (ブラウザが開かなかった場合の手動コピー用)。**JWT は返さない**。
#[tauri::command]
pub async fn gateway_open_checkout(app: AppHandle, plan: String) -> Result<String, String> {
    let plan = plan.trim().to_string();
    if !is_purchasable_plan(&plan) {
        return Err("不明なプランです".to_string());
    }
    if !is_gateway_configured() {
        return Err("このビルドではアカウント機能を利用できません (ゲートウェイ未設定)".to_string());
    }
    let jwt = load_saved_jwt()
        .ok_or_else(|| "先に RushVoice アカウントへログインしてください".to_string())?;

    let client = http_client()?;
    let url = format!("{}/v1/billing/checkout", gateway_base_url());
    let resp = client
        .post(&url)
        .bearer_auth(&jwt)
        .json(&serde_json::json!({ "plan": plan }))
        .send()
        .await
        .map_err(|e| format!("接続失敗: {}", e))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        log::warn!("[gateway-auth] checkout failed: HTTP {}", status.as_u16());
        return Err(checkout_error_message(status.as_u16(), &body));
    }
    let checkout_url = parse_checkout_url(&body)
        .ok_or_else(|| "決済ページの URL を取得できませんでした".to_string())?;

    // 既定ブラウザで開く (capabilities の `opener:default` が https を許可済)
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(checkout_url.clone(), None::<&str>)
        .map_err(|e| format!("ブラウザを開けませんでした: {}", e))?;
    log::info!("[gateway-auth] checkout opened for plan={}", plan);
    Ok(checkout_url)
}

/// ログアウト: JWT を削除し、転写経路を byok に戻す。`gateway:auth-changed` を emit する。
#[tauri::command]
pub fn gateway_logout(app: AppHandle) -> Result<(), String> {
    store_jwt(None)?;
    set_kv_best_effort(TRANSCRIBE_ROUTE_MODE_KEY, ROUTE_MODE_BYOK);
    set_kv_best_effort(GATEWAY_EMAIL_KEY, "");
    set_kv_best_effort(GATEWAY_PLAN_KEY, "");
    log::info!("[gateway-auth] logout OK; transcribe route switched to byok");
    emit_auth_changed(&app, &GatewayStatus::logged_out());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_error_message_maps_known_cases() {
        assert!(verify_error_message(401, r#"{"error":"invalid_code"}"#)
            .contains("コードが正しくありません"));
        assert!(verify_error_message(401, r#"{"error":"code_expired"}"#)
            .contains("有効期限"));
        assert!(verify_error_message(429, r#"{"error":"too_many_attempts"}"#)
            .contains("上限"));
        assert!(verify_error_message(500, "").contains("HTTP 500"));
    }

    #[test]
    fn parse_me_response_extracts_fields() {
        let body = r#"{
            "email": "user@example.com",
            "entitlement": {
                "plan": "trial",
                "status": "active",
                "current_period_end": "2026-08-01T00:00:00Z"
            }
        }"#;
        let me = parse_me_response(body).expect("Some");
        assert_eq!(me.email, "user@example.com");
        assert_eq!(me.plan.as_deref(), Some("trial"));
        assert_eq!(me.status.as_deref(), Some("active"));
        assert_eq!(me.current_period_end.as_deref(), Some("2026-08-01T00:00:00Z"));
    }

    #[test]
    fn parse_me_refresh_token_reads_optional_token() {
        assert_eq!(
            parse_me_refresh_token(r#"{"email":"a@b","token":"x.y.z"}"#).as_deref(),
            Some("x.y.z")
        );
        assert_eq!(parse_me_refresh_token(r#"{"email":"a@b"}"#), None);
    }

    #[test]
    fn parse_me_response_tolerates_missing_entitlement() {
        let body = r#"{"email": "user@example.com"}"#;
        let me = parse_me_response(body).expect("Some");
        assert_eq!(me.email, "user@example.com");
        assert_eq!(me.plan, None);
        assert_eq!(me.status, None);
        assert_eq!(me.current_period_end, None);
    }

    #[test]
    fn parse_me_response_rejects_garbage() {
        assert!(parse_me_response("not json").is_none());
        assert!(parse_me_response(r#"{"no_email": true}"#).is_none());
    }

    #[test]
    fn is_plausible_email_basic_cases() {
        assert!(is_plausible_email("user@example.com"));
        assert!(is_plausible_email("  user@example.co.jp  "));
        assert!(!is_plausible_email(""));
        assert!(!is_plausible_email("no-at-sign"));
        assert!(!is_plausible_email("@example.com"));
        assert!(!is_plausible_email("user@"));
        assert!(!is_plausible_email("user@nodot"));
    }

    #[test]
    fn is_purchasable_plan_accepts_only_known_plans() {
        assert!(is_purchasable_plan("standard"));
        assert!(is_purchasable_plan("pro"));
        // trial は購入対象ではない (ログイン時に自動付与される)
        assert!(!is_purchasable_plan("trial"));
        assert!(!is_purchasable_plan(""));
        assert!(!is_purchasable_plan("Standard"));
    }

    #[test]
    fn plan_allows_pro_features_matrix() {
        // 未ログイン (BYOK) は従来どおり全機能
        assert!(plan_allows_pro_features(None));
        assert!(plan_allows_pro_features(Some("pro")));
        // 14 日間フル機能トライアル (2026-07-27 決定)
        assert!(plan_allows_pro_features(Some("trial")));
        assert!(!plan_allows_pro_features(Some("standard")));
        // 未知の値は安全側 (= 上位機能は出さない)
        assert!(!plan_allows_pro_features(Some("free")));
        assert!(!plan_allows_pro_features(Some("")));
    }

    #[test]
    fn parse_checkout_url_requires_https() {
        assert_eq!(
            parse_checkout_url(r#"{"url":"https://checkout.stripe.com/c/pay/cs_test_123"}"#)
                .as_deref(),
            Some("https://checkout.stripe.com/c/pay/cs_test_123")
        );
        // http / 別スキームは決済ページとして採用しない
        assert!(parse_checkout_url(r#"{"url":"http://example.com"}"#).is_none());
        assert!(parse_checkout_url(r#"{"url":"file:///C:/windows"}"#).is_none());
        assert!(parse_checkout_url(r#"{"error":"stripe_error"}"#).is_none());
        assert!(parse_checkout_url("not json").is_none());
    }

    #[test]
    fn checkout_error_message_maps_known_cases() {
        assert!(checkout_error_message(403, r#"{"error":"invalid_token"}"#).contains("再度ログイン"));
        assert!(checkout_error_message(400, r#"{"error":"invalid_plan"}"#).contains("プラン"));
        assert!(checkout_error_message(502, r#"{"error":"stripe_error"}"#).contains("決済サービス"));
        assert!(checkout_error_message(500, "").contains("HTTP 500"));
    }

    #[test]
    fn gateway_base_url_default_has_no_trailing_slash() {
        // 環境変数未設定時のデフォルト値 (テスト環境で RUSHVOICE_GATEWAY_URL が
        // セットされている場合はそちらを尊重するため、末尾スラッシュのみ検証)
        let url = gateway_base_url();
        // 接続先を注入していないビルド (ソースから自前でビルドした場合) では空になる
        if url.is_empty() {
            assert!(!is_gateway_configured());
            return;
        }
        assert!(!url.ends_with('/'), "got: {}", url);
        assert!(url.starts_with("http"), "got: {}", url);
    }
}

#[cfg(test)]
mod build_injection_tests {
    use super::*;

    /// ビルド時注入の整合性 (2026-07-27・内部URLをソースから外した際の回帰)。
    /// 注入あり → 末尾スラッシュなしで configured / 注入なし → not configured。
    #[test]
    fn gateway_base_url_matches_build_injection() {
        match DEFAULT_GATEWAY_BASE_URL {
            Some(u) => {
                assert!(!u.is_empty(), "空文字を注入しないこと");
                assert!(
                    is_gateway_configured(),
                    "注入があれば configured と判定されること"
                );
                assert!(
                    !gateway_base_url().ends_with('/'),
                    "末尾スラッシュを落とすこと"
                );
            }
            None => assert!(
                !is_gateway_configured(),
                "未注入なら未設定として扱い BYOK に縮退すること"
            ),
        }
    }
}
