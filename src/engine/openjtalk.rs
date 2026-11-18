//! Open JTalk（ttshub-openjtalk のサーバー。リポジトリの openjtalk/）
//!
//! 合成は POST /synthesize。文ごとに合成したヘッダなしの PCM をストリームで返し、形式はレスポンスヘッダで伝わる
//!
//! 共通パラメータは open_jtalk のオプションに置き換える（speed → r、pitch → fm、volume → g）。
//! intonation は扱わない。近いのは jf（対数 F0 の GV の重み）だが、Mei や Takumi の声は F0 の GV を持たず、何も変わらない。
//! `x-openjtalk-<オプション名>` は open_jtalk のオプションとしてそのまま渡す（範囲に丸めるのと、知らないものを捨てるのはサーバー）

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Deserialize;

use super::{
    body_stream, status_error, AudioBody, BackendClient, Capabilities, EngineError, EnginePlugin, PcmFormat, VoiceEntry,
};
use crate::params::Params;

pub struct OpenJTalk;

#[derive(Deserialize)]
struct VoiceList {
    voices: Vec<VoiceItem>,
}

#[derive(Deserialize)]
struct VoiceItem {
    id: String,
    character: String,
    style: Option<String>,
}

#[async_trait]
impl EnginePlugin for OpenJTalk {
    fn param_prefix(&self) -> &'static str {
        "openjtalk"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { speed: true, pitch: true, volume: true, intonation: false }
    }

    async fn list_voices(&self, backend: &BackendClient) -> Result<Vec<VoiceEntry>, EngineError> {
        let resp = backend.http.get(backend.url("/voices")).send().await?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), "voices"));
        }
        let list: VoiceList = resp.json().await?;
        Ok(list
            .voices
            .into_iter()
            .map(|v| VoiceEntry { internal_id: v.id, character: v.character, style: v.style, is_default: false })
            .collect())
    }

    async fn synthesize(
        &self,
        backend: &BackendClient,
        voice: &VoiceEntry,
        text: &str,
        params: &Params,
    ) -> Result<AudioBody, EngineError> {
        let resp = backend
            .http
            .post(backend.url("/synthesize"))
            .json(&serde_json::json!({
                "voice": voice.internal_id,
                "text": text,
                "options": options(params, self.param_prefix()),
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status(), "synthesize"));
        }
        let header = |name: &str| -> Result<u32, EngineError> {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| EngineError::Unavailable(format!("synthesize: {name} がない")))
        };
        let sample_rate = header("x-pcm-sample-rate")?;
        let channels = header("x-pcm-channels")?;
        let bit_depth = header("x-pcm-bit-depth")?;
        if bit_depth != 16 || !(1..=2).contains(&channels) {
            return Err(EngineError::Unavailable(format!(
                "synthesize: 対応していない形式 {bit_depth}bit {channels}ch"
            )));
        }
        let format = PcmFormat { sample_rate, channels: channels as u16 };
        Ok(AudioBody::Pcm(format, body_stream(resp)))
    }

    async fn health(&self, backend: &BackendClient) -> bool {
        matches!(backend.http.get(backend.url("/health")).send().await, Ok(r) if r.status().is_success())
    }
}

/// 共通パラメータとエンジン固有パラメータを open_jtalk のオプションにする。エンジン固有のものが優先
fn options(params: &Params, prefix: &str) -> BTreeMap<String, f64> {
    let mut opts = BTreeMap::new();
    if let Some(speed) = params.speed {
        opts.insert("r".to_owned(), speed as f64);
    }
    // VOICEVOX の pitch は対数 F0（自然対数）に足す量。半音に直す（0.15 ≒ 2.6 半音）
    if let Some(pitch) = params.pitch {
        opts.insert("fm".to_owned(), pitch as f64 * 12.0 / std::f64::consts::LN_2);
    }
    // 倍率を dB に直す。0 は無音（サーバーが下限の -100dB に丸める）
    if let Some(volume) = params.volume {
        opts.insert("g".to_owned(), if volume > 0.0 { 20.0 * (volume as f64).log10() } else { -1000.0 });
    }
    for (key, raw) in params.extra_for(prefix) {
        if let Ok(value) = raw.parse::<f64>() {
            if value.is_finite() {
                opts.insert(key.to_owned(), value);
            }
        }
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::StreamExt;
    use ttshub_openjtalk::testing::FakeOpenJTalk;

    fn opts(list: &[(&str, f64)]) -> BTreeMap<String, f64> {
        list.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn common_params_become_open_jtalk_options() {
        let params =
            Params { speed: Some(1.5), pitch: Some(0.0), volume: Some(0.0), intonation: Some(1.2), extra: vec![] };
        assert_eq!(options(&params, "openjtalk"), opts(&[("fm", 0.0), ("g", -1000.0), ("r", 1.5)]));

        let o = options(&Params { pitch: Some(0.15), volume: Some(2.0), ..Default::default() }, "openjtalk");
        assert!((o["fm"] - 2.597).abs() < 0.01);
        assert!((o["g"] - 6.02).abs() < 0.01);
    }

    #[test]
    fn engine_specific_options_override_and_non_numbers_are_dropped() {
        let params = Params {
            speed: Some(1.5),
            extra: vec![
                ("x-openjtalk-r".into(), "0.8".into()),
                ("x-openjtalk-a".into(), "0.6".into()),
                ("x-openjtalk-b".into(), "soft".into()),
                ("x-voicevox-r".into(), "2".into()),
            ],
            ..Default::default()
        };
        assert_eq!(options(&params, "openjtalk"), opts(&[("a", 0.6), ("r", 0.8)]));
    }

    fn backend(base: String) -> BackendClient {
        BackendClient { base, http: reqwest::Client::new(), api_key: None }
    }

    #[tokio::test]
    async fn talks_to_the_openjtalk_server() {
        let fake = FakeOpenJTalk::new();
        let backend = backend(fake.serve().await);
        let plugin = OpenJTalk;
        assert!(plugin.health(&backend).await);

        let voices = plugin.list_voices(&backend).await.unwrap();
        let names: Vec<_> = voices.iter().map(|v| (v.character.as_str(), v.style.as_deref())).collect();
        assert_eq!(names, vec![("mei", Some("normal")), ("mei", Some("happy"))]);

        let params = Params { speed: Some(1.2), ..Default::default() };
        let AudioBody::Pcm(format, stream) =
            plugin.synthesize(&backend, &voices[1], "あいう。えお", &params).await.unwrap()
        else {
            panic!("PCM で返るはず");
        };
        assert_eq!(format, PcmFormat { sample_rate: 48000, channels: 1 });
        let bytes: Vec<Bytes> = stream.map(|c| c.unwrap()).collect().await;
        assert_eq!(bytes.iter().map(|c| c.len()).sum::<usize>(), 36);

        // 合成に失敗したら、代わりの声に回せるエラー
        let err = plugin.synthesize(&backend, &voices[0], "失敗する", &params).await.err().unwrap();
        assert!(matches!(err, EngineError::Unavailable(_)), "{err}");
    }

    #[tokio::test]
    async fn unreachable_server_is_unhealthy() {
        let backend = backend("http://127.0.0.1:9".into());
        assert!(!OpenJTalk.health(&backend).await);
        assert!(matches!(OpenJTalk.list_voices(&backend).await, Err(EngineError::Unavailable(_))));
    }
}
