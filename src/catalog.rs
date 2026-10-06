//! カタログ: 全エンジンの話者一覧の集約・検索・あいまい照合

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::Serialize;

use unicode_normalization::UnicodeNormalization;

use crate::address::{compact, slug};
use crate::config::EngineConfig;
use crate::engine::VoiceEntry;

/// カタログ上の 1 つの話者
#[derive(Debug, Clone, Serialize)]
pub struct Voice {
    pub address: String,
    pub engine: String,
    pub character: String,
    pub style: Option<String>,
    pub display_name: String,
    pub credit: Option<String>,
    pub terms_url: Option<String>,
    pub billable: bool,
    #[serde(skip)]
    pub entry: VoiceEntry,
    #[serde(skip)]
    char_slug: String,
    /// そのキャラの既定のスタイルか（スタイルを省略したときに選ばれる）
    #[serde(skip)]
    char_default: bool,
}

/// 1 つの論理エンジンの話者一覧
#[derive(Debug, Default)]
pub struct EngineVoices {
    voices: Vec<Voice>,
    by_key: HashMap<(String, String), usize>,
    char_default: HashMap<String, usize>,
    default: Option<usize>,
}

impl EngineVoices {
    /// エンジンが申告した一覧から作る。正規化した名前が重複したものは両方とも除き、警告する
    pub fn build(engine: &str, config: &EngineConfig, entries: Vec<VoiceEntry>) -> Self {
        let mut count: HashMap<(String, String), usize> = HashMap::new();
        for e in &entries {
            *count.entry(key_of(e)).or_default() += 1;
        }
        let mut ev = EngineVoices::default();
        for e in entries {
            let key = key_of(&e);
            if count[&key] > 1 {
                tracing::warn!(engine, character = %e.character, style = ?e.style, "正規化した名前が重複するため除外");
                continue;
            }
            let char_slug = key.0.clone();
            let address = match &e.style {
                Some(_) => format!("{engine}:{}/{}", key.0, key.1),
                None => format!("{engine}:{}", key.0),
            };
            let display_name = match &e.style {
                Some(style) => format!("{}（{style}）", e.character),
                None => e.character.clone(),
            };
            let credit = config.credit_template.as_ref().map(|t| {
                t.replace("{character}", &e.character).replace("{style}", e.style.as_deref().unwrap_or(""))
            });
            let idx = ev.voices.len();
            let char_default = !ev.char_default.contains_key(&char_slug);
            if char_default {
                ev.char_default.insert(char_slug.clone(), idx);
            }
            if e.is_default && ev.default.is_none() {
                ev.default = Some(idx);
            }
            ev.by_key.insert(key, idx);
            ev.voices.push(Voice {
                address,
                engine: engine.to_owned(),
                character: e.character.clone(),
                style: e.style.clone(),
                display_name,
                credit,
                terms_url: config.terms_url.clone(),
                billable: config.billable,
                entry: e,
                char_slug,
                char_default,
            });
        }
        // 設定のデフォルト話者 → エンジンの申告 → 先頭
        if let Some(found) = config.default_voice.as_deref().and_then(|s| ev.lookup_index(s)) {
            ev.default = Some(found);
        }
        if ev.default.is_none() && !ev.voices.is_empty() {
            ev.default = Some(0);
        }
        ev
    }

    /// `キャラ[/スタイル]` から話者を探す。スタイルを省略したらそのキャラの先頭のスタイル
    fn lookup_index(&self, speaker: &str) -> Option<usize> {
        if let Some((c, s)) = speaker.split_once('/') {
            if let Some(&i) = self.by_key.get(&(slug(c), slug(s))) {
                return Some(i);
            }
        }
        // 名前そのものに / を含むキャラ（小夜/SAYO など）
        self.char_default.get(&slug(speaker)).copied()
    }

    pub fn len(&self) -> usize {
        self.voices.len()
    }
}

fn key_of(e: &VoiceEntry) -> (String, String) {
    (slug(&e.character), e.style.as_deref().map(slug).unwrap_or_default())
}

