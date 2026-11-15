// endpoints.rs — 接続先 (文字起こし・整文/翻訳) の解決
//
// 2026-11-15: 有料版のゲートウェイ (当社のキーで中継) を終了し、利用者自身のキーか
// 利用者が用意した OpenAI 互換サーバー (ローカル LLM を含む) へ直接つなぐ形にした。
//
// 接続先は 2 系統それぞれに選べる:
//   - 文字起こし (`/audio/transcriptions`): settings_kv "provider"
//       hedged (Groq→遅いとき OpenAI) / groq / openai / custom
//   - 整文・字幕翻訳・議事録整形 (`/chat/completions`): settings_kv "llm_provider"
//       groq / openai / custom
//
// custom は OpenAI 互換の任意のサーバー。ベース URL は OpenAI SDK の base_url と同じく
// `/v1` まで (例: Ollama なら http://localhost:11434/v1)。末尾にパスを付けて呼ぶ。
// キーは任意 (空なら Authorization を送らない)。http://localhost も可。
//
// 解決は純粋関数 (`resolve_*`) と、設定・鍵を読む薄い関数 (`load_*`) に分けてテストする。
// キー・URL の値はログに出さない (URL はホスト部だけ)。

use crate::crypto::SecretsBundle;
use crate::recorder::ProviderChoice;

pub const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

pub const TRANSCRIPTIONS_PATH: &str = "/audio/transcriptions";
pub const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
pub const MODELS_PATH: &str = "/models";

/// Groq の文字起こしモデル
pub const GROQ_TRANSCRIBE_MODEL: &str = "whisper-large-v3-turbo";
/// OpenAI の文字起こしモデル (通常の録音・速さ重視)
pub const OPENAI_TRANSCRIBE_MODEL: &str = "gpt-4o-mini-transcribe";
/// OpenAI の文字起こしモデル (議事録・字幕: 区間ごとの時刻と言語が要るため verbose_json 対応の whisper-1)
pub const OPENAI_SEGMENT_TRANSCRIBE_MODEL: &str = "whisper-1";
/// カスタム接続先でモデル名を空にしたときの既定 (whisper.cpp server 等はモデル名を見ない)
pub const CUSTOM_TRANSCRIBE_DEFAULT_MODEL: &str = "whisper-1";

/// Groq の整文モデル。2026-11-16: llama-3.3-70b-versatile / llama-3.1-8b-instant が
/// Groq で使えなくなったため gpt-oss へ (設定の「モデル名」で上書きできる)。
pub const GROQ_CHAT_MODEL: &str = "openai/gpt-oss-120b";
/// Groq の日次枠が尽きたときの退避先 (小型)
pub const GROQ_CHAT_FALLBACK_MODEL: &str = "openai/gpt-oss-20b";
/// OpenAI の整文モデル
pub const OPENAI_CHAT_MODEL: &str = "gpt-4o-mini";

// ----- settings_kv のキー -----
pub const TRANSCRIBE_PROVIDER_KEY: &str = "provider";
pub const CUSTOM_TRANSCRIBE_BASE_URL_KEY: &str = "custom_transcribe_base_url";
pub const CUSTOM_TRANSCRIBE_MODEL_KEY: &str = "custom_transcribe_model";
pub const LLM_PROVIDER_KEY: &str = "llm_provider";
pub const CUSTOM_LLM_BASE_URL_KEY: &str = "custom_llm_base_url";
pub const CUSTOM_LLM_MODEL_KEY: &str = "custom_llm_model";
/// Groq / OpenAI の整文モデル名の上書き (空なら既定)。提供元がモデルを廃止しても設定で直せるように。
pub const GROQ_LLM_MODEL_KEY: &str = "groq_llm_model";
pub const OPENAI_LLM_MODEL_KEY: &str = "openai_llm_model";

/// 考える段階を持つモデル (gpt-oss) は、出力上限に考えた分も数えられる。
/// 考える量を最小にし、上限に余白を足さないと本文が切れて原文に戻ってしまう。
pub const REASONING_EFFORT: &str = "low";
pub const REASONING_TOKEN_HEADROOM: u32 = 1024;

