//! Open JTalk を HTTP で呼べるようにする、ttshub 用のエンジンのサーバー
//!
//! | エンドポイント | 内容 |
//! |---|---|
//! | `GET /health` | open_jtalk・辞書・声がそろっていれば 200、なければ 503 |
//! | `GET /voices` | 話者の一覧 `{ "voices": [{ "id", "character", "style" }] }` |
//! | `POST /synthesize` | `{ "voice": <id>, "text", "options": { "r": 1.2, ... } }` を合成し、ヘッダなしの PCM をストリームで返す |
//!
//! 合成の応答の形式はヘッダで伝える（`X-PCM-Sample-Rate`、`X-PCM-Channels`、`X-PCM-Bit-Depth`）。
//! 文ごとに合成して、できた文から順に返す。最初の文が合成できなければ、音声を返し始める前に 500 を返す。
//! `options` は open_jtalk のオプション（a, b, r, fm, u, jm, jf, g）で、範囲外の値は丸め、ほかは捨てる

mod synth;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

pub use synth::{OpenJTalk, SynthError, Voice, OPTIONS};

#[derive(Deserialize)]
struct SynthRequest {
    voice: String,
    text: String,
    #[serde(default)]
    options: HashMap<String, f64>,
}

pub fn router(engine: Arc<OpenJTalk>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/voices", get(voices))
        .route("/synthesize", post(synthesize))
        .with_state(engine)
}

async fn health(State(engine): State<Arc<OpenJTalk>>) -> StatusCode {
    if engine.healthy() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn voices(State(engine): State<Arc<OpenJTalk>>) -> Response {
    let voices = engine.voices();
    if voices.is_empty() {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Open JTalk の声（*.htsvoice）がない".into());
    }
    Json(serde_json::json!({ "voices": voices })).into_response()
}

async fn synthesize(State(engine): State<Arc<OpenJTalk>>, Json(req): Json<SynthRequest>) -> Response {
    match engine.synthesize(&req.voice, &req.text, &req.options).await {
        Ok((sample_rate, stream)) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::HeaderName::from_static("x-pcm-sample-rate"), sample_rate.to_string()),
                (header::HeaderName::from_static("x-pcm-channels"), "1".to_owned()),
                (header::HeaderName::from_static("x-pcm-bit-depth"), "16".to_owned()),
            ],
            Body::from_stream(stream),
        )
            .into_response(),
        Err(SynthError::UnknownVoice(id)) => error(StatusCode::NOT_FOUND, format!("声 {id} がない")),
        Err(SynthError::EmptyText) => error(StatusCode::BAD_REQUEST, "読む文がない".into()),
        Err(SynthError::Failed(e)) => {
            tracing::warn!(voice = %req.voice, "合成できない: {e}");
            error(StatusCode::INTERNAL_SERVER_ERROR, e)
        }
    }
}

fn error(status: StatusCode, message: String) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use crate::testing::FakeOpenJTalk;

    #[tokio::test]
    async fn serves_voices_and_streams_pcm() {
        let fake = FakeOpenJTalk::new();
        let base = fake.serve().await;
        let http = reqwest::Client::new();

        assert_eq!(http.get(format!("{base}/health")).send().await.unwrap().status(), 200);
        let voices: serde_json::Value = http.get(format!("{base}/voices")).send().await.unwrap().json().await.unwrap();
        assert_eq!(
            voices["voices"][0],
            serde_json::json!({ "id": "mei/mei_normal", "character": "mei", "style": "normal" })
        );

        let resp = http
            .post(format!("{base}/synthesize"))
            .json(&serde_json::json!({ "voice": "mei/mei_happy", "text": "あいう。えお", "options": { "r": 1.2 } }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["x-pcm-sample-rate"], "48000");
        assert_eq!(resp.headers()["x-pcm-channels"], "1");
        assert_eq!(resp.headers()["x-pcm-bit-depth"], "16");
        assert_eq!(resp.bytes().await.unwrap().len(), 36);

        let status = |voice: &'static str, text: &'static str| {
            let req =
                http.post(format!("{base}/synthesize")).json(&serde_json::json!({ "voice": voice, "text": text }));
            async move { req.send().await.unwrap().status() }
        };
        assert_eq!(status("nobody", "あ").await, 404);
        assert_eq!(status("mei/mei_normal", "  ").await, 400);
        assert_eq!(status("mei/mei_normal", "失敗する").await, 500);
    }
}
