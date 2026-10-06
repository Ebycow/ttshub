//! Aivis Cloud API（https://api.aivis-project.com）
//!
//! 使うモデルを設定の models に UUID で並べる。モデルの話者とスタイルを、それぞれ 1 つの声として扱う。
//! 合成は POST /v1/tts/synthesize。WAV で受け取り、形式の変換は共通変換に任せる
//! クレジットはモデルの説明（「AivisSpeech: まお」など）から読み取り、規約は AivisHub のモデルのページにする

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::{
    body_stream, find_credit, status_error, AudioBody, BackendClient, Capabilities, Credit, EngineError,
    EnginePlugin, VoiceEntry,
};
use crate::params::Params;

pub struct AivisCloud {
    models: Vec<String>,
}

impl AivisCloud {
    pub fn new(models: Vec<String>) -> Self {
        Self { models }
    }

    /// 使うモデルの情報。1 つのモデルが消えていても、ほかのモデルは使えるようにする
    async fn fetch_models(&self, backend: &BackendClient) -> Result<Vec<Model>, EngineError> {
        let mut models = Vec::new();
        for uuid in &self.models {
            let req = backend.http.get(backend.url(&format!("/v1/aivm-models/{uuid}")));
            let resp = authorized(req, backend).send().await?;
            if !resp.status().is_success() {
                tracing::warn!(model = %uuid, status = %resp.status(), "Aivis のモデルを取得できない");
                continue;
            }
            models.push(resp.json().await?);
        }
        Ok(models)
    }
}

#[derive(Deserialize)]
struct Model {
    aivm_model_uuid: String,
    #[serde(default)]
    description: String,
    speakers: Vec<Speaker>,
}

#[derive(Deserialize)]
struct Speaker {
    aivm_speaker_uuid: String,
    name: String,
    styles: Vec<Style>,
}

#[derive(Deserialize)]
struct Style {
    name: String,
    local_id: i64,
}

/// 内部 ID は `モデル UUID/話者 UUID/スタイル ID`
fn internal_id(model: &str, speaker: &str, style: i64) -> String {
    format!("{model}/{speaker}/{style}")
}

fn parse_internal_id(id: &str) -> Option<(&str, &str, i64)> {
    let mut parts = id.splitn(3, '/');
    let model = parts.next()?;
    let speaker = parts.next()?;
    let style = parts.next()?.parse().ok()?;
    Some((model, speaker, style))
}

fn authorized(req: reqwest::RequestBuilder, backend: &BackendClient) -> reqwest::RequestBuilder {
    match &backend.api_key {
        Some(key) => req.bearer_auth(key),
        None => req,
    }
}

/// 合成リクエストの本文
fn request_body(voice: &VoiceEntry, text: &str, params: &Params, prefix: &str) -> Option<Value> {
    let (model, speaker, style) = parse_internal_id(&voice.internal_id)?;
    let mut body = Map::new();
    body.insert("model_uuid".into(), json!(model));
    body.insert("speaker_uuid".into(), json!(speaker));
    body.insert("style_id".into(), json!(style));
    body.insert("text".into(), json!(text));
    body.insert("output_format".into(), json!("wav"));
    if let Some(v) = params.speed {
        body.insert("speaking_rate".into(), json!(v));
    }
    if let Some(v) = params.volume {
        body.insert("volume".into(), json!(v));
    }
    // そのほかは x-aivis-<キー> で渡す（pitch、emotional_intensity、tempo_dynamics など）
    for (key, raw) in params.extra_for(prefix) {
        let value = raw
            .parse::<f64>()
            .ok()
            .map(Value::from)
            .or_else(|| raw.parse::<bool>().ok().map(Value::from))
            .unwrap_or_else(|| Value::from(raw));
        body.insert(key.into(), value);
    }
    // チャットの文章が SSML のタグとして解釈されないようにする（利用者のパラメータでも有効にさせない）
    body.insert("use_ssml".into(), json!(false));
    Some(Value::Object(body))
}

