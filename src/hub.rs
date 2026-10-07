//! 合成の取りまとめ: アドレスの解決、フォールバック、カタログの更新

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use indexmap::IndexMap;

use crate::address::{self, Target};
use crate::catalog::{Catalog, EngineVoices, Named, Resolution, Voice};
use crate::config::Config;
use crate::engine::{AudioBody, Credit, EngineError, VoiceEntry};
use crate::params::{self, Params};
use crate::router::Engine;

/// 別名・プリセット・default をたどる上限（循環の防止）
const MAX_EXPANSION: usize = 8;

#[derive(Debug, Clone)]
pub struct ClientCtx {
    pub name: String,
    pub default_voice: Option<String>,
    pub allow_billable_fallback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    None,
    BackendUnavailable,
    EngineUnavailable,
    UnknownVoice,
}

impl FallbackReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FallbackReason::None => "none",
            FallbackReason::BackendUnavailable => "backend_unavailable",
            FallbackReason::EngineUnavailable => "engine_unavailable",
            FallbackReason::UnknownVoice => "unknown_voice",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("{0}")]
    BadAddress(String),
    #[error("unknown voice: {name}")]
    UnknownVoice { name: String, suggestions: Vec<String> },
    #[error("{0}")]
    Unavailable(String),
}

pub struct SpeechOutcome {
    pub body: AudioBody,
    pub voice: Voice,
    pub reason: FallbackReason,
    pub warnings: Vec<String>,
    /// プラグインが音量に対応しないとき、共通変換でかけるゲイン
    pub gain: Option<f32>,
    pub backend: String,
}

/// アドレスを解決した結果
struct Requested {
    voice: Option<Voice>,
    engine: Option<String>,
    params: Vec<(String, String)>,
    /// 見つからなかったときの照合用の文字列
    label: String,
}

pub struct Hub {
    pub config: Config,
    pub engines: IndexMap<String, Arc<Engine>>,
    pub catalog: Catalog,
    pub order: Vec<String>,
    clients: HashMap<String, ClientCtx>,
    admin_key: Option<String>,
    /// エンジン → キャラ（slug）→ エンジンが申告したクレジット。調べ終えたキャラだけを持つ
    credits: RwLock<HashMap<String, HashMap<String, Credit>>>,
}

