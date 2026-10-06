//! VOICEVOX 互換エンジン（VOICEVOX, AivisSpeech, SHAREVOX, COEIROINK v1 など）
//!
//! 合成は audio_query → synthesis の 2 段階。synthesis は WAV を一括で返す
//!
//! クレジット表記と規約の URL は、AivisSpeech ならモデルの説明（/aivm_models）から、
//! それ以外は話者の規約（/speaker_info の policy）から読み取る。
//! AivisSpeech のこれらの API は数十秒かかるため、取得は裏で行い、取れた分を覚えておいて次の一覧に載せる

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use super::{
    body_stream, find_credit, find_last_url, status_error, AudioBody, BackendClient, Capabilities, EngineError,
    EnginePlugin, VoiceEntry,
};
use crate::params::Params;

#[derive(Default)]
pub struct Voicevox {
    credits: Arc<Mutex<CreditCache>>,
}

#[derive(Default)]
struct CreditCache {
    /// 話者 UUID → 読み取った結果（読み取れなかった話者も空で覚え、取り直さない）
    known: HashMap<String, Credit>,
    /// 裏で取得している最中か
    fetching: bool,
}

#[derive(Deserialize)]
struct Speaker {
    name: String,
    #[serde(default)]
    speaker_uuid: String,
    styles: Vec<Style>,
}

