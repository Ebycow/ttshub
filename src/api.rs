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
}
