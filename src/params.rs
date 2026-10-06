//! 合成パラメータ

/// 解決済みのパラメータ。共通名は範囲内に丸めてある
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Params {
    pub speed: Option<f32>,
    pub pitch: Option<f32>,
    pub volume: Option<f32>,
    pub intonation: Option<f32>,
    /// `x-<プラグイン名>-<キー>` の形のエンジン固有パラメータ
    pub extra: Vec<(String, String)>,
}

impl Params {
    /// エンジン固有パラメータのうち、指定したプラグインのものを `(キー, 値)` で返す
    pub fn extra_for<'a>(&'a self, plugin_prefix: &'a str) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        self.extra.iter().filter_map(move |(k, v)| {
            k.strip_prefix("x-")
                .and_then(|rest| rest.strip_prefix(plugin_prefix))
                .and_then(|rest| rest.strip_prefix('-'))
                .map(|key| (key, v.as_str()))
        })
    }
}

const RANGES: [(&str, f32, f32); 4] =
    [("speed", 0.5, 2.0), ("pitch", -0.15, 0.15), ("volume", 0.0, 2.0), ("intonation", 0.0, 2.0)];

/// `(キー, 値)` の列からパラメータを作る。後に書いたものが優先される。
/// 範囲外の値は丸め、知らないキーや読めない値は無視して、それぞれ警告を返す
pub fn resolve(pairs: &[(String, String)]) -> (Params, Vec<String>) {
    let mut params = Params::default();
    let mut warnings = Vec::new();
    for (key, value) in pairs {
        if key.starts_with("x-") {
            params.extra.retain(|(k, _)| k != key);
            params.extra.push((key.clone(), value.clone()));
            continue;
        }
        let Some(&(_, min, max)) = RANGES.iter().find(|(name, _, _)| name == key) else {
            warnings.push(format!("unknown parameter {key} ignored"));
            continue;
        };
        let Ok(v) = value.parse::<f32>() else {
            warnings.push(format!("{key}={value} is not a number; ignored"));
            continue;
        };
        if !v.is_finite() {
            warnings.push(format!("{key}={value} is not a number; ignored"));
            continue;
        }
        let clamped = v.clamp(min, max);
        if clamped != v {
            warnings.push(format!("{key} clamped to {clamped}"));
        }
        let slot = match key.as_str() {
            "speed" => &mut params.speed,
            "pitch" => &mut params.pitch,
            "volume" => &mut params.volume,
            _ => &mut params.intonation,
        };
        *slot = Some(clamped);
    }
    (params, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn later_values_win_and_out_of_range_is_clamped() {
        let (p, w) = resolve(&pairs(&[("speed", "1.4"), ("speed", "3"), ("pitch", "0.05")]));
        assert_eq!(p.speed, Some(2.0));
        assert_eq!(p.pitch, Some(0.05));
        assert_eq!(w, vec!["speed clamped to 2"]);
    }

    #[test]
    fn unknown_and_invalid_values_are_ignored() {
        let (p, w) = resolve(&pairs(&[("foo", "1"), ("speed", "fast"), ("volume", "NaN")]));
        assert_eq!(p, Params::default());
        assert_eq!(w.len(), 3);
    }

    #[test]
    fn engine_specific_params_pass_through() {
        let (p, w) = resolve(&pairs(&[("x-voicevox-prePhonemeLength", "0.2"), ("x-sbv2-sdp_ratio", "0.4")]));
        assert!(w.is_empty());
        assert_eq!(p.extra_for("voicevox").collect::<Vec<_>>(), vec![("prePhonemeLength", "0.2")]);
    }
}