#[derive(Deserialize)]
struct Style {
    name: String,
    id: i64,
    /// talk 以外（歌唱用の sing など）は読み上げに使えない
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

/// 話者ごとのクレジット表記と規約の URL
#[derive(Debug, Clone, Default, PartialEq)]
struct Credit {
    credit: Option<String>,
    terms_url: Option<String>,
}

#[derive(Deserialize)]
struct SpeakerInfo {
    #[serde(default)]
    policy: String,
}

#[derive(Deserialize)]
struct AivmModel {
    manifest: AivmManifest,
    #[serde(default)]
    is_private_model: bool,
    #[serde(default)]
    speakers: Vec<AivmModelSpeaker>,
}

#[derive(Deserialize)]
struct AivmManifest {
    uuid: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct AivmModelSpeaker {
    speaker: AivmSpeaker,
}

#[derive(Deserialize)]
struct AivmSpeaker {
    speaker_uuid: String,
}

/// 規約を同時に取りに行く数
const CREDIT_FETCH_CONCURRENCY: usize = 8;
/// 話者一覧を返す前に、クレジットの取得を待つ上限（間に合わなければ次の一覧に載せる）
const CREDIT_WAIT: Duration = Duration::from_secs(4);
/// AivisSpeech の /aivm_models は音声サンプルなどを含み、数十 MB・数十秒かかる
const AIVM_MODELS_TIMEOUT: Duration = Duration::from_secs(300);
const SPEAKER_INFO_TIMEOUT: Duration = Duration::from_secs(60);

/// 話者の規約からクレジットを読み取る。読み取れなければ空
fn credit_from_policy(policy: &str) -> Credit {
    Credit { credit: find_credit(policy), terms_url: find_last_url(policy) }
}

/// AivisSpeech のモデル一覧から、話者 UUID ごとのクレジットを作る。規約は AivisHub のモデルのページ
fn credits_from_aivm_models(models: HashMap<String, AivmModel>) -> HashMap<String, Credit> {
    let mut out = HashMap::new();
    for model in models.into_values() {
        let credit = Credit {
            credit: find_credit(&model.manifest.description),
            terms_url: (!model.is_private_model)
                .then(|| format!("https://hub.aivis-project.com/aivm-models/{}", model.manifest.uuid)),
        };
        for sp in model.speakers {
            out.insert(sp.speaker.speaker_uuid, credit.clone());
        }
    }
    out
}

/// AivisSpeech のモデル一覧。AivisSpeech でない（404 など）か取れなければ None
async fn fetch_aivm_credits(backend: &BackendClient) -> Option<HashMap<String, Credit>> {
    let resp = backend.http.get(backend.url("/aivm_models")).timeout(AIVM_MODELS_TIMEOUT).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    match resp.json::<HashMap<String, AivmModel>>().await {
        Ok(models) => Some(credits_from_aivm_models(models)),
        Err(e) => {
            tracing::warn!(backend = %backend.base, error = %e.without_url(), "aivm_models を読めない");
            None
        }
    }
}

/// 話者の規約を取って読み取る。取れなければ None（次の一覧のときに取り直す）
async fn fetch_speaker_credit(backend: &BackendClient, uuid: &str) -> Option<Credit> {
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
    resp.json::<SpeakerInfo>().await.ok().map(|info| credit_from_policy(&info.policy))
}

/// まだ読み取っていない話者（UUID, キャラ名）のクレジットを取得する
async fn fetch_credits(backend: &BackendClient, missing: Vec<(String, String)>) -> HashMap<String, Credit> {
    let mut found = HashMap::new();
    if let Some(aivm) = fetch_aivm_credits(backend).await {
        found.extend(missing.iter().filter_map(|(uuid, _)| aivm.get(uuid).map(|c| (uuid.clone(), c.clone()))));
    }
    let rest: Vec<String> = missing
        .iter()
        .filter(|(uuid, _)| found.get(uuid).is_none_or(|c: &Credit| c.credit.is_none()))
        .map(|(uuid, _)| uuid.clone())
        .collect();
    let fetched: Vec<(String, Option<Credit>)> = futures::stream::iter(rest)
        .map(|uuid| async move {
            let credit = fetch_speaker_credit(backend, &uuid).await;
            (uuid, credit)
        })
        .buffer_unordered(CREDIT_FETCH_CONCURRENCY)
        .collect()
        .await;
    for (uuid, c) in fetched {
        match (found.remove(&uuid), c) {
            (Some(prev), Some(c)) => {
                found.insert(uuid, Credit { credit: c.credit, terms_url: prev.terms_url.or(c.terms_url) });
            }
            (prev, c) => {
                if let Some(c) = c.or(prev) {
                    found.insert(uuid, c);
                }
            }
        }
    }
    for (uuid, name) in &missing {
        match found.get(uuid) {
            Some(c) if c.credit.is_some() => {}
            Some(_) => tracing::warn!(backend = %backend.base, character = %name, "クレジット表記を読み取れない"),
            None => tracing::warn!(backend = %backend.base, character = %name, "話者の規約を取得できない。次の更新で取り直す"),
        }
    }
    found
}

impl Voicevox {
    /// 覚えているクレジットを返す。まだ読み取っていない話者があれば裏で取得を始め、少しだけ待つ
    async fn credits(&self, backend: &BackendClient, speakers: &[Speaker]) -> HashMap<String, Credit> {
        let handle = {
            let mut cache = self.credits.lock().unwrap();
            let missing: Vec<(String, String)> = speakers
                .iter()
                .filter(|sp| !sp.speaker_uuid.is_empty() && !cache.known.contains_key(&sp.speaker_uuid))
                .map(|sp| (sp.speaker_uuid.clone(), sp.name.clone()))
                .collect();
            if missing.is_empty() || cache.fetching {
                None
            } else {
                cache.fetching = true;
                let cache = Arc::clone(&self.credits);
                let backend = BackendClient {
                    base: backend.base.clone(),
                    http: backend.http.clone(),
                    api_key: backend.api_key.clone(),
                };
                Some(tokio::spawn(async move {
                    let found = fetch_credits(&backend, missing).await;
                    let mut cache = cache.lock().unwrap();
                    cache.known.extend(found);
                    cache.fetching = false;
                }))
            }
        };
        if let Some(mut handle) = handle {
            // 間に合わなければ取得は裏で続け、次の一覧に載せる
            let _ = tokio::time::timeout(CREDIT_WAIT, &mut handle).await;
        }
        self.credits.lock().unwrap().known.clone()
    }
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
        let mut credits = self.credits(backend, &speakers).await;
        Ok(speakers
            .into_iter()
            .flat_map(|sp| {
                let character = sp.name;
                let credit = credits.remove(&sp.speaker_uuid).unwrap_or_default();
                sp.styles
                    .into_iter()
                    .filter(|st| st.kind.as_deref().is_none_or(|k| k == "talk"))
                    .map(move |st| VoiceEntry {
                        internal_id: st.id.to_string(),
                        character: character.clone(),
                        style: Some(st.name),
                        is_default: false,
                        credit: credit.credit.clone(),
                        terms_url: credit.terms_url.clone(),
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

    #[test]
    fn reads_credit_from_aivm_model_description() {
        let models: HashMap<String, AivmModel> = serde_json::from_value(serde_json::json!({
            "a59cb814": {
                "is_private_model": false,
                "manifest": {
                    "uuid": "a59cb814",
                    "description": "いたずらっ子な小悪魔ボイス。\nクレジットして頂ける際は「AivisSpeech: まお」をご利用ください。"
                },
                "speakers": [{ "speaker": { "speaker_uuid": "e756b8e4", "name": "まお" } }]
            }
        }))
        .unwrap();
        let credits = credits_from_aivm_models(models);
        assert_eq!(
            credits["e756b8e4"],
            Credit {
                credit: Some("AivisSpeech: まお".into()),
                terms_url: Some("https://hub.aivis-project.com/aivm-models/a59cb814".into()),
            }
        );
    }
}