/// gpt-oss 系なら reasoning_effort を付ける (他のモデルに付けると 400 になる接続先がある)。
pub fn reasoning_effort_for(model: &str) -> Option<&'static str> {
    if model.contains("gpt-oss") {
        Some(REASONING_EFFORT)
    } else {
        None
    }
}

/// 出力上限に、考える段階の分の余白を足す。
pub fn with_reasoning_headroom(model: &str, max_tokens: u32) -> u32 {
    if reasoning_effort_for(model).is_some() {
        max_tokens + REASONING_TOKEN_HEADROOM
    } else {
        max_tokens
    }
}

/// Groq / OpenAI のモデル名を設定で上書きする (カスタムは元からモデル名を持つので触らない)。
pub fn apply_model_override(mut ep: ChatEndpoint, model_override: &str) -> ChatEndpoint {
    let m = model_override.trim();
    if ep.label != "custom" && !m.is_empty() {
        ep.model = m.to_string();
    }
    ep
}

/// カスタム接続先の設定 (URL・モデル名)。キーは SecretsBundle 側。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CustomEndpointConfig {
    pub base_url: String,
    pub model: String,
}

/// ベース URL にパスを付ける。末尾の `/` は除く。利用者が既にパスまで書いていたらそのまま使う。
pub fn endpoint_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with(path) {
        base.to_string()
    } else {
        format!("{}{}", base, path)
    }
}

/// ベース URL として受け付けられるか (http/https で始まり、ホストがある)。
pub fn validate_base_url(base_url: &str) -> Result<(), String> {
    let s = base_url.trim();
    if s.is_empty() {
        return Err("接続先の URL が未設定です".into());
    }
    let rest = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
        .ok_or_else(|| "URL は http:// か https:// で始めてください".to_string())?;
    if rest.trim_start_matches('/').is_empty() {
        return Err("URL にホスト名がありません".into());
    }
    Ok(())
}

/// ログ用: URL のスキームとホスト (と port) だけ返す。パス・クエリは出さない。
pub fn host_for_log(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        None => return "(invalid url)".into(),
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    // user:pass@host の形なら資格情報を落とす
    let host = host.rsplit('@').next().unwrap_or(host);
    format!("{}://{}", scheme, host)
}

/// キーが空なら Authorization を付けない (ローカルサーバーはキー不要が多い)。
pub fn with_auth(rb: reqwest::RequestBuilder, api_key: &str) -> reqwest::RequestBuilder {
    if api_key.trim().is_empty() {
        rb
    } else {
        rb.bearer_auth(api_key)
    }
}

fn non_empty(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

// ===========================================================================
// 文字起こし
// ===========================================================================

/// 1 つの文字起こし接続先。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscribeEndpoint {
    /// 履歴に残す接続先の名前 ("groq" / "openai" / "custom")
    pub label: &'static str,
    pub url: String,
    /// 空ならキーなし
    pub api_key: String,
    pub model: String,
    /// response_format=verbose_json を要求するか (区間・logprob を使う)
    pub verbose_json: bool,
}

/// 通常の録音で使う接続先。fallback が Some なら hedged (primary が遅いとき並走)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscribeSetup {
    pub primary: TranscribeEndpoint,
    pub fallback: Option<TranscribeEndpoint>,
}

impl TranscribeSetup {
    pub fn is_hedged(&self) -> bool {
        self.fallback.is_some()
    }

    /// 勝者の名前からモデル名を引く (履歴用)。
    pub fn model_for(&self, label: &str) -> String {
        match &self.fallback {
            Some(f) if f.label == label => f.model.clone(),
            _ => self.primary.model.clone(),
        }
    }
}

pub const NO_KEY_MESSAGE: &str =
    "文字起こしの接続先が未設定です。設定の「接続先とAPIキー」で Groq か OpenAI のキー、またはカスタム接続先を設定してください";