/// 全論理エンジンのカタログ
#[derive(Default)]
pub struct Catalog {
    engines: RwLock<HashMap<String, Arc<EngineVoices>>>,
}

/// あいまい照合の結果
#[derive(Debug, Serialize)]
pub struct Resolution {
    #[serde(rename = "match")]
    pub matched: Option<String>,
    pub confidence: f64,
    pub suggestions: Vec<String>,
}

/// 照合の候補（話者、別名、プリセット）
struct Candidate<'a> {
    address: &'a str,
    /// 比較に使う形（compact 済み）と、それがキャラ名だけの形か
    forms: Vec<(String, bool)>,
    /// キャラ名とスタイル名を分けて照合するための形（compact 済み）
    parts: Option<(String, String)>,
    group: String,
    char_default: bool,
}

const MATCH_THRESHOLD: f64 = 0.8;
const AMBIGUITY_MARGIN: f64 = 0.05;

impl Catalog {
    pub fn set(&self, engine: &str, voices: EngineVoices) {
        self.engines.write().unwrap().insert(engine.to_owned(), Arc::new(voices));
    }

    pub fn engine(&self, engine: &str) -> Option<Arc<EngineVoices>> {
        self.engines.read().unwrap().get(engine).cloned()
    }

    pub fn lookup(&self, engine: &str, speaker: &str) -> Option<Voice> {
        let ev = self.engine(engine)?;
        ev.lookup_index(speaker).map(|i| ev.voices[i].clone())
    }

    pub fn engine_default(&self, engine: &str) -> Option<Voice> {
        let ev = self.engine(engine)?;
        ev.default.map(|i| ev.voices[i].clone())
    }

    /// 全話者を論理エンジンの記述順に並べる
    pub fn all(&self, order: &[String]) -> Vec<Voice> {
        let engines = self.engines.read().unwrap();
        order.iter().filter_map(|e| engines.get(e)).flat_map(|ev| ev.voices.iter().cloned()).collect()
    }

    /// 検索。q が空なら全件。別名とプリセットも候補に含める
    pub fn search(
        &self,
        order: &[String],
        q: &str,
        engine: Option<&str>,
        extras: &[Named],
    ) -> Vec<SearchHit> {
        let voices: Vec<Voice> =
            self.all(order).into_iter().filter(|v| engine.is_none_or(|e| v.engine == e)).collect();
        let q = Query::new(q);
        if q.full.is_empty() {
            let mut hits: Vec<SearchHit> = voices.into_iter().map(SearchHit::Voice).collect();
            if engine.is_none() {
                hits.extend(extras.iter().map(|n| SearchHit::Named { name: n.name.clone(), target: n.target.clone() }));
            }
            return hits;
        }
        let cands = candidates(&voices, extras);
        let mut scored: Vec<(f64, bool, usize)> = cands
            .iter()
            .enumerate()
            .map(|(i, c)| (score(&q, c), c.char_default, i))
            .filter(|(s, _, _)| *s >= 0.5)
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        scored
            .into_iter()
            .map(|(_, _, i)| {
                if i < voices.len() {
                    SearchHit::Voice(voices[i].clone())
                } else {
                    let n = &extras[i - voices.len()];
                    SearchHit::Named { name: n.name.clone(), target: n.target.clone() }
                }
            })
            .filter(|h| engine.is_none() || matches!(h, SearchHit::Voice(_)))
            .collect()
    }

