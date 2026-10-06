//! VOICEVOX 互換エンジン（VOICEVOX, AivisSpeech, SHAREVOX, COEIROINK v1 など）
//!
//! 合成は audio_query → synthesis の 2 段階。synthesis は WAV を一括で返す
//!
//! クレジットは話者の規約（/speaker_info の policy）から読み取る

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use super::{
    body_stream, find_credit, find_last_url, status_error, AudioBody, BackendClient, Capabilities, Credit,
    EngineError, EnginePlugin, VoiceEntry,
};
use crate::params::Params;

pub struct Voicevox;

#[derive(Deserialize)]
struct Speaker {
    name: String,
    #[serde(default)]
    speaker_uuid: String,
    styles: Vec<Style>,
}

#[derive(Deserialize)]
struct SpeakerInfo {
    #[serde(default)]
    policy: String,
}

/// 規約を同時に取りに行く数
const CREDIT_FETCH_CONCURRENCY: usize = 4;
/// 規約 1 件の取得の上限（AivisSpeech は 1 件に数秒かかる）
const SPEAKER_INFO_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct Style {
    name: String,
    id: i64,
    /// talk 以外（歌唱用の sing など）は読み上げに使えない
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

#[async_trait]
impl EnginePlugin for Voicevox {
    fn param_prefix(&self) -> &'static str {
        "voicevox"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { speed: true, pitch: true, volume: true, intonation: true }
    }

    async fn list_voices(&self, backend: &BackendClient) -> Result<Vec<VoiceEntry>, EngineError> {
        let resp = backend.http.get(backend.url("/speakers")).send().await?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), "speakers"));
        }
        let speakers: Vec<Speaker> = resp.json().await?;
        Ok(speakers
            .into_iter()
            .flat_map(|sp| {
                let character = sp.name;
                sp.styles
                    .into_iter()
                    .filter(|st| st.kind.as_deref().is_none_or(|k| k == "talk"))
                    .map(move |st| VoiceEntry {
                        internal_id: st.id.to_string(),
                        character: character.clone(),
                        style: Some(st.name),
                        is_default: false,
                    })
            })
            .collect())
    }

    async fn synthesize(
        &self,
        backend: &BackendClient,
        voice: &VoiceEntry,
        text: &str,
        params: &Params,
    ) -> Result<AudioBody, EngineError> {
        let id = voice.internal_id.as_str();
        let resp = backend
            .http
            .post(backend.url("/audio_query"))
            .query(&[("text", text), ("speaker", id)])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), "audio_query"));
        }
        let mut query: Value = resp.json().await?;
        apply_params(&mut query, params, self.param_prefix());

        let resp = backend.http.post(backend.url("/synthesis")).query(&[("speaker", id)]).json(&query).send().await?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), "synthesis"));
        }
        Ok(AudioBody::Wav(body_stream(resp)))
    }

    async fn health(&self, backend: &BackendClient) -> bool {
        matches!(backend.http.get(backend.url("/version")).send().await, Ok(r) if r.status().is_success())
    }

    async fn credits(
        &self,
        backend: &BackendClient,
        characters: &[String],
    ) -> Result<HashMap<String, Credit>, EngineError> {
        let resp = backend.http.get(backend.url("/speakers")).send().await?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), "speakers"));
        }
        let speakers: Vec<Speaker> = resp.json().await?;
        let wanted = speakers.into_iter().filter(|sp| characters.contains(&sp.name) && !sp.speaker_uuid.is_empty());
        Ok(futures::stream::iter(wanted)
            .map(|sp| async move { fetch_policy(backend, &sp.speaker_uuid).await.map(|p| (sp.name, credit_from_policy(&p))) })
            .buffer_unordered(CREDIT_FETCH_CONCURRENCY)
            .filter_map(|r| async move { r })
            .collect()
            .await)
    }
}

/// 話者の規約の文面。取れなければ None
async fn fetch_policy(backend: &BackendClient, uuid: &str) -> Option<String> {
    let resp = backend
        .http
        .get(backend.url("/speaker_info"))
        // 立ち絵や音声サンプルを base64 で受け取らないよう、URL で返させる
        .query(&[("speaker_uuid", uuid), ("resource_format", "url")])
        .timeout(SPEAKER_INFO_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<SpeakerInfo>().await.ok().map(|info| info.policy)
}

/// 規約の「VOICEVOX:ずんだもん」のような表記と、末尾の URL を読み取る
fn credit_from_policy(policy: &str) -> Credit {
    Credit { credit: find_credit(policy), terms_url: find_last_url(policy) }
}

fn apply_params(query: &mut Value, params: &Params, prefix: &str) {
    let Some(obj) = query.as_object_mut() else { return };
    for (field, value) in [
        ("speedScale", params.speed),
        ("pitchScale", params.pitch),
        ("volumeScale", params.volume),
        ("intonationScale", params.intonation),
    ] {
        if let Some(v) = value {
            obj.insert(field.into(), Value::from(v as f64));
        }
    }
    // エンジン固有パラメータは audio_query の同名の項目を上書きする（数値、真偽値、文字列の順に解釈）
    for (key, raw) in params.extra_for(prefix) {
        let value = raw
            .parse::<f64>()
            .ok()
            .map(Value::from)
            .or_else(|| raw.parse::<bool>().ok().map(Value::from))
            .unwrap_or_else(|| Value::from(raw));
        obj.insert(key.into(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_overwrite_audio_query_fields() {
        let mut q = serde_json::json!({ "speedScale": 1.0, "pitchScale": 0.0, "prePhonemeLength": 0.1 });
        let params = Params {
            speed: Some(1.5),
            extra: vec![("x-voicevox-prePhonemeLength".into(), "0.3".into())],
            ..Default::default()
        };
        apply_params(&mut q, &params, "voicevox");
        assert_eq!(q["speedScale"], 1.5);
        assert_eq!(q["pitchScale"], 0.0);
        assert_eq!(q["prePhonemeLength"], 0.3);
    }
}