fn groq_transcribe(key: String) -> TranscribeEndpoint {
    TranscribeEndpoint {
        label: "groq",
        url: endpoint_url(GROQ_BASE_URL, TRANSCRIPTIONS_PATH),
        api_key: key,
        model: GROQ_TRANSCRIBE_MODEL.into(),
        // Groq は verbose_json で no_speech_prob / avg_logprob を返す (幻聴判定に使う)
        verbose_json: true,
    }
}

fn openai_transcribe(key: String) -> TranscribeEndpoint {
    TranscribeEndpoint {
        label: "openai",
        url: endpoint_url(OPENAI_BASE_URL, TRANSCRIPTIONS_PATH),
        api_key: key,
        model: OPENAI_TRANSCRIBE_MODEL.into(),
        // gpt-4o-mini-transcribe は json のみ
        verbose_json: false,
    }
}

fn custom_transcribe(
    custom: &CustomEndpointConfig,
    key: Option<String>,
    verbose_json: bool,
) -> Result<TranscribeEndpoint, String> {
    validate_base_url(&custom.base_url)
        .map_err(|e| format!("文字起こしのカスタム接続先: {}", e))?;
    let model = custom.model.trim();
    Ok(TranscribeEndpoint {
        label: "custom",
        url: endpoint_url(&custom.base_url, TRANSCRIPTIONS_PATH),
        // キーは任意 (無ければ空 = Authorization を送らない)
        api_key: key.unwrap_or_default(),
        model: if model.is_empty() {
            CUSTOM_TRANSCRIBE_DEFAULT_MODEL.into()
        } else {
            model.into()
        },
        verbose_json,
    })
}

/// 通常の録音の接続先を決める (純粋関数)。
///
/// - custom: カスタム接続先だけ。互換性を優先し response_format=json で受ける
///   (verbose_json を返さないサーバーがあるため。幻聴対策は文言一致の層が効く)。
/// - hedged: Groq と OpenAI の両方のキーがあれば並走、片方だけならそれ単独。
/// - groq / openai: そのキーで単独。openai のキーが無く Groq があれば Groq に縮退 (従来どおり)。
pub fn resolve_transcribe_setup(
    choice: ProviderChoice,
    keys: &SecretsBundle,
    custom: &CustomEndpointConfig,
) -> Result<TranscribeSetup, String> {
    let groq = non_empty(&keys.groq_api_key);
    let openai = non_empty(&keys.openai_api_key);
    match choice {
        ProviderChoice::Custom => Ok(TranscribeSetup {
            primary: custom_transcribe(custom, non_empty(&keys.custom_transcribe_api_key), false)?,
            fallback: None,
        }),
        ProviderChoice::Hedged => match (groq, openai) {
            (Some(g), Some(o)) => Ok(TranscribeSetup {
                primary: groq_transcribe(g),
                fallback: Some(openai_transcribe(o)),
            }),
            (Some(g), None) => Ok(TranscribeSetup {
                primary: groq_transcribe(g),
                fallback: None,
            }),
            (None, Some(o)) => Ok(TranscribeSetup {
                primary: openai_transcribe(o),
                fallback: None,
            }),
            (None, None) => Err(NO_KEY_MESSAGE.into()),
        },
        ProviderChoice::Groq => groq
            .map(|g| TranscribeSetup {
                primary: groq_transcribe(g),
                fallback: None,
            })
            .ok_or_else(|| "Groq の API キーが未設定です (設定画面で保存してください)".into()),
        ProviderChoice::Openai => match (openai, groq) {
            (Some(o), _) => Ok(TranscribeSetup {
                primary: openai_transcribe(o),
                fallback: None,
            }),
            (None, Some(g)) => {
                log::warn!("[endpoints] provider=openai but OpenAI key absent → Groq");
                Ok(TranscribeSetup {
                    primary: groq_transcribe(g),
                    fallback: None,
                })
            }
            (None, None) => Err("OpenAI の API キーが未設定です (設定画面で保存してください)".into()),
        },
    }
}

