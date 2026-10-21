//! 外部 API

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::Instrument;

use crate::audio::{self, Codec, OutputFormat};
use crate::catalog::SearchHit;
use crate::hub::{ClientCtx, Hub, HubError};
use crate::reqlog::{self, Outcome, RequestLog};

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/v1/speech", post(speech))
        .route("/v1/voices", get(voices))
        .route("/v1/voices/resolve", get(resolve))
        .route("/v1/health", get(health))
        .route("/admin/engines", get(admin_engines))
        .route("/admin/catalog/refresh", post(admin_refresh))
        .with_state(hub)
}

// ---- エラー ----

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    suggestions: Vec<String>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), suggestions: vec![] }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = json!({ "code": self.code, "message": self.message });
        if !self.suggestions.is_empty() {
            error["suggestions"] = json!(self.suggestions);
        }
        (self.status, Json(json!({ "error": error }))).into_response()
    }
}

impl From<HubError> for ApiError {
    fn from(e: HubError) -> Self {
        match e {
            HubError::BadAddress(m) => ApiError::new(StatusCode::BAD_REQUEST, "bad_address", m),
            HubError::UnknownVoice { name, suggestions } => ApiError {
                suggestions,
                ..ApiError::new(StatusCode::NOT_FOUND, "unknown_voice", format!("unknown voice: {name}"))
            },
            HubError::Unavailable(m) => ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", m),
        }
    }
}

// ---- 認証 ----

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").map(str::trim)
}

fn client(hub: &Hub, headers: &HeaderMap) -> Result<Option<ClientCtx>, ApiError> {
    hub.authenticate(bearer(headers))
        .map_err(|_| ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "a valid API key is required"))
}

fn admin(hub: &Hub, headers: &HeaderMap) -> Result<(), ApiError> {
    if hub.authorize_admin(bearer(headers)) {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "a valid admin key is required"))
    }
}

// ---- /v1/speech ----

#[derive(Deserialize)]
struct SpeechRequest {
    text: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    format: FormatRequest,
    #[serde(default)]
    fallback: FallbackMode,
    /// 受付から音声を返し始めるまでに待てる時間。過ぎそうなら代わりの声に切り替え、それでも間に合わなければエラー
    #[serde(default)]
    deadline_ms: Option<u64>,
}

/// `deadline_ms` に指定できる上限（打ち間違いで実質無期限にならないように）
const MAX_DEADLINE_MS: u64 = 600_000;

#[derive(Deserialize, Default)]
struct FormatRequest {
    codec: Option<Codec>,
    sample_rate: Option<u32>,
    channels: Option<u16>,
}

#[derive(Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FallbackMode {
    #[default]
    Auto,
    None,
}

/// HTTP ヘッダに日本語を入れるための RFC 8187 形式（attr-char 以外をパーセントエンコード）
const ATTR_CHAR: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'!')
    .remove(b'#')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b'-')
    .remove(b'.')
    .remove(b'^')
    .remove(b'_')
    .remove(b'`')
    .remove(b'|')
    .remove(b'~');

pub fn ext_value(s: &str) -> String {
    format!("UTF-8''{}", utf8_percent_encode(s, ATTR_CHAR))
}

/// 合成。受付から終了までを同じ ID で記録し、ID は `X-TTS-Request-Id` で返す。
/// 利用者が `X-Request-Id` を付けてきたら、その ID を使う（利用者側のログと突き合わせるため）
async fn speech(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    body: Result<Json<SpeechRequest>, JsonRejection>,
) -> Response {
    let started = Instant::now();
    let id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| reqlog::valid_id(v))
        .map_or_else(reqlog::new_id, str::to_owned);
    let span = tracing::info_span!("speech", id = %id);

    let mut log = None;
    let result = speech_inner(&hub, &headers, body, started, &id, &mut log).instrument(span).await;
    let mut resp = match result {
        Ok(resp) => resp,
        Err(e) => {
            if let Some(log) = log.as_mut() {
                // 本文が混ざらないよう、合成できなかったときの説明（声とエンジンのエラー）以外はコードだけ記録する
                match e.status {
                    StatusCode::SERVICE_UNAVAILABLE => log.finish(Outcome::Unavailable, Some(&e.message)),
                    _ => log.finish(Outcome::Rejected, Some(e.code)),
                }
            }
            e.into_response()
        }
    };
    if let Ok(v) = HeaderValue::from_str(&id) {
        resp.headers_mut().insert("x-tts-request-id", v);
    }
    resp
}