    /// あいまい照合
    pub fn resolve(&self, order: &[String], q: &str, extras: &[Named]) -> Resolution {
        // 別名・プリセットの名前そのものなら、それで決まり
        if let Some(n) = extras.iter().find(|n| n.name == q.trim()) {
            return Resolution { matched: Some(n.name.clone()), confidence: 1.0, suggestions: vec![] };
        }
        // エンジン名付きなら、そのエンジンに絞って照合する
        let (engine, body) = match q.split_once(':') {
            Some((e, rest)) if order.iter().any(|o| o == e.trim()) => (Some(e.trim()), rest),
            _ => (None, q),
        };
        if let Some(e) = engine {
            if let Some(v) = self.lookup(e, body.trim()) {
                return Resolution { matched: Some(v.address), confidence: 1.0, suggestions: vec![] };
            }
        }
        let voices: Vec<Voice> =
            self.all(order).into_iter().filter(|v| engine.is_none_or(|e| v.engine == e)).collect();
        let extras: &[Named] = if engine.is_some() { &[] } else { extras };
        let cands = candidates(&voices, extras);
        let q = Query::new(body);
        let mut scored: Vec<(f64, &Candidate)> = cands.iter().map(|c| (score(&q, c), c)).collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.char_default.cmp(&a.1.char_default)));

        let suggestions: Vec<String> = scored
            .iter()
            .filter(|(s, _)| *s >= 0.4)
            .take(5)
            .map(|(_, c)| c.address.to_owned())
            .collect();
        let Some(&(top, best)) = scored.first() else {
            return Resolution { matched: None, confidence: 0.0, suggestions };
        };
        // 同じキャラの別スタイルは曖昧とみなさない（既定のスタイルが先に並ぶ）
        let rival = scored.iter().skip(1).find(|(_, c)| c.group != best.group).map(|(s, _)| *s).unwrap_or(0.0);
        let matched = (top >= MATCH_THRESHOLD && top - rival >= AMBIGUITY_MARGIN).then(|| best.address.to_owned());
        Resolution { matched, confidence: (top * 1000.0).round() / 1000.0, suggestions }
    }
}

/// 別名またはプリセット
#[derive(Debug, Clone)]
pub struct Named {
    /// 別名、または `preset:` を付けたプリセット名
    pub name: String,
    /// 指す先のアドレス
    pub target: String,
    /// 指す先の話者のキャラ（`エンジン:キャラ`）。同じキャラの話者と曖昧とみなさないために使う
    pub group: Option<String>,
}

/// 検索結果の 1 件
pub enum SearchHit {
    Voice(Voice),
    /// 別名またはプリセット（名前, 指す先）
    Named { name: String, target: String },
}

fn candidates<'a>(voices: &'a [Voice], extras: &'a [Named]) -> Vec<Candidate<'a>> {
    let mut out: Vec<Candidate> = voices
        .iter()
        .map(|v| {
            let c = compact(&v.character);
            let mut forms = vec![(c.clone(), true)];
            let mut parts = None;
            if let Some(style) = &v.style {
                let st = compact(style);
                forms.push((format!("{c}{st}"), false));
                parts = Some((c, st));
            }
            Candidate {
                address: &v.address,
                forms,
                parts,
                group: format!("{}:{}", v.engine, v.char_slug),
                char_default: v.char_default,
            }
        })
        .collect();
    out.extend(extras.iter().map(|n| Candidate {
        address: &n.name,
        forms: vec![(compact(n.name.strip_prefix("preset:").unwrap_or(&n.name)), false)],
        parts: None,
        group: n.group.clone().unwrap_or_else(|| n.name.clone()),
        char_default: true,
    }));
    out
}

/// 照合する文字列。空白か `/` で区切られていれば、最後の語をスタイル名とみなした照合もする
struct Query {
    full: String,
    parts: Option<(String, String)>,
}

impl Query {
    fn new(q: &str) -> Self {
        let words: Vec<String> = q
            .nfkc()
            .collect::<String>()
            .split(|c: char| c.is_whitespace() || c == '/')
            .map(compact)
            .filter(|w| !w.is_empty())
            .collect();
        let parts = (words.len() >= 2).then(|| (words[..words.len() - 1].concat(), words[words.len() - 1].clone()));
        Self { full: words.concat(), parts }
    }
}

fn score(q: &Query, c: &Candidate) -> f64 {
    let whole = c
        .forms
        .iter()
        .map(|(form, char_only)| {
            let s = similarity(&q.full, form);
            // キャラ名だけで当たったときは、既定のスタイルをわずかに優先する
            if *char_only && !c.char_default {
                s * 0.99
            } else {
                s
            }
        })
        .fold(0.0, f64::max);
    let split = match (&q.parts, &c.parts) {
        (Some((qc, qs)), Some((cc, cs))) => (similarity(qc, cc) + similarity(qs, cs)) / 2.0,
        _ => 0.0,
    };
    whole.max(split)
}