/// 議事録・字幕の接続先 (純粋関数)。区間の時刻・言語が要るので verbose_json 必須。
///
/// - custom: カスタム接続先 (verbose_json を返せるサーバーが必要: whisper.cpp server・
///   faster-whisper 系の OpenAI 互換サーバーは対応)
/// - それ以外: Groq のキーがあれば Groq、無ければ OpenAI (whisper-1)
pub fn resolve_segment_transcribe_endpoint(
    choice: ProviderChoice,
    keys: &SecretsBundle,
    custom: &CustomEndpointConfig,
) -> Result<TranscribeEndpoint, String> {
    if choice == ProviderChoice::Custom {
        return custom_transcribe(custom, non_empty(&keys.custom_transcribe_api_key), true);
    }
    if let Some(g) = non_empty(&keys.groq_api_key) {
        return Ok(groq_transcribe(g));
    }
    if let Some(o) = non_empty(&keys.openai_api_key) {
        return Ok(TranscribeEndpoint {
            label: "openai",
            url: endpoint_url(OPENAI_BASE_URL, TRANSCRIPTIONS_PATH),
            api_key: o,
            model: OPENAI_SEGMENT_TRANSCRIBE_MODEL.into(),
            verbose_json: true,
        });
    }
    Err(NO_KEY_MESSAGE.into())
}

// ===========================================================================
// 整文・翻訳 (chat/completions)
// ===========================================================================

/// 整文・翻訳の接続先の種類 (settings_kv "llm_provider")。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProvider {
    Groq,
    Openai,
    Custom,
}