/// `log` には受付を記録した後のリクエストの記録を入れる。応答を返すときは、流し終えるまで記録をストリームに持たせる
async fn speech_inner(
    hub: &Hub,
    headers: &HeaderMap,
    body: Result<Json<SpeechRequest>, JsonRejection>,
    started: Instant,
    id: &str,
    slot: &mut Option<RequestLog>,
) -> Result<Response, ApiError> {
    let client = client(hub, headers)?;
    let client_name = client.as_ref().map(|c| c.name.as_str()).unwrap_or("-");
    let Json(req) = body.map_err(|e| {
        // 解析エラーの文言は本文の一部を引用することがあるため、記録しない（利用者への応答には含める）
        tracing::info!(client = client_name, "受付を拒否（JSON が正しくない）");
        ApiError::new(StatusCode::BAD_REQUEST, "bad_request", e.body_text())
    })?;

    let chars = req.text.chars().count();
    let voice = req.voice.as_deref().unwrap_or("default");
    *slot = Some(RequestLog::start(id.to_owned(), started, client_name, chars, voice));

    if req.text.trim().is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "empty_text", "text is empty"));
    }
    if chars > hub.config.max_text_chars {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "text_too_long",
            format!("text is longer than {} characters", hub.config.max_text_chars),
        ));
    }
    let out = OutputFormat {
        codec: req.format.codec.unwrap_or(Codec::PcmS16le),
        sample_rate: req.format.sample_rate.unwrap_or(48000),
        channels: req.format.channels.unwrap_or(2),
    };
    if !(8000..=192_000).contains(&out.sample_rate) || !(1..=2).contains(&out.channels) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "bad_format", "unsupported sample_rate or channels"));
    }

    let deadline = match req.deadline_ms {
        None => None,
        Some(ms @ 1..=MAX_DEADLINE_MS) => Some(started + Duration::from_millis(ms)),
        Some(_) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "bad_deadline",
                format!("deadline_ms must be between 1 and {MAX_DEADLINE_MS}"),
            ))
        }
    };

    let fallback = req.fallback == FallbackMode::Auto;
    let outcome = hub.synthesize(&req.text, voice, fallback, client.as_ref(), deadline).await?;

    tracing::info!(
        resolved = %outcome.voice.address,
        fallback = outcome.reason.as_str(),
        backend = %outcome.backend,
        response_start_ms = started.elapsed().as_millis() as u64,
        "応答開始"
    );

    let bytes_per_sec = u64::from(out.sample_rate) * u64::from(out.channels) * 2;
    let audio = audio::convert(outcome.body, out, outcome.gain);
    let tracked = slot.take().expect("受付を記録した").track(audio, bytes_per_sec);
    let mut resp = Response::new(Body::from_stream(tracked));
    let h = resp.headers_mut();
    let set = |h: &mut HeaderMap, name: &'static str, value: String| {
        if let Ok(v) = HeaderValue::from_str(&value) {
            h.insert(name, v);
        }
    };
    match out.codec {
        Codec::PcmS16le => {
            set(h, "content-type", "audio/pcm".into());
            set(h, "x-tts-pcm-sample-rate", out.sample_rate.to_string());
            set(h, "x-tts-pcm-bit-depth", "16".into());
            set(h, "x-tts-pcm-channels", out.channels.to_string());
        }
        Codec::Wav => set(h, "content-type", "audio/wav".into()),
    }
    set(h, "x-tts-resolved-voice", ext_value(&outcome.voice.address));
    set(h, "x-tts-fallback", outcome.reason.as_str().into());
    if let Some(credit) = &outcome.voice.credit {
        set(h, "x-tts-credit", ext_value(credit));
    }
    if !outcome.warnings.is_empty() {
        set(h, "x-tts-warning", outcome.warnings.join("; "));
    }
    Ok(resp)
}

// ---- /v1/voices ----

#[derive(Deserialize)]
struct VoicesQuery {
    #[serde(default)]
    q: String,
    engine: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
    /// true なら、稼働中のバックエンドがある論理エンジンの話者（と、それを指す別名・プリセット）だけを返す
    #[serde(default)]
    available: bool,
}