/// 0.0〜1.0。編集距離による類似度と、部分一致を考慮する
fn similarity(q: &str, form: &str) -> f64 {
    if q.is_empty() || form.is_empty() {
        return 0.0;
    }
    let ql = q.chars().count();
    let fl = form.chars().count();
    let mut s = 1.0 - levenshtein(q, form) as f64 / ql.max(fl) as f64;
    // 一部分だけ書いた場合（「めたん」→「四国めたん」）。先頭が合うほうを少し優先する
    if ql >= 2 && form.contains(q) {
        let bonus = if form.starts_with(q) { 0.05 } else { 0.0 };
        s = s.max(0.7 + bonus + 0.25 * ql as f64 / fl as f64);
    }
    s
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(credit: Option<&str>, default_voice: Option<&str>) -> EngineConfig {
        serde_yaml::from_str::<EngineConfig>(&format!(
            "plugin: voicevox-compatible\nbackends: [http://x]\n{}{}",
            credit.map(|c| format!("credit_template: \"{c}\"\n")).unwrap_or_default(),
            default_voice.map(|d| format!("default_voice: \"{d}\"\n")).unwrap_or_default(),
        ))
        .unwrap()
    }

    fn vv(id: &str, character: &str, style: &str) -> VoiceEntry {
        VoiceEntry { internal_id: id.into(), character: character.into(), style: Some(style.into()), is_default: false }
    }

    fn catalog() -> (Catalog, Vec<String>) {
        let cat = Catalog::default();
        cat.set(
            "voicevox",
            EngineVoices::build(
                "voicevox",
                &config(Some("VOICEVOX:{character}"), None),
                vec![
                    vv("2", "四国めたん", "ノーマル"),
                    vv("3", "ずんだもん", "ノーマル"),
                    vv("1", "ずんだもん", "あまあま"),
                    vv("7", "ずんだもん", "ツンツン"),
                    vv("46", "小夜/SAYO", "ノーマル"),
                    vv("108", "東北きりたん", "ノーマル"),
                ],
            ),
        );
        let eby = |n: &str, d: bool| VoiceEntry { internal_id: n.into(), character: n.into(), style: None, is_default: d };
        cat.set(
            "ebyroid",
            EngineVoices::build(
                "ebyroid",
                &config(None, None),
                vec![eby("kiritan", true), eby("aoi", false), eby("akane", false)],
            ),
        );
        (cat, vec!["ebyroid".into(), "voicevox".into()])
    }

    #[test]
    fn builds_addresses_and_credits() {
        let (cat, _) = catalog();
        let v = cat.lookup("voicevox", "ずんだもん/あまあま").unwrap();
        assert_eq!(v.address, "voicevox:ずんだもん/あまあま");
        assert_eq!(v.display_name, "ずんだもん（あまあま）");
        assert_eq!(v.credit.as_deref(), Some("VOICEVOX:ずんだもん"));
        assert_eq!(v.entry.internal_id, "1");
        assert_eq!(cat.lookup("ebyroid", "kiritan").unwrap().address, "ebyroid:kiritan");
    }

    #[test]
    fn lookup_falls_back_to_first_style_and_handles_slash_names() {
        let (cat, _) = catalog();
        assert_eq!(cat.lookup("voicevox", "ずんだもん").unwrap().entry.internal_id, "3");
        assert_eq!(cat.lookup("voicevox", "小夜/SAYO").unwrap().entry.internal_id, "46");
        assert_eq!(cat.lookup("voicevox", "小夜-sayo/ノーマル").unwrap().entry.internal_id, "46");
        assert!(cat.lookup("voicevox", "ずんだもん/ささやき").is_none());
    }

    #[test]
    fn engine_default_prefers_config_then_engine_then_first() {
        let (cat, _) = catalog();
        assert_eq!(cat.engine_default("ebyroid").unwrap().address, "ebyroid:kiritan");
        assert_eq!(cat.engine_default("voicevox").unwrap().address, "voicevox:四国めたん/ノーマル");
        let ev = EngineVoices::build("voicevox", &config(None, Some("ずんだもん")), vec![vv("2", "四国めたん", "ノーマル"), vv("3", "ずんだもん", "ノーマル")]);
        assert_eq!(ev.voices[ev.default.unwrap()].address, "voicevox:ずんだもん/ノーマル");
    }

    #[test]
    fn duplicated_slugs_are_excluded() {
        let ev = EngineVoices::build("v", &config(None, None), vec![vv("1", "A B", "x"), vv("2", "AB", "x"), vv("3", "C", "x")]);
        assert_eq!(ev.len(), 1);
    }

    #[test]
    fn resolves_character_and_style() {
        let (cat, order) = catalog();
        let r = cat.resolve(&order, "ずんだもん あまあま", &[]);
        assert_eq!(r.matched.as_deref(), Some("voicevox:ずんだもん/あまあま"));
        let r = cat.resolve(&order, "ずんだもん", &[]);
        assert_eq!(r.matched.as_deref(), Some("voicevox:ずんだもん/ノーマル"));
        let r = cat.resolve(&order, "ずんだもｎ　つんつん", &[]);
        assert_eq!(r.matched.as_deref(), Some("voicevox:ずんだもん/ツンツン"));
        let r = cat.resolve(&order, "めたん ノーマル", &[]);
        assert_eq!(r.matched.as_deref(), Some("voicevox:四国めたん/ノーマル"));
        let r = cat.resolve(&order, "voicevox:ずんだもん/あまあま", &[]);
        assert_eq!(r.matched.as_deref(), Some("voicevox:ずんだもん/あまあま"));
        assert_eq!(r.confidence, 1.0);
    }

    #[test]
    fn ambiguous_or_unknown_queries_return_suggestions_only() {
        let (cat, order) = catalog();
        // 「もん」はずんだもんにもあんこもんにも含まれる
        cat.set("v2", EngineVoices::build("v2", &config(None, None), vec![vv("9", "あんこもん", "ノーマル")]));
        let order2 = vec!["ebyroid".to_string(), "voicevox".to_string(), "v2".to_string()];
        let r = cat.resolve(&order2, "もん", &[]);
        assert_eq!(r.matched, None);
        assert!(r.suggestions.contains(&"v2:あんこもん/ノーマル".to_string()));
        let r = cat.resolve(&order, "xyz", &[]);
        assert_eq!(r.matched, None);
    }

    #[test]
    fn resolves_aliases_and_presets() {
        let (cat, order) = catalog();
        let named = |name: &str, target: &str, group: &str| Named { name: name.into(), target: target.into(), group: Some(group.into()) };
        let extras = vec![
            named("zundamon", "voicevox:ずんだもん", "voicevox:ずんだもん"),
            named("preset:早口ずんだもん", "voicevox:ずんだもん?speed=1.4", "voicevox:ずんだもん"),
            named("kiritan", "ebyroid:kiritan", "ebyroid:kiritan"),
        ];
        assert_eq!(cat.resolve(&order, "zundamon", &extras).matched.as_deref(), Some("zundamon"));
        assert_eq!(cat.resolve(&order, "早口ずんだもん", &extras).matched.as_deref(), Some("preset:早口ずんだもん"));
        assert_eq!(cat.resolve(&order, "preset:早口ずんだもん", &extras).matched.as_deref(), Some("preset:早口ずんだもん"));
        // 別名と同じ話者は競合とみなさない
        assert_eq!(cat.resolve(&order, "kiritan", &extras).matched.as_deref(), Some("kiritan"));
    }

    #[test]
    fn search_ranks_prefix_matches() {
        let (cat, order) = catalog();
        let hits = cat.search(&order, "ずんだ", None, &[]);
        let first = match &hits[0] {
            SearchHit::Voice(v) => v.address.clone(),
            SearchHit::Named { name, .. } => name.clone(),
        };
        assert_eq!(first, "voicevox:ずんだもん/ノーマル");
        assert_eq!(hits.len(), 3);
        assert_eq!(cat.search(&order, "", Some("ebyroid"), &[]).len(), 3);
    }
}