impl LlmProvider {
    /// 未設定・未知の値は Groq (従来の既定)。
    pub fn parse(s: &str) -> Self {
        match s {
            "openai" => Self::Openai,
            "custom" => Self::Custom,
            _ => Self::Groq,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatEndpoint {
    pub label: &'static str,
    pub url: String,
    /// 空ならキーなし
    pub api_key: String,
    pub model: String,
    /// 429 で待っても戻らない (日次枠の枯渇) ときに切り替えるモデル。Groq だけ。
    pub fallback_model: Option<String>,
}

/// 整文・翻訳の接続先を決める (純粋関数)。
pub fn resolve_chat_endpoint(
    provider: LlmProvider,
    keys: &SecretsBundle,
    custom: &CustomEndpointConfig,
) -> Result<ChatEndpoint, String> {
    match provider {
        LlmProvider::Groq => non_empty(&keys.groq_api_key)
            .map(|k| ChatEndpoint {
                label: "groq",
                url: endpoint_url(GROQ_BASE_URL, CHAT_COMPLETIONS_PATH),
                api_key: k,
                model: GROQ_CHAT_MODEL.into(),
                fallback_model: Some(GROQ_CHAT_FALLBACK_MODEL.into()),
            })
            .ok_or_else(|| "整文の接続先 (Groq) の API キーが未設定です".into()),
        LlmProvider::Openai => non_empty(&keys.openai_api_key)
            .map(|k| ChatEndpoint {
                label: "openai",
                url: endpoint_url(OPENAI_BASE_URL, CHAT_COMPLETIONS_PATH),
                api_key: k,
                model: OPENAI_CHAT_MODEL.into(),
                fallback_model: None,
            })
            .ok_or_else(|| "整文の接続先 (OpenAI) の API キーが未設定です".into()),
        LlmProvider::Custom => {
            validate_base_url(&custom.base_url)
                .map_err(|e| format!("整文のカスタム接続先: {}", e))?;
            let model = custom.model.trim();
            if model.is_empty() {
                return Err("整文のカスタム接続先: モデル名が未設定です (例: Ollama なら qwen2.5:7b)".into());
            }
            let key = non_empty(&keys.custom_llm_api_key).unwrap_or_default();
            Ok(ChatEndpoint {
                label: "custom",
                url: endpoint_url(&custom.base_url, CHAT_COMPLETIONS_PATH),
                api_key: key,
                model: model.into(),
                fallback_model: None,
            })
        }
    }
}

// ===========================================================================
// 設定・鍵を読む薄い関数
// ===========================================================================

fn load_keys() -> SecretsBundle {
    match crate::crypto::load_secrets(&crate::crypto::default_secrets_path()) {
        Ok(Some(b)) => b,
        Ok(None) => SecretsBundle::default(),
        Err(e) => {
            log::warn!("[endpoints] load_secrets failed: {}; treating as no keys", e);
            SecretsBundle::default()
        }
    }
}

fn kv(db: Option<&crate::config_db::ConfigDb>, key: &str) -> String {
    db.and_then(|d| d.get_kv(key).ok().flatten())
        .unwrap_or_default()
}

fn open_config() -> Option<crate::config_db::ConfigDb> {
    crate::config_db::ConfigDb::open(&crate::config_db::default_config_db_path())
        .map_err(|e| log::warn!("[endpoints] open config_db failed: {}", e))
        .ok()
}

fn load_transcribe_inputs() -> (ProviderChoice, SecretsBundle, CustomEndpointConfig) {
    let db = open_config();
    let choice = ProviderChoice::parse(&kv(db.as_ref(), TRANSCRIBE_PROVIDER_KEY));
    let custom = CustomEndpointConfig {
        base_url: kv(db.as_ref(), CUSTOM_TRANSCRIBE_BASE_URL_KEY),
        model: kv(db.as_ref(), CUSTOM_TRANSCRIBE_MODEL_KEY),
    };
    (choice, load_keys(), custom)
}

/// 通常の録音の接続先を、保存済みの設定と鍵から決める。
pub fn load_transcribe_setup() -> Result<TranscribeSetup, String> {
    let (choice, keys, custom) = load_transcribe_inputs();
    resolve_transcribe_setup(choice, &keys, &custom)
}

/// 議事録・字幕の文字起こし接続先を、保存済みの設定と鍵から決める。
pub fn load_segment_transcribe_endpoint() -> Result<TranscribeEndpoint, String> {
    let (choice, keys, custom) = load_transcribe_inputs();
    resolve_segment_transcribe_endpoint(choice, &keys, &custom)
}

/// 整文・翻訳の接続先を、保存済みの設定と鍵から決める。
pub fn load_chat_endpoint() -> Result<ChatEndpoint, String> {
    let db = open_config();
    let provider = LlmProvider::parse(&kv(db.as_ref(), LLM_PROVIDER_KEY));
    let custom = CustomEndpointConfig {
        base_url: kv(db.as_ref(), CUSTOM_LLM_BASE_URL_KEY),
        model: kv(db.as_ref(), CUSTOM_LLM_MODEL_KEY),
    };
    let ep = resolve_chat_endpoint(provider, &load_keys(), &custom)?;
    let override_key = match ep.label {
        "groq" => GROQ_LLM_MODEL_KEY,
        "openai" => OPENAI_LLM_MODEL_KEY,
        _ => return Ok(ep),
    };
    Ok(apply_model_override(ep, &kv(db.as_ref(), override_key)))
}

// ===========================================================================
// 旧版の有料アカウント (ゲートウェイ) からの移行
// ===========================================================================

/// 移行の案内を出すかどうかの印 (settings_kv)。"1" = 出す。利用者が閉じたら "0"。
pub const GATEWAY_RETIRED_NOTICE_KEY: &str = "gateway_retired_notice";
/// 旧版が使っていた設定キー (転写経路・メール・プラン)。移行時に空にする。
const LEGACY_GATEWAY_KV_KEYS: [&str; 3] = ["transcribe_route_mode", "gateway_email", "gateway_plan"];

/// 起動時に 1 回呼ぶ。旧版のゲートウェイにログインしていた利用者のログイン情報を捨て、
/// 「自分のキーを設定してください」の案内を出す印を立てる。冪等。
pub fn retire_gateway_login() {
    let path = crate::crypto::default_secrets_path();
    let mut had_login = false;
    match crate::crypto::load_secrets(&path) {
        Ok(Some(bundle)) if bundle.had_legacy_gateway_login => {
            had_login = true;
            // 保存し直すと旧ログイン情報は書き出されない (= 消える)。他のキーは残る。
            if let Err(e) = crate::crypto::save_secrets(&path, &bundle) {
                log::warn!("[gateway-retire] failed to drop legacy login: {}", e);
            }
        }
        Ok(_) => {}
        Err(e) => log::warn!("[gateway-retire] load_secrets failed: {}", e),
    }

    let Some(db) = open_config() else { return };
    if kv(Some(&db), "transcribe_route_mode") == "gateway" {
        had_login = true;
    }
    for key in LEGACY_GATEWAY_KV_KEYS {
        if !kv(Some(&db), key).is_empty() {
            let _ = db.set_kv(key, "");
        }
    }
    if had_login {
        log::info!("[gateway-retire] legacy gateway login removed; showing BYOK notice");
        let _ = db.set_kv(GATEWAY_RETIRED_NOTICE_KEY, "1");
    }
}

// ===========================================================================
// 接続確認
// ===========================================================================

#[derive(Debug, serde::Serialize)]
pub struct EndpointTestResult {
    pub ok: bool,
    pub status: Option<u16>,
    pub message: String,
}

/// 接続確認の結果を文言にする (純粋関数)。
/// `/models` を持たないサーバー (whisper.cpp server 等) もあるので 404 は「つながった」扱い。
pub fn describe_models_status(status: u16) -> (bool, String) {
    match status {
        200..=299 => (true, "OK".into()),
        401 | 403 => (false, format!("HTTP {}: キーが違うか、権限がありません", status)),
        404 | 405 => (
            true,
            format!("つながりました (HTTP {}: このサーバーにはモデル一覧がありません)", status),
        ),
        _ => (false, format!("HTTP {}", status)),
    }
}

/// `{base}/models` に軽く GET して、つながるか・キーが通るかを確かめる (5 秒で打ち切り)。
pub async fn test_endpoint(base_url: &str, api_key: &str) -> EndpointTestResult {
    if let Err(e) = validate_base_url(base_url) {
        return EndpointTestResult {
            ok: false,
            status: None,
            message: e,
        };
    }
    let url = endpoint_url(base_url, MODELS_PATH);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return EndpointTestResult {
                ok: false,
                status: None,
                message: e.to_string(),
            }
        }
    };
    match with_auth(client.get(&url), api_key).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let (ok, message) = describe_models_status(status);
            EndpointTestResult {
                ok,
                status: Some(status),
                message,
            }
        }
        Err(e) => EndpointTestResult {
            ok: false,
            status: None,
            message: format!("接続できません ({}): サーバーが起動しているか、URL を確認してください", host_for_log(&url)),
        }
        .with_log(&e),
    }
}