async fn voices(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Query(q): Query<VoicesQuery>,
) -> Result<Json<Value>, ApiError> {
    client(&hub, &headers)?;
    let limit = q.limit.unwrap_or(25).clamp(1, 200);
    let offset: usize = q.cursor.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0);
    let mut hits = hub.catalog.search(&hub.order, &q.q, q.engine.as_deref(), &hub.named_targets());
    if q.available {
        hits.retain(|h| hit_available(&hub, h));
    }
    let total = hits.len();
    let page: Vec<Value> = hits.into_iter().skip(offset).take(limit).map(|h| hit_json(&hub, h)).collect();
    let next = (offset + limit < total).then(|| (offset + limit).to_string());
    Ok(Json(json!({ "voices": page, "next_cursor": next })))
}

/// 話者はその論理エンジンが稼働中か。別名・プリセットは指す先のエンジンで判断し、エンジンが分からなければ残す
fn hit_available(hub: &Hub, hit: &SearchHit) -> bool {
    let engine = match hit {
        SearchHit::Voice(v) => Some(v.engine.as_str()),
        SearchHit::Named { target, .. } => target.split_once(':').map(|(e, _)| e),
    };
    match engine.and_then(|e| hub.engines.get(e)) {
        Some(e) => e.is_available(),
        None => !matches!(hit, SearchHit::Voice(_)),
    }
}

fn hit_json(hub: &Hub, hit: SearchHit) -> Value {
    match hit {
        SearchHit::Voice(v) => {
            let available = hub.engines.get(&v.engine).is_some_and(|e| e.is_available());
            let mut j = serde_json::to_value(&v).unwrap();
            j["kind"] = json!("voice");
            j["available"] = json!(available);
            j
        }
        SearchHit::Named { name, target } => {
            let kind = if name.starts_with("preset:") { "preset" } else { "alias" };
            // engine は指す先の話者のエンジン（指す先が見つからなければ null）
            let engine = hub.describe(&target).map(|v| v.engine);
            json!({ "address": name, "kind": kind, "engine": engine, "display_name": name, "target": target })
        }
    }
}

#[derive(Deserialize)]
struct ResolveQuery {
    q: String,
}