impl Hub {
    pub fn new(config: Config) -> Result<Self, String> {
        // 合成は上限を設けずにエンジンの応答を待つことがある（打ち切った合成の計算が終わるまで枠を持つ）。
        // エンジンのマシンごと落ちて切断が届かなくても気づけるよう、TCP keepalive を使う
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(3))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let mut engines = IndexMap::new();
        for (name, ec) in &config.engines {
            engines.insert(name.clone(), Arc::new(Engine::new(name, ec.clone(), http.clone())?));
        }
        let mut clients = HashMap::new();
        for (name, cc) in &config.clients {
            match std::env::var(&cc.key_env) {
                Ok(key) if !key.is_empty() => {
                    clients.insert(
                        key,
                        ClientCtx {
                            name: name.clone(),
                            default_voice: cc.default_voice.clone(),
                            allow_billable_fallback: cc.allow_billable_fallback,
                        },
                    );
                }
                _ => tracing::warn!(client = %name, env = %cc.key_env, "鍵の環境変数がないため、この利用者は使えない"),
            }
        }
        if !config.clients.is_empty() && clients.is_empty() {
            return Err("clients を設定したが、有効な鍵が 1 つもない".into());
        }
        let admin_key = config.admin_key_env.as_ref().and_then(|k| std::env::var(k).ok()).filter(|k| !k.is_empty());
        let order = config.engines.keys().cloned().collect();
        Ok(Self {
            config,
            engines,
            catalog: Catalog::default(),
            order,
            clients,
            admin_key,
            credits: RwLock::default(),
        })
    }

    /// 鍵から利用者を特定する。利用者を設定していなければ誰でも使える（Ok(None)）
    pub fn authenticate(&self, bearer: Option<&str>) -> Result<Option<ClientCtx>, ()> {
        if self.clients.is_empty() {
            return Ok(None);
        }
        bearer.and_then(|k| self.clients.get(k)).cloned().map(Some).ok_or(())
    }

    pub fn authorize_admin(&self, bearer: Option<&str>) -> bool {
        self.admin_key.as_deref().is_none_or(|k| bearer == Some(k))
    }

    /// 別名とプリセットの一覧（検索・照合の候補用）。プリセットは `preset:` を付けた名前
    pub fn named_targets(&self) -> Vec<Named> {
        self.config
            .aliases
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .chain(self.config.presets.iter().map(|(k, v)| (format!("preset:{k}"), v.clone())))
            .map(|(name, target)| {
                let group = self.voice_of(&target, None).map(|v| format!("{}:{}", v.engine, address::slug(&v.character)));
                Named { name, target, group }
            })
            .collect()
    }

    fn expand(&self, raw: &str, client: Option<&ClientCtx>) -> Result<Requested, HubError> {
        let mut addr = address::parse(raw).map_err(|e| HubError::BadAddress(e.to_string()))?;
        let mut params = addr.params.clone();
        for _ in 0..MAX_EXPANSION {
            let next = match &addr.target {
                Target::Voice { engine, speaker } => {
                    let known = self.engines.contains_key(engine);
                    return Ok(Requested {
                        voice: self.catalog.lookup(engine, speaker),
                        engine: known.then(|| engine.clone()),
                        params,
                        label: format!("{engine}:{speaker}"),
                    });
                }
                Target::Default => client
                    .and_then(|c| c.default_voice.clone())
                    .unwrap_or_else(|| self.config.default_voice.clone()),
                Target::Preset(name) => match self.config.presets.get(name) {
                    Some(t) => t.clone(),
                    None => return Ok(unknown(params, name)),
                },
                Target::Bare(name) => match self.config.aliases.get(name) {
                    Some(t) => t.clone(),
                    None => return Ok(unknown(params, name)),
                },
            };
            addr = address::parse(&next).map_err(|e| HubError::BadAddress(e.to_string()))?;
            // 展開先のパラメータより、利用者が書いたパラメータを優先する
            let mut merged = addr.params.clone();
            merged.extend(params);
            params = merged;
        }
        Err(HubError::BadAddress(format!("{raw}: 別名・プリセットの展開が深すぎる")))
    }

    /// アドレス（別名・プリセットを含む）が指す話者
    pub fn describe(&self, address: &str) -> Option<Voice> {
        self.voice_of(address, None)
    }

    /// あいまい照合。別名に当たったら、指す先の正式なアドレスを返す（プリセットはパラメータを持つのでそのまま）
    pub fn resolve(&self, q: &str) -> (Resolution, Option<Voice>) {
        let mut r = self.catalog.resolve(&self.order, q, &self.named_targets());
        let voice = r.matched.as_deref().and_then(|m| self.describe(m));
        if let (Some(m), Some(v)) = (&r.matched, &voice) {
            let alias_without_params = self.config.aliases.get(m).is_some_and(|t| !t.contains('?'));
            if alias_without_params {
                r.matched = Some(v.address.clone());
            }
        }
        (r, voice)
    }

    /// アドレスを話者に解決する（パラメータは捨てる）
    fn voice_of(&self, raw: &str, client: Option<&ClientCtx>) -> Option<Voice> {
        self.expand(raw, client).ok().and_then(|r| r.voice)
    }

    /// 試す順の話者。要求した話者 → 同じキャラの別エンジン → エンジンのデフォルト → 利用者のデフォルト → 全体のデフォルト
    fn candidates(&self, req: &Requested, client: Option<&ClientCtx>, fallback: bool) -> Vec<Voice> {
        let mut list: Vec<Voice> = req.voice.iter().cloned().collect();
        if !fallback {
            return list;
        }
        if let Some(v) = &req.voice {
            // 対応はキャラ単位で見る。移った先に同じスタイルがあればそれを使う
            let same_character =
                |a: &Voice, b: &Voice| a.engine == b.engine && address::slug(&a.character) == address::slug(&b.character);
            for group in &self.config.equivalents {
                let resolved: Vec<Voice> = group.iter().filter_map(|a| self.voice_of(a, client)).collect();
                if !resolved.iter().any(|g| same_character(g, v)) {
                    continue;
                }
                for g in resolved.into_iter().filter(|g| !same_character(g, v)) {
                    let same_style = v
                        .style
                        .as_ref()
                        .and_then(|st| self.catalog.lookup(&g.engine, &format!("{}/{st}", g.character)));
                    list.push(same_style.unwrap_or(g));
                }
            }
        }
        if let Some(e) = &req.engine {
            list.extend(self.catalog.engine_default(e));
        }
        if let Some(d) = client.and_then(|c| c.default_voice.as_deref()) {
            list.extend(self.voice_of(d, client));
        }
        list.extend(self.voice_of(&self.config.default_voice, client));

        let allow_billable = client.is_some_and(|c| c.allow_billable_fallback);
        let mut seen = std::collections::HashSet::new();
        list.into_iter()
            .enumerate()
            .filter(|(i, v)| (req.voice.is_some() && *i == 0) || allow_billable || !v.billable)
            .map(|(_, v)| v)
            .filter(|v| seen.insert(v.address.clone()))
            .collect()
    }

    /// 合成する。`deadline` があれば、その時刻までに音声を返し始めるか、あきらめてエラーにする。
    /// 最初の候補は、代わりの声を試す時間（`fallback_reserve_ms`）を残して打ち切る。代わりの声は残りをすべて使える。
    /// 時間切れや接続できないなどで失敗したエンジンの声は、残りの候補からも外す（同じエンジンは計算中か止まっているため）
    pub async fn synthesize(
        &self,
        text: &str,
        voice: &str,
        fallback: bool,
        client: Option<&ClientCtx>,
        deadline: Option<Instant>,
    ) -> Result<SpeechOutcome, HubError> {
        let req = self.expand(voice, client)?;
        let (params, mut warnings) = params::resolve(&req.params);
        let cands = self.candidates(&req, client, fallback);
        if cands.is_empty() {
            let names = self.named_targets();
            let suggestions = self.catalog.resolve(&self.order, &req.label, &names).suggestions;
            return Err(HubError::UnknownVoice { name: req.label, suggestions });
        }

        // 話者一覧をまだ一度も取れていないエンジンなら、話者が無いのではなくエンジンが止まっている
        let engine_down = req.voice.is_none() && req.engine.as_ref().is_some_and(|e| self.catalog.engine(e).is_none());
        let mut last = String::new();
        // 期限が短くても要求された声を試せるよう、残しておく時間は期限までの半分までにする
        let reserve = Duration::from_millis(self.config.timeouts.fallback_reserve_ms)
            .min(deadline.map_or(Duration::MAX, |d| d.saturating_duration_since(Instant::now()) / 2));
        // 時間切れや接続できないなどで失敗したエンジン。声が見つからないときの代わり（エンジンのデフォルト）とは違い、
        // 同じエンジンの別の声を試しても間に合わないので飛ばす
        let mut down: HashSet<&str> = HashSet::new();
        for (i, v) in cands.iter().enumerate() {
            if down.contains(v.engine.as_str()) {
                continue;
            }
            // 残しておく時間は、別のエンジンの候補があるときだけ取る（同じエンジンの候補は失敗したら飛ばすため）
            let has_fallback = i == 0 && cands[1..].iter().any(|c| c.engine != v.engine);
            let until = deadline.map(|d| if has_fallback { d.checked_sub(reserve).unwrap_or(d) } else { d });
            if until.is_some_and(|u| u <= Instant::now()) {
                if deadline.is_some_and(|d| d <= Instant::now()) {
                    last = format!("{}: 期限までに試す時間がなかった", v.address);
                    break;
                }
                // 代わりの声に回す時間しか残っていない
                continue;
            }
            let engine = &self.engines[&v.engine];
            match engine.synthesize(&v.entry, text, &params, &self.config.timeouts, until).await {
                Ok((body, backend, switched)) => {
                    let reason = match (i, req.voice.is_some()) {
                        (0, true) if switched => FallbackReason::BackendUnavailable,
                        (0, true) => FallbackReason::None,
                        (_, false) if engine_down => FallbackReason::EngineUnavailable,
                        (_, false) => FallbackReason::UnknownVoice,
                        _ => FallbackReason::EngineUnavailable,
                    };
                    let gain = unsupported(engine, &params, &mut warnings);
                    return Ok(SpeechOutcome { body, voice: v.clone(), reason, warnings, gain, backend });
                }
                Err(e) => {
                    if matches!(e, EngineError::Unavailable(_)) {
                        down.insert(v.engine.as_str());
                    }
                    tracing::warn!(voice = %v.address, error = %e, "合成に失敗。次の候補を試す");
                    last = format!("{}: {e}", v.address);
                }
            }
        }
        Err(HubError::Unavailable(last))
    }

    // ---- カタログ ----

    fn snapshot_path(&self, engine: &str) -> PathBuf {
        self.config.state_dir.join("catalog").join(format!("{engine}.json"))
    }

    /// 前回保存したカタログを読む（再起動直後、エンジンが応答しなくても使えるように）
    pub fn load_snapshots(&self) {
        for name in self.engines.keys() {
            let Ok(text) = std::fs::read_to_string(self.snapshot_path(name)) else { continue };
            match serde_json::from_str::<Vec<VoiceEntry>>(&text) {
                Ok(entries) => {
                    let ev = self.build(name, entries);
                    tracing::info!(engine = %name, voices = ev.len(), "保存済みのカタログを読み込んだ");
                    self.catalog.set(name, ev);
                }
                Err(e) => tracing::warn!(engine = %name, error = %e, "保存済みのカタログが読めない"),
            }
        }
    }

    pub async fn refresh_catalog(&self, name: &str) -> Result<usize, String> {
        let engine = self.engines.get(name).ok_or("知らないエンジン")?;
        let entries = engine.list_voices().await.map_err(|e| e.to_string())?;
        let ev = self.build(name, entries.clone());
        let n = ev.len();
        self.catalog.set(name, ev);
        let path = self.snapshot_path(name);
        let saved = std::fs::create_dir_all(path.parent().unwrap())
            .and_then(|_| std::fs::write(&path, serde_json::to_vec(&entries).unwrap()));
        if let Err(e) = saved {
            tracing::warn!(engine = %name, error = %e, "カタログを保存できない");
        }
        Ok(n)
    }

    pub async fn refresh_all(&self) {
        for name in &self.order {
            match self.refresh_catalog(name).await {
                Ok(n) => tracing::info!(engine = %name, voices = n, "カタログを更新"),
                Err(e) => tracing::warn!(engine = %name, error = %e, "カタログを更新できない。前回の一覧を使い続ける"),
            }
        }
    }

    fn build(&self, name: &str, entries: Vec<VoiceEntry>) -> EngineVoices {
        let credits = self.credits.read().unwrap();
        EngineVoices::build(name, &self.engines[name].config, entries, credits.get(name).unwrap_or(&HashMap::new()))
    }

    /// まだ調べていないキャラのクレジットをエンジンに問い合わせ、カタログに反映する。
    /// 話者一覧の取得とは別に呼ぶ（規約の取得に時間がかかるエンジンがあるため）
    pub async fn fill_credits(&self, name: &str) {
        let (Some(engine), Some(ev)) = (self.engines.get(name), self.catalog.engine(name)) else { return };
        let missing: Vec<String> = {
            let credits = self.credits.read().unwrap();
            let known = credits.get(name);
            ev.characters().into_iter().filter(|c| known.is_none_or(|k| !k.contains_key(&address::slug(c)))).collect()
        };
        if missing.is_empty() {
            return;
        }
        let found = match engine.credits(&missing).await {
            Ok(found) if !found.is_empty() => found,
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(engine = %name, error = %e, "クレジットを取得できない。次の更新で取り直す");
                return;
            }
        };
        let configured = |c: &str| engine.config.credits.keys().any(|k| address::slug(k) == address::slug(c));
        for (character, credit) in &found {
            if credit.credit.is_none() && !configured(character) {
                tracing::warn!(engine = %name, character = %character, "クレジット表記が分からない。必要なら設定の credits に書く");
            }
        }
        self.credits
            .write()
            .unwrap()
            .entry(name.to_owned())
            .or_default()
            .extend(found.into_iter().map(|(c, credit)| (address::slug(&c), credit)));
        // 問い合わせの間にカタログが更新されていてもよいように、今の一覧から作り直す
        if let Some(ev) = self.catalog.engine(name) {
            self.catalog.set(name, self.build(name, ev.entries()));
        }
    }

    pub async fn fill_all_credits(&self) {
        for name in &self.order {
            self.fill_credits(name).await;
        }
    }
}

