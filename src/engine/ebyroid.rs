//! ebyroid（VOICEROID+ / VOICEROID2）
//!
//! 合成は POST /api/v2/audiostream。ヘッダなしの PCM をストリームで返し、形式はレスポンスヘッダで伝わる

use async_trait::async_trait;
use serde::Deserialize;

use super::{
    body_stream, status_error, AudioBody, BackendClient, Capabilities, EngineError, EnginePlugin, PcmFormat,
    VoiceEntry,
};
use crate::params::Params;

pub struct Ebyroid;

#[derive(Deserialize)]
struct VoiceList {
    voices: Vec<VoiceItem>,
}

#[derive(Deserialize)]
struct VoiceItem {
    name: String,
    #[serde(default)]
    default: bool,
}

#[async_trait]
impl EnginePlugin for Ebyroid {
    fn param_prefix(&self) -> &'static str {
        "ebyroid"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn list_voices(&self, backend: &BackendClient) -> Result<Vec<VoiceEntry>, EngineError> {
        let resp = backend.http.get(backend.url("/api/v1/voices")).send().await?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), "voices"));
        }
        let list: VoiceList = resp.json().await?;
        Ok(list
            .voices
            .into_iter()
            .map(|v| VoiceEntry { internal_id: v.name.clone(), character: v.name, style: None, is_default: v.default })
            .collect())
    }

    async fn synthesize(
        &self,
        backend: &BackendClient,
        voice: &VoiceEntry,
        text: &str,
        _params: &Params,
    ) -> Result<AudioBody, EngineError> {
        let resp = backend
            .http
            .post(backend.url("/api/v2/audiostream"))
            .json(&serde_json::json!({ "text": text, "name": voice.internal_id }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), "audiostream"));
        }
        let header = |name: &str| -> Result<u32, EngineError> {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| EngineError::Unavailable(format!("audiostream: {name} がない")))
        };
        let sample_rate = header("ebyroid-pcm-sample-rate")?;
        let channels = header("ebyroid-pcm-number-of-channels")?;
        let bit_depth = header("ebyroid-pcm-bit-depth")?;
        if bit_depth != 16 || !(1..=2).contains(&channels) {
            return Err(EngineError::Unavailable(format!(
                "audiostream: 対応していない形式 {bit_depth}bit {channels}ch"
            )));
        }
        let format = PcmFormat { sample_rate, channels: channels as u16 };
        Ok(AudioBody::Pcm(format, body_stream(resp)))
    }

    async fn health(&self, backend: &BackendClient) -> bool {
        matches!(backend.http.get(backend.url("/api/v1/health")).send().await, Ok(r) if r.status().is_success())
    }
}