async fn resolve(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Query(q): Query<ResolveQuery>,
) -> Result<Json<Value>, ApiError> {
    client(&hub, &headers)?;
    // プリセットに当たったときも、その先の話者を voice に入れる（表示名とクレジットを出せるように）
    let (r, voice) = hub.resolve(&q.q);
    let mut j = serde_json::to_value(r).unwrap();
    j["voice"] = voice.map(|v| hit_json(&hub, SearchHit::Voice(v))).unwrap_or(Value::Null);
    Ok(Json(j))
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

// ---- 管理 API ----

async fn admin_engines(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    admin(&hub, &headers)?;
    let engines: Vec<Value> = hub
        .engines
        .values()
        .map(|e| {
            json!({
                "name": e.name,
                "plugin": e.config.plugin,
                "balance": format!("{:?}", e.config.balance),
                "available": e.is_available(),
                "voices": hub.catalog.engine(&e.name).map_or(0, |v| v.len()),
                "backends": e.status(),
            })
        })
        .collect();
    Ok(Json(json!({ "engines": engines })))
}

async fn admin_refresh(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    admin(&hub, &headers)?;
    let mut result = serde_json::Map::new();
    for name in &hub.order {
        let v = match hub.refresh_catalog(name).await {
            Ok(n) => json!({ "voices": n }),
            Err(e) => json!({ "error": e }),
        };
        result.insert(name.clone(), v);
    }
    Ok(Json(Value::Object(result)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_value_encodes_utf8() {
        assert_eq!(ext_value("VOICEVOX:ずんだもん"), "UTF-8''VOICEVOX%3A%E3%81%9A%E3%82%93%E3%81%A0%E3%82%82%E3%82%93");
        assert_eq!(ext_value("ebyroid:kiritan"), "UTF-8''ebyroid%3Akiritan");
    }

    // ---- 偽の VOICEVOX を相手に、HTTP の API を通しで確かめる ----

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    use crate::config::Config;

    /// 偽のエンジンが返す音声（24kHz モノラル、0.1 秒の正弦波）
    const ENGINE_RATE: u32 = 24000;

    #[derive(Default)]
    struct FakeEngine {
        /// true なら合成を 500 で失敗させる
        fail_synthesis: AtomicBool,
        /// 最後に /synthesis が受け取った audio_query
        last_query: Mutex<Option<Value>>,
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    async fn start_fake_engine(state: Arc<FakeEngine>) -> String {
        let router = Router::new()
            .route("/version", get(|| async { Json("0.0.0") }))
            .route(
                "/speakers",
                get(|| async {
                    Json(json!([
                        { "name": "ずんだもん", "speaker_uuid": "u-zunda", "styles": [
                            { "name": "ノーマル", "id": 3, "type": "talk" },
                            { "name": "ハミング", "id": 99, "type": "sing" },
                        ] },
                        { "name": "四国めたん", "speaker_uuid": "u-metan", "styles": [{ "name": "ノーマル", "id": 2 }] },
                    ]))
                }),
            )
            .route(
                "/speaker_info",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    let policy = match q["speaker_uuid"].as_str() {
                        "u-zunda" => "「VOICEVOX:ずんだもん」と記載すれば利用可能です。\nhttps://example.com/zunda",
                        _ => "",
                    };
                    Json(json!({ "policy": policy }))
                }),
            )
            .route("/audio_query", post(|| async { Json(json!({ "speedScale": 1.0, "pitchScale": 0.0 })) }))
            .route(
                "/synthesis",
                post(|State(s): State<Arc<FakeEngine>>, Json(q): Json<Value>| async move {
                    *s.last_query.lock().unwrap() = Some(q);
                    if s.fail_synthesis.load(Ordering::SeqCst) {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    let mut wav = audio::wav_header(ENGINE_RATE, 1).to_vec();
                    for i in 0..ENGINE_RATE / 10 {
                        let s = ((i as f32 * 440.0 * std::f32::consts::TAU / ENGINE_RATE as f32).sin() * 8000.0) as i16;
                        wav.extend_from_slice(&s.to_le_bytes());
                    }
                    ([(header::CONTENT_TYPE, "audio/wav")], wav).into_response()
                }),
            )
            .with_state(state);
        serve(router).await
    }

    struct Fixture {
        base: String,
        engine: Arc<FakeEngine>,
        http: reqwest::Client,
    }

    impl Fixture {
        /// `extra` は設定の末尾に足す YAML
        async fn start(extra: &str) -> Self {
            let engine = Arc::new(FakeEngine::default());
            let vv = start_fake_engine(engine.clone()).await;
            let state_dir = std::env::temp_dir().join(format!("ttshub-test-{}", reqlog::new_id()));
            let yaml = format!(
                r#"
default_voice: vv:ずんだもん
state_dir: {}
engines:
  vv: {{ plugin: voicevox-compatible, backends: ["{vv}"] }}
  down: {{ plugin: voicevox-compatible, backends: ["http://127.0.0.1:1"] }}
aliases:
  zundamon: vv:ずんだもん
presets:
  早口めたん: "vv:四国めたん?speed=1.5"
{extra}"#,
                state_dir.display()
            );
            let config: Config = serde_yaml::from_str(&yaml).unwrap();
            let hub = Arc::new(Hub::new(config).unwrap());
            // 止まっているバックエンドは 2 回続けて失敗して切り離される
            for _ in 0..2 {
                for e in hub.engines.values() {
                    e.check_health().await;
                }
            }
            hub.refresh_catalog("vv").await.unwrap();
            hub.fill_credits("vv").await;
            let base = serve(router(hub)).await;
            Self { base, engine, http: reqwest::Client::new() }
        }

        async fn speech(&self, body: Value) -> reqwest::Response {
            self.http.post(format!("{}/v1/speech", self.base)).json(&body).send().await.unwrap()
        }

        async fn get_json(&self, path: &str) -> Value {
            let resp = self.http.get(format!("{}{path}", self.base)).send().await.unwrap();
            assert_eq!(resp.status(), 200, "{path}");
            resp.json().await.unwrap()
        }
    }

    fn header_str<'a>(resp: &'a reqwest::Response, name: &str) -> &'a str {
        resp.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
    }

    async fn error_code(resp: reqwest::Response) -> String {
        let j: Value = resp.json().await.unwrap();
        j["error"]["code"].as_str().unwrap_or_default().to_owned()
    }

    #[tokio::test]
    async fn speech_returns_converted_pcm_with_voice_and_credit() {
        let f = Fixture::start("").await;
        let resp = f.speech(json!({ "text": "こんにちは", "voice": "zundamon" })).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(header_str(&resp, "content-type"), "audio/pcm");
        assert_eq!(header_str(&resp, "x-tts-pcm-sample-rate"), "48000");
        assert_eq!(header_str(&resp, "x-tts-pcm-channels"), "2");
        assert_eq!(header_str(&resp, "x-tts-resolved-voice"), ext_value("vv:ずんだもん/ノーマル"));
        assert_eq!(header_str(&resp, "x-tts-fallback"), "none");
        assert_eq!(header_str(&resp, "x-tts-credit"), ext_value("VOICEVOX:ずんだもん"));
        assert!(!header_str(&resp, "x-tts-request-id").is_empty());
        // 24kHz モノラル 0.1 秒 → 48kHz ステレオ 16bit でおよそ 19200 バイト
        let pcm = resp.bytes().await.unwrap();
        assert_eq!(pcm.len() % 4, 0);
        assert!((15_000..=24_000).contains(&pcm.len()), "{}", pcm.len());
    }

    #[tokio::test]
    async fn speech_returns_wav_and_passes_params_to_the_engine() {
        let f = Fixture::start("").await;
        let resp = f
            .speech(json!({ "text": "あ", "voice": "preset:早口めたん", "format": { "codec": "wav", "sample_rate": 16000, "channels": 1 } }))
            .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(header_str(&resp, "content-type"), "audio/wav");
        assert_eq!(header_str(&resp, "x-tts-resolved-voice"), ext_value("vv:四国めたん/ノーマル"));
        // クレジット表記のない話者には付けない
        assert!(resp.headers().get("x-tts-credit").is_none());
        let wav = resp.bytes().await.unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16000);
        assert_eq!(f.engine.last_query.lock().unwrap().as_ref().unwrap()["speedScale"], 1.5);
    }

    #[tokio::test]
    async fn speech_keeps_the_callers_request_id() {
        let f = Fixture::start("").await;
        let resp = f
            .http
            .post(format!("{}/v1/speech", f.base))
            .header("x-request-id", "abc-123")
            .json(&json!({ "text": "あ" }))
            .send()
            .await
            .unwrap();
        assert_eq!(header_str(&resp, "x-tts-request-id"), "abc-123");
    }

    #[tokio::test]
    async fn speech_rejects_bad_requests() {
        let f = Fixture::start("max_text_chars: 5\n").await;
        let cases = [
            (json!({ "text": "  " }), "empty_text"),
            (json!({ "text": "あいうえおか" }), "text_too_long"),
            (json!({ "text": "あ", "format": { "sample_rate": 1000 } }), "bad_format"),
            (json!({ "text": "あ", "format": { "channels": 3 } }), "bad_format"),
            (json!({ "text": "あ", "deadline_ms": 0 }), "bad_deadline"),
            (json!({ "voice": "zundamon" }), "bad_request"),
        ];
        for (body, code) in cases {
            let resp = f.speech(body.clone()).await;
            assert_eq!(resp.status(), 400, "{body}");
            assert_eq!(error_code(resp).await, code, "{body}");
        }
    }

    #[tokio::test]
    async fn unknown_voice_is_404_without_fallback_and_falls_back_otherwise() {
        let f = Fixture::start("").await;
        let resp = f.speech(json!({ "text": "あ", "voice": "vv:ずんだも", "fallback": "none" })).await;
        assert_eq!(resp.status(), 404);
        let j: Value = resp.json().await.unwrap();
        assert_eq!(j["error"]["code"], "unknown_voice");
        assert!(j["error"]["suggestions"].as_array().is_some_and(|s| !s.is_empty()), "{j}");

        let resp = f.speech(json!({ "text": "あ", "voice": "vv:だれか" })).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(header_str(&resp, "x-tts-fallback"), "unknown_voice");
        assert_eq!(header_str(&resp, "x-tts-resolved-voice"), ext_value("vv:ずんだもん/ノーマル"));
    }

    #[tokio::test]
    async fn stopped_engine_falls_back_to_the_default_voice() {
        let f = Fixture::start("").await;
        let resp = f.speech(json!({ "text": "あ", "voice": "down:だれか" })).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(header_str(&resp, "x-tts-fallback"), "engine_unavailable");
        assert_eq!(header_str(&resp, "x-tts-resolved-voice"), ext_value("vv:ずんだもん/ノーマル"));
    }

    #[tokio::test]
    async fn failing_engine_is_503() {
        let f = Fixture::start("").await;
        f.engine.fail_synthesis.store(true, Ordering::SeqCst);
        let resp = f.speech(json!({ "text": "あ", "voice": "zundamon", "fallback": "none" })).await;
        assert_eq!(resp.status(), 503);
        assert_eq!(error_code(resp).await, "unavailable");
    }

    #[tokio::test]
    async fn voices_lists_searches_and_pages() {
        let f = Fixture::start("").await;
        let all = f.get_json("/v1/voices").await;
        let addresses: Vec<&str> = all["voices"].as_array().unwrap().iter().map(|v| v["address"].as_str().unwrap()).collect();
        // 歌唱用のスタイルは出さない
        assert!(addresses.contains(&"vv:ずんだもん/ノーマル"), "{addresses:?}");
        assert!(addresses.contains(&"vv:四国めたん/ノーマル"), "{addresses:?}");
        assert!(!addresses.iter().any(|a| a.contains("ハミング")), "{addresses:?}");

        let hit = f.get_json("/v1/voices?q=%E3%82%81%E3%81%9F%E3%82%93&available=true").await;
        let first = &hit["voices"][0];
        assert_eq!(first["address"], "vv:四国めたん/ノーマル");
        assert_eq!(first["kind"], "voice");
        assert_eq!(first["available"], true);

        let page = f.get_json("/v1/voices?limit=1").await;
        assert_eq!(page["voices"].as_array().unwrap().len(), 1);
        let next = page["next_cursor"].as_str().unwrap();
        let page2 = f.get_json(&format!("/v1/voices?limit=1&cursor={next}")).await;
        assert_ne!(page["voices"][0]["address"], page2["voices"][0]["address"]);
    }

    #[tokio::test]
    async fn resolve_follows_aliases() {
        let f = Fixture::start("").await;
        let r = f.get_json("/v1/voices/resolve?q=zundamon").await;
        assert_eq!(r["voice"]["address"], "vv:ずんだもん/ノーマル");
        assert_eq!(r["voice"]["available"], true);
    }

    #[tokio::test]
    async fn admin_engines_reports_backend_state() {
        let f = Fixture::start("").await;
        let j = f.get_json("/admin/engines").await;
        let engines = j["engines"].as_array().unwrap();
        let by_name = |n: &str| engines.iter().find(|e| e["name"] == n).unwrap();
        assert_eq!(by_name("vv")["available"], true);
        assert_eq!(by_name("vv")["voices"], 2);
        assert_eq!(by_name("down")["available"], false);
    }

    #[tokio::test]
    async fn keys_are_required_once_clients_are_configured() {
        std::env::set_var("TTSHUB_TEST_CLIENT_KEY", "client-secret");
        std::env::set_var("TTSHUB_TEST_ADMIN_KEY", "admin-secret");
        let f = Fixture::start(
            "clients:\n  st: { key_env: TTSHUB_TEST_CLIENT_KEY, default_voice: vv:四国めたん }\nadmin_key_env: TTSHUB_TEST_ADMIN_KEY\n",
        )
        .await;
        let url = |p: &str| format!("{}{p}", f.base);

        // health だけは鍵なしで使える（コンテナのヘルスチェック用）
        assert_eq!(f.http.get(url("/v1/health")).send().await.unwrap().status(), 200);
        for key in [None, Some("wrong")] {
            let mut req = f.http.get(url("/v1/voices"));
            if let Some(k) = key {
                req = req.bearer_auth(k);
            }
            let resp = req.send().await.unwrap();
            assert_eq!(resp.status(), 401, "{key:?}");
            assert_eq!(error_code(resp).await, "unauthorized");
        }
        assert_eq!(f.speech(json!({ "text": "あ" })).await.status(), 401);

        // 利用者ごとのデフォルト話者を使う
        let resp =
            f.http.post(url("/v1/speech")).bearer_auth("client-secret").json(&json!({ "text": "あ" })).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(header_str(&resp, "x-tts-resolved-voice"), ext_value("vv:四国めたん/ノーマル"));

        // 管理 API は利用者の鍵では使えない
        let admin = |k: &str| f.http.get(url("/admin/engines")).bearer_auth(k.to_owned()).send();
        assert_eq!(admin("client-secret").await.unwrap().status(), 401);
        assert_eq!(admin("admin-secret").await.unwrap().status(), 200);
    }
}