fn unknown(params: Vec<(String, String)>, name: &str) -> Requested {
    Requested { voice: None, engine: None, params, label: name.to_owned() }
}

/// エンジンが処理できないパラメータを警告し、音量だけは共通変換で処理する
fn unsupported(engine: &Engine, params: &Params, warnings: &mut Vec<String>) -> Option<f32> {
    let cap = engine.plugin.capabilities();
    for (name, set, ok) in [
        ("speed", params.speed.is_some(), cap.speed),
        ("pitch", params.pitch.is_some(), cap.pitch),
        ("intonation", params.intonation.is_some(), cap.intonation),
    ] {
        if set && !ok {
            warnings.push(format!("{name} is not supported by {}; ignored", engine.name));
        }
    }
    if cap.volume {
        None
    } else {
        params.volume
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub(yaml: &str) -> Hub {
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let hub = Hub::new(config).unwrap();
        let vv = |id: &str, c: &str, s: &str| VoiceEntry {
            internal_id: id.into(),
            character: c.into(),
            style: Some(s.into()),
            is_default: false,
        };
        let eby = |n: &str, d: bool| VoiceEntry { internal_id: n.into(), character: n.into(), style: None, is_default: d };
        for (name, entries) in [
            ("ebyroid", vec![eby("kiritan", true), eby("aoi", false)]),
            ("voicevox", vec![vv("2", "四国めたん", "ノーマル"), vv("3", "ずんだもん", "ノーマル"), vv("108", "東北きりたん", "ノーマル")]),
            ("cloud", vec![vv("alloy", "alloy", "default")]),
        ] {
            hub.catalog.set(name, hub.build(name, entries));
        }
        hub
    }

    const YAML: &str = r#"
default_voice: ebyroid:kiritan
engines:
  ebyroid: { plugin: ebyroid, backends: [http://a] }
  voicevox: { plugin: voicevox-compatible, backends: [http://b], default_voice: ずんだもん }
  cloud: { plugin: voicevox-compatible, backends: [http://c], billable: true }
equivalents:
  - [ebyroid:kiritan, voicevox:東北きりたん]
aliases:
  zundamon: voicevox:ずんだもん
presets:
  早口ずんだもん: "zundamon?speed=1.4&pitch=0.1"
"#;

    /// 話者（internal_id）ごとに決めた時間をかけて応答を始める偽のエンジン。None なら応答を返したまま送り終えない
    struct SlowPlugin(HashMap<&'static str, Option<Duration>>);

    #[async_trait::async_trait]
    impl crate::engine::EnginePlugin for SlowPlugin {
        fn param_prefix(&self) -> &'static str {
            "slow"
        }
        fn capabilities(&self) -> crate::engine::Capabilities {
            Default::default()
        }
        async fn list_voices(&self, _: &crate::engine::BackendClient) -> Result<Vec<VoiceEntry>, crate::engine::EngineError> {
            Ok(vec![])
        }
        async fn synthesize(
            &self,
            _: &crate::engine::BackendClient,
            voice: &VoiceEntry,
            _: &str,
            _: &Params,
        ) -> Result<AudioBody, crate::engine::EngineError> {
            let format = crate::engine::PcmFormat { sample_rate: 48000, channels: 2 };
            match self.0.get(voice.internal_id.as_str()).copied().flatten() {
                Some(delay) => {
                    tokio::time::sleep(delay).await;
                    let chunk = futures::stream::iter([Ok(bytes::Bytes::from_static(&[0; 4]))]);
                    Ok(AudioBody::Pcm(format, Box::pin(chunk)))
                }
                None => Ok(AudioBody::Pcm(format, Box::pin(futures::stream::pending()))),
            }
        }
        async fn health(&self, _: &crate::engine::BackendClient) -> bool {
            true
        }
    }

    /// YAML のエンジンを、すべて偽のエンジンに差し替えた Hub
    fn slow_hub(yaml: &str, delays: &[(&'static str, Option<u64>)]) -> Hub {
        let mut h = hub(yaml);
        let plugin: Arc<SlowPlugin> =
            Arc::new(SlowPlugin(delays.iter().map(|(id, ms)| (*id, ms.map(Duration::from_millis))).collect()));
        for (name, ec) in h.config.engines.clone() {
            let e = Engine::with_plugin(&name, ec, reqwest::Client::new(), plugin.clone());
            h.engines.insert(name, Arc::new(e));
        }
        h
    }

    fn after_ms(ms: u64) -> Option<Instant> {
        Some(Instant::now() + Duration::from_millis(ms))
    }

    #[tokio::test]
    async fn deadline_leaves_time_for_fallback() {
        let yaml = format!("{YAML}timeouts: {{ fallback_reserve_ms: 300 }}\n");
        // 東北きりたん（voicevox）は 5 秒かかり、代わりの ebyroid:kiritan はすぐ応答する
        let h = slow_hub(&yaml, &[("108", Some(5000)), ("kiritan", Some(10))]);
        let started = Instant::now();
        let out = h.synthesize("あ", "voicevox:東北きりたん", true, None, after_ms(700)).await.ok().unwrap();
        let took = started.elapsed();
        assert_eq!(out.voice.address, "ebyroid:kiritan");
        assert!(took >= Duration::from_millis(350) && took < Duration::from_millis(700), "{took:?}");
        // 期限のせいで打ち切っただけなので、バックエンドの失敗にしない
        assert!(h.engines["voicevox"].status()[0].last_error.is_none());
    }

    #[tokio::test]
    async fn slow_engine_skips_its_own_default_and_falls_back_to_another_engine() {
        let yaml = format!("{YAML}timeouts: {{ fallback_reserve_ms: 300 }}\n");
        // 四国めたん（voicevox）が間に合わないとき、同じ voicevox のデフォルト（ずんだもん）に残りの時間を使わず、
        // 別のエンジンの ebyroid:kiritan で読む
        let h = slow_hub(&yaml, &[("2", Some(5000)), ("3", Some(10)), ("kiritan", Some(10))]);
        let out = h.synthesize("あ", "voicevox:四国めたん", true, None, after_ms(700)).await.ok().unwrap();
        assert_eq!(out.voice.address, "ebyroid:kiritan");
    }

    #[tokio::test]
    async fn unknown_voice_still_falls_back_to_the_engine_default() {
        let h = slow_hub(YAML, &[("3", Some(10)), ("kiritan", Some(10))]);
        let out = h.synthesize("あ", "voicevox:だれか", true, None, after_ms(700)).await.ok().unwrap();
        assert_eq!(out.voice.address, "voicevox:ずんだもん/ノーマル");
    }

    #[tokio::test]
    async fn abandoned_synthesis_keeps_the_slot_until_the_engine_finishes() {
        let yaml = YAML.replace("backends: [http://b]", "backends: [http://b], max_concurrency_per_backend: 1");
        // 四国めたんは 500ms かかるが、利用者は 150ms しか待てない
        let h = slow_hub(&yaml, &[("2", Some(500)), ("3", Some(10)), ("kiritan", Some(10))]);
        let r = h.synthesize("あ", "voicevox:四国めたん", false, None, after_ms(150)).await;
        assert!(r.is_err());
        // 打ち切った後もエンジンは計算しているので、枠は埋まったまま（裏のタスクが打ち切りに気づくのを待つ）
        tokio::time::sleep(Duration::from_millis(20)).await;
        let st = &h.engines["voicevox"].status()[0];
        assert_eq!((st.active, st.overrun), (1, 1));

        // 期限のある合成は、計算中のエンジンで待たずに別のエンジンへ回る
        let started = Instant::now();
        let out = h.synthesize("あ", "voicevox:ずんだもん", true, None, after_ms(1000)).await.ok().unwrap();
        assert_eq!(out.voice.address, "ebyroid:kiritan");
        assert!(started.elapsed() < Duration::from_millis(200), "{:?}", started.elapsed());

        // エンジンが終えたら枠を返す
        tokio::time::sleep(Duration::from_millis(500)).await;
        let st = &h.engines["voicevox"].status()[0];
        assert_eq!((st.active, st.overrun), (0, 0));
        let out = h.synthesize("あ", "voicevox:ずんだもん", true, None, after_ms(1000)).await.ok().unwrap();
        assert_eq!(out.voice.address, "voicevox:ずんだもん/ノーマル");
    }

    #[tokio::test]
    async fn timed_out_synthesis_keeps_the_slot_past_response_start_ms() {
        let yaml = YAML.replace(
            "backends: [http://b]",
            "backends: [http://b], max_concurrency_per_backend: 1, response_start_ms: 100",
        );
        // 四国めたんは 600ms かかるが、エンジンの上限（response_start_ms）は 100ms
        let h = slow_hub(&yaml, &[("2", Some(600)), ("3", Some(10)), ("kiritan", Some(10))]);
        let started = Instant::now();
        let r = h.synthesize("あ", "voicevox:四国めたん", false, None, None).await;
        assert!(r.is_err());
        assert!(started.elapsed() < Duration::from_millis(300), "{:?}", started.elapsed());

        // 上限を過ぎても、エンジンが終えるまで枠を返さない
        tokio::time::sleep(Duration::from_millis(250)).await;
        let st = &h.engines["voicevox"].status()[0];
        assert_eq!((st.active, st.overrun), (1, 1));
        assert!(st.overrun_oldest_ms.is_some_and(|ms| ms >= 200), "{:?}", st.overrun_oldest_ms);
        let out = h.synthesize("あ", "voicevox:ずんだもん", true, None, after_ms(1000)).await.ok().unwrap();
        assert_eq!(out.voice.address, "ebyroid:kiritan");

        tokio::time::sleep(Duration::from_millis(400)).await;
        let st = &h.engines["voicevox"].status()[0];
        assert_eq!((st.active, st.overrun, st.overrun_oldest_ms), (0, 0, None));
    }

    #[tokio::test]
    async fn short_deadline_still_tries_the_requested_voice() {
        // 期限（400ms）が残しておく時間（既定 2 秒）より短くても、要求された声を先に試す
        let h = slow_hub(YAML, &[("108", Some(50)), ("kiritan", Some(10))]);
        let out = h.synthesize("あ", "voicevox:東北きりたん", true, None, after_ms(400)).await.ok().unwrap();
        assert_eq!(out.voice.address, "voicevox:東北きりたん/ノーマル");
    }

    #[tokio::test]
    async fn deadline_gives_up_when_no_candidate_answers_in_time() {
        let h = slow_hub(YAML, &[("108", Some(5000)), ("kiritan", Some(5000)), ("3", Some(5000))]);
        let started = Instant::now();
        let r = h.synthesize("あ", "voicevox:東北きりたん", true, None, after_ms(300)).await;
        assert!(matches!(r, Err(HubError::Unavailable(_))));
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn deadline_bounds_the_queue_wait() {
        let yaml = YAML.replace("backends: [http://b]", "backends: [http://b], max_concurrency_per_backend: 1");
        // ずんだもんは応答したまま送り終えないので、枠をふさぎ続ける
        let h = slow_hub(&yaml, &[("3", None)]);
        let _busy = h.synthesize("あ", "voicevox:ずんだもん", false, None, None).await.ok().unwrap();
        let started = Instant::now();
        let r = h.synthesize("あ", "voicevox:ずんだもん", false, None, after_ms(200)).await;
        assert!(r.is_err());
        // 空き待ちの上限（既定 10 秒）ではなく、期限で打ち切る
        assert!(started.elapsed() < Duration::from_millis(400), "{:?}", started.elapsed());
    }

    fn addresses(list: &[Voice]) -> Vec<&str> {
        list.iter().map(|v| v.address.as_str()).collect()
    }

    #[test]
    fn presets_expand_through_aliases_and_caller_params_win() {
        let h = hub(YAML);
        let r = h.expand("preset:早口ずんだもん?speed=1.1", None).unwrap();
        assert_eq!(r.voice.unwrap().address, "voicevox:ずんだもん/ノーマル");
        let (p, _) = params::resolve(&r.params);
        assert_eq!(p.speed, Some(1.1));
        assert_eq!(p.pitch, Some(0.1));
    }

    #[test]
    fn default_uses_client_then_global() {
        let h = hub(YAML);
        assert_eq!(h.expand("default", None).unwrap().voice.unwrap().address, "ebyroid:kiritan");
        let c = ClientCtx { name: "st".into(), default_voice: Some("voicevox:四国めたん".into()), allow_billable_fallback: false };
        assert_eq!(h.expand("default", Some(&c)).unwrap().voice.unwrap().address, "voicevox:四国めたん/ノーマル");
    }

    #[test]
    fn fallback_chain_follows_design_order() {
        let h = hub(YAML);
        let req = h.expand("voicevox:東北きりたん", None).unwrap();
        assert_eq!(
            addresses(&h.candidates(&req, None, true)),
            vec!["voicevox:東北きりたん/ノーマル", "ebyroid:kiritan", "voicevox:ずんだもん/ノーマル"]
        );
        assert_eq!(addresses(&h.candidates(&req, None, false)), vec!["voicevox:東北きりたん/ノーマル"]);
    }

    #[test]
    fn equivalents_match_by_character_and_keep_the_style() {
        let h = hub(&YAML.replace(
            "  - [ebyroid:kiritan, voicevox:東北きりたん]",
            "  - [ebyroid:kiritan, voicevox:東北きりたん]\n  - [cloud:alloy, voicevox:四国めたん]",
        ));
        h.catalog.set(
            "cloud",
            h.build(
                "cloud",
                vec![
                    VoiceEntry { internal_id: "a".into(), character: "alloy".into(), style: Some("default".into()), is_default: false },
                    VoiceEntry { internal_id: "b".into(), character: "alloy".into(), style: Some("ノーマル".into()), is_default: false },
                ],
            ),
        );
        // 設定ではスタイルを省略していても、別のスタイルを要求したときに対応が効き、同じスタイルへ移る
        let req = h.expand("cloud:alloy/ノーマル", None).unwrap();
        let list = h.candidates(&req, None, true);
        assert_eq!(addresses(&list)[..2], ["cloud:alloy/ノーマル", "voicevox:四国めたん/ノーマル"]);
    }

    #[test]
    fn unknown_voice_falls_back_to_defaults() {
        let h = hub(YAML);
        let req = h.expand("voicevox:だれか", None).unwrap();
        assert!(req.voice.is_none());
        assert_eq!(addresses(&h.candidates(&req, None, true)), vec!["voicevox:ずんだもん/ノーマル", "ebyroid:kiritan"]);
        assert!(h.candidates(&req, None, false).is_empty());
    }

    #[test]
    fn billable_engines_are_only_used_when_requested_or_allowed() {
        let h = hub(&YAML.replace("default_voice: ebyroid:kiritan", "default_voice: cloud:alloy"));
        let req = h.expand("voicevox:四国めたん", None).unwrap();
        assert_eq!(addresses(&h.candidates(&req, None, true)), vec!["voicevox:四国めたん/ノーマル", "voicevox:ずんだもん/ノーマル"]);
        let c = ClientCtx { name: "x".into(), default_voice: None, allow_billable_fallback: true };
        assert!(addresses(&h.candidates(&req, Some(&c), true)).contains(&"cloud:alloy/default"));
        let req = h.expand("cloud:alloy", None).unwrap();
        assert_eq!(addresses(&h.candidates(&req, None, true))[0], "cloud:alloy/default");
    }

    #[test]
    fn resolve_returns_canonical_address_for_aliases_but_keeps_presets() {
        let h = hub(&YAML.replace("zundamon: voicevox:ずんだもん", "zundamon: voicevox:ずんだもん\n  きりたん: ebyroid:kiritan"));
        let (r, v) = h.resolve("きりたん");
        assert_eq!(r.matched.as_deref(), Some("ebyroid:kiritan"));
        assert_eq!(v.unwrap().address, "ebyroid:kiritan");
        let (r, v) = h.resolve("早口ずんだもん");
        assert_eq!(r.matched.as_deref(), Some("preset:早口ずんだもん"));
        assert_eq!(v.unwrap().address, "voicevox:ずんだもん/ノーマル");
    }

    #[test]
    fn alias_loops_are_rejected() {
        let h = hub(&YAML.replace("zundamon: voicevox:ずんだもん", "zundamon: zundamon"));
        assert!(matches!(h.expand("zundamon", None), Err(HubError::BadAddress(_))));
    }
}