#[async_trait]
impl EnginePlugin for AivisCloud {
    fn param_prefix(&self) -> &'static str {
        "aivis"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { speed: true, pitch: false, volume: true, intonation: false }
    }

    async fn list_voices(&self, backend: &BackendClient) -> Result<Vec<VoiceEntry>, EngineError> {
        let mut entries = Vec::new();
        for model in self.fetch_models(backend).await? {
            for sp in model.speakers {
                for st in sp.styles {
                    entries.push(VoiceEntry {
                        internal_id: internal_id(&model.aivm_model_uuid, &sp.aivm_speaker_uuid, st.local_id),
                        character: sp.name.clone(),
                        style: Some(st.name),
                        is_default: false,
                    });
                }
            }
        }
        if entries.is_empty() {
            return Err(EngineError::Unavailable("使えるモデルがない".into()));
        }
        Ok(entries)
    }

    async fn synthesize(
        &self,
        backend: &BackendClient,
        voice: &VoiceEntry,
        text: &str,
        params: &Params,
    ) -> Result<AudioBody, EngineError> {
        let body = request_body(voice, text, params, self.param_prefix())
            .ok_or_else(|| EngineError::Rejected(format!("内部 ID が正しくない: {}", voice.internal_id)))?;
        let req = backend.http.post(backend.url("/v1/tts/synthesize")).json(&body);
        let resp = authorized(req, backend).send().await?;
        let status = resp.status();
        if !status.is_success() {
            // 402: クレジット不足。止まっているのと同じ扱いにして、代わりの声で読ませる
            if status == reqwest::StatusCode::PAYMENT_REQUIRED || status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(EngineError::Unavailable(format!("synthesize: HTTP {status}")));
            }
            return Err(status_error(status, "synthesize"));
        }
        Ok(AudioBody::Wav(body_stream(resp)))
    }

    async fn health(&self, backend: &BackendClient) -> bool {
        // 認証の要らない検索で、API に届くかだけを確かめる（課金されない）
        let req = backend.http.get(backend.url("/v1/aivm-models/search")).query(&[("limit", "1")]);
        matches!(req.send().await, Ok(r) if r.status().is_success())
    }

    async fn credits(
        &self,
        backend: &BackendClient,
        characters: &[String],
    ) -> Result<HashMap<String, Credit>, EngineError> {
        let mut out = HashMap::new();
        for model in self.fetch_models(backend).await? {
            let credit = Credit {
                credit: find_credit(&model.description),
                terms_url: Some(format!("https://hub.aivis-project.com/aivm-models/{}", model.aivm_model_uuid)),
            };
            for sp in model.speakers.into_iter().filter(|sp| characters.contains(&sp.name)) {
                out.insert(sp.name, credit.clone());
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice() -> VoiceEntry {
        VoiceEntry {
            internal_id: internal_id("model-1", "speaker-1", 2),
            character: "まお".into(),
            style: Some("あまあま".into()),
            is_default: false,
        }
    }

    #[test]
    fn builds_request_with_model_speaker_and_style() {
        let params = Params { speed: Some(1.2), volume: Some(0.8), ..Default::default() };
        let body = request_body(&voice(), "こんにちは<break time=\"1s\"/>", &params, "aivis").unwrap();
        assert_eq!(body["model_uuid"], "model-1");
        assert_eq!(body["speaker_uuid"], "speaker-1");
        assert_eq!(body["style_id"], 2);
        assert_eq!(body["output_format"], "wav");
        assert_eq!(body["use_ssml"], false);
        assert!((body["speaking_rate"].as_f64().unwrap() - 1.2).abs() < 1e-6);
        assert!((body["volume"].as_f64().unwrap() - 0.8).abs() < 1e-6);
    }

    #[test]
    fn engine_specific_params_pass_through_but_ssml_stays_off() {
        let params = Params {
            extra: vec![
                ("x-aivis-emotional_intensity".into(), "1.5".into()),
                ("x-aivis-use_ssml".into(), "true".into()),
            ],
            ..Default::default()
        };
        let body = request_body(&voice(), "a", &params, "aivis").unwrap();
        assert_eq!(body["emotional_intensity"], 1.5);
        assert_eq!(body["use_ssml"], false);
    }

    #[test]
    fn rejects_malformed_internal_id() {
        let mut v = voice();
        v.internal_id = "broken".into();
        assert!(request_body(&v, "a", &Params::default(), "aivis").is_none());
    }
}