impl EndpointTestResult {
    fn with_log(self, e: &reqwest::Error) -> Self {
        log::warn!("[endpoints] test_endpoint failed: {}", e);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(groq: Option<&str>, openai: Option<&str>) -> SecretsBundle {
        SecretsBundle {
            groq_api_key: groq.map(String::from),
            openai_api_key: openai.map(String::from),
            ..Default::default()
        }
    }

    fn custom(base: &str, model: &str) -> CustomEndpointConfig {
        CustomEndpointConfig {
            base_url: base.into(),
            model: model.into(),
        }
    }

    #[test]
    fn endpoint_url_appends_path_and_trims_slash() {
        assert_eq!(
            endpoint_url("http://localhost:11434/v1/", CHAT_COMPLETIONS_PATH),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url(" http://127.0.0.1:8080/v1 ", TRANSCRIPTIONS_PATH),
            "http://127.0.0.1:8080/v1/audio/transcriptions"
        );
        // 利用者がパスまで書いた場合は二重に付けない
        assert_eq!(
            endpoint_url("http://localhost:1234/v1/chat/completions", CHAT_COMPLETIONS_PATH),
            "http://localhost:1234/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url(GROQ_BASE_URL, TRANSCRIPTIONS_PATH),
            "https://api.groq.com/openai/v1/audio/transcriptions"
        );
    }

    #[test]
    fn validate_base_url_cases() {
        assert!(validate_base_url("http://localhost:11434/v1").is_ok());
        assert!(validate_base_url("https://example.com/v1").is_ok());
        assert!(validate_base_url("").is_err());
        assert!(validate_base_url("localhost:11434").is_err());
        assert!(validate_base_url("ftp://x").is_err());
        assert!(validate_base_url("http://").is_err());
    }

    #[test]
    fn host_for_log_hides_path_and_credentials() {
        assert_eq!(
            host_for_log("http://user:pw@localhost:8080/v1/models?k=1"),
            "http://localhost:8080"
        );
        assert_eq!(host_for_log("nonsense"), "(invalid url)");
    }

    #[test]
    fn hedged_uses_both_keys_when_present() {
        let s = resolve_transcribe_setup(
            ProviderChoice::Hedged,
            &keys(Some("g"), Some("o")),
            &CustomEndpointConfig::default(),
        )
        .unwrap();
        assert_eq!(s.primary.label, "groq");
        assert!(s.primary.verbose_json);
        assert_eq!(s.fallback.as_ref().unwrap().label, "openai");
        assert!(s.is_hedged());
        assert_eq!(s.model_for("openai"), OPENAI_TRANSCRIBE_MODEL);
        assert_eq!(s.model_for("groq"), GROQ_TRANSCRIBE_MODEL);
    }

    #[test]
    fn hedged_degrades_to_whichever_key_exists() {
        let only_groq =
            resolve_transcribe_setup(ProviderChoice::Hedged, &keys(Some("g"), None), &Default::default())
                .unwrap();
        assert_eq!(only_groq.primary.label, "groq");
        assert!(!only_groq.is_hedged());
        // 以前は Groq のキーが必須だった。OpenAI だけでも使えるようにした
        let only_openai =
            resolve_transcribe_setup(ProviderChoice::Hedged, &keys(None, Some("o")), &Default::default())
                .unwrap();
        assert_eq!(only_openai.primary.label, "openai");
        assert!(
            resolve_transcribe_setup(ProviderChoice::Hedged, &keys(None, None), &Default::default())
                .is_err()
        );
    }

    #[test]
    fn openai_choice_falls_back_to_groq_like_before() {
        let s = resolve_transcribe_setup(ProviderChoice::Openai, &keys(Some("g"), None), &Default::default())
            .unwrap();
        assert_eq!(s.primary.label, "groq");
        assert!(
            resolve_transcribe_setup(ProviderChoice::Groq, &keys(None, Some("o")), &Default::default())
                .is_err()
        );
    }

    #[test]
    fn custom_transcribe_needs_no_key_and_uses_json() {
        let s = resolve_transcribe_setup(
            ProviderChoice::Custom,
            &keys(None, None),
            &custom("http://localhost:8080/v1", ""),
        )
        .unwrap();
        assert_eq!(s.primary.label, "custom");
        assert_eq!(s.primary.url, "http://localhost:8080/v1/audio/transcriptions");
        assert_eq!(s.primary.api_key, "");
        assert_eq!(s.primary.model, CUSTOM_TRANSCRIBE_DEFAULT_MODEL);
        assert!(!s.primary.verbose_json);
        assert!(s.fallback.is_none());
    }

    #[test]
    fn custom_transcribe_requires_url() {
        let e = resolve_transcribe_setup(ProviderChoice::Custom, &keys(Some("g"), None), &custom("", ""))
            .unwrap_err();
        assert!(e.contains("URL"));
    }

    #[test]
    fn segment_endpoint_requires_verbose_json() {
        let c = resolve_segment_transcribe_endpoint(
            ProviderChoice::Custom,
            &keys(None, None),
            &custom("http://localhost:8000/v1", "Systran/faster-whisper-small"),
        )
        .unwrap();
        assert!(c.verbose_json);
        assert_eq!(c.model, "Systran/faster-whisper-small");

        let g = resolve_segment_transcribe_endpoint(ProviderChoice::Openai, &keys(Some("g"), Some("o")), &Default::default())
            .unwrap();
        assert_eq!(g.label, "groq");

        let o = resolve_segment_transcribe_endpoint(ProviderChoice::Hedged, &keys(None, Some("o")), &Default::default())
            .unwrap();
        assert_eq!(o.label, "openai");
        assert_eq!(o.model, OPENAI_SEGMENT_TRANSCRIBE_MODEL);
        assert!(o.verbose_json);
    }

    #[test]
    fn chat_endpoint_per_provider() {
        let g = resolve_chat_endpoint(LlmProvider::Groq, &keys(Some("g"), None), &Default::default()).unwrap();
        assert_eq!(g.url, "https://api.groq.com/openai/v1/chat/completions");
        assert_eq!(g.fallback_model.as_deref(), Some(GROQ_CHAT_FALLBACK_MODEL));

        // モデル名の上書き: Groq / OpenAI は効き、空なら既定のまま
        let over = apply_model_override(g.clone(), " qwen/qwen3.8-27b ");
        assert_eq!(over.model, "qwen/qwen3.8-27b");
        assert_eq!(apply_model_override(g.clone(), "  ").model, GROQ_CHAT_MODEL);
        let o = resolve_chat_endpoint(LlmProvider::Openai, &keys(None, Some("o")), &Default::default()).unwrap();
        assert_eq!(o.model, OPENAI_CHAT_MODEL);
        assert!(o.fallback_model.is_none());

        assert!(resolve_chat_endpoint(LlmProvider::Groq, &keys(None, Some("o")), &Default::default()).is_err());
    }

    #[test]
    fn chat_custom_local_llm_without_key() {
        let c = resolve_chat_endpoint(
            LlmProvider::Custom,
            &keys(None, None),
            &custom("http://localhost:11434/v1", "qwen2.5:7b"),
        )
        .unwrap();
        assert_eq!(c.url, "http://localhost:11434/v1/chat/completions");
        assert_eq!(c.api_key, "");
        assert_eq!(c.model, "qwen2.5:7b");

        let no_model = resolve_chat_endpoint(
            LlmProvider::Custom,
            &keys(None, None),
            &custom("http://localhost:11434/v1", " "),
        );
        assert!(no_model.unwrap_err().contains("モデル名"));
    }

    #[test]
    fn llm_provider_parse_defaults_to_groq() {
        assert_eq!(LlmProvider::parse(""), LlmProvider::Groq);
        assert_eq!(LlmProvider::parse("openai"), LlmProvider::Openai);
        assert_eq!(LlmProvider::parse("custom"), LlmProvider::Custom);
        assert_eq!(LlmProvider::parse("gateway"), LlmProvider::Groq);
    }

    #[test]
    fn describe_models_status_cases() {
        assert!(describe_models_status(200).0);
        assert!(!describe_models_status(401).0);
        assert!(describe_models_status(404).0, "モデル一覧が無いサーバーでも、つながっていれば OK");
        assert!(!describe_models_status(500).0);
    }

    #[test]
    fn with_auth_skips_empty_key() {
        let client = reqwest::Client::new();
        let req = with_auth(client.get("http://localhost/v1/models"), "  ").build().unwrap();
        assert!(req.headers().get("authorization").is_none());
        let req = with_auth(client.get("http://localhost/v1/models"), "k").build().unwrap();
        assert!(req.headers().get("authorization").is_some());
    }

    #[test]
    fn reasoning_params_only_for_gpt_oss() {
        assert_eq!(reasoning_effort_for("openai/gpt-oss-120b"), Some("low"));
        assert_eq!(reasoning_effort_for("gpt-oss:20b"), Some("low"));
        assert_eq!(reasoning_effort_for("gpt-4o-mini"), None);
        assert_eq!(reasoning_effort_for("qwen2.5:7b"), None);
        assert_eq!(with_reasoning_headroom("openai/gpt-oss-20b", 100), 100 + REASONING_TOKEN_HEADROOM);
        assert_eq!(with_reasoning_headroom("gpt-4o-mini", 100), 100);
        // 既定のモデルが廃止済みの llama に戻っていないこと
        assert!(!GROQ_CHAT_MODEL.contains("llama"));
        assert!(!GROQ_CHAT_FALLBACK_MODEL.contains("llama"));
    }
}
