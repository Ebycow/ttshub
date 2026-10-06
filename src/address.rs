//! アドレスの文法と名前の正規化

use unicode_normalization::UnicodeNormalization;

/// アドレスの指す先
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `default`
    Default,
    /// `preset:<名前>`
    Preset(String),
    /// `<エンジン>:<キャラ>[/<スタイル>]`。speaker は正規化前の文字列
    Voice { engine: String, speaker: String },
    /// エンジン名のない文字列。別名として解決する
    Bare(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub target: Target,
    /// `?` 以降のパラメータ（書かれた順）
    pub params: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AddressError {
    #[error("アドレスの話者名が空")]
    EmptySpeaker,
}

pub fn parse(input: &str) -> Result<Address, AddressError> {
    let input = input.trim();
    let (head, query) = match input.split_once('?') {
        Some((h, q)) => (h.trim(), q),
        None => (input, ""),
    };
    let params = form_urlencoded::parse(query.as_bytes()).into_owned().collect();

    let target = if head.is_empty() || head == "default" {
        Target::Default
    } else if let Some(name) = head.strip_prefix("preset:") {
        if name.trim().is_empty() {
            return Err(AddressError::EmptySpeaker);
        }
        Target::Preset(name.trim().to_owned())
    } else if let Some((engine, speaker)) = head.split_once(':') {
        let speaker = speaker.trim();
        if speaker.is_empty() {
            return Err(AddressError::EmptySpeaker);
        }
        Target::Voice { engine: engine.trim().to_owned(), speaker: speaker.to_owned() }
    } else {
        Target::Bare(head.to_owned())
    };
    Ok(Address { target, params })
}

/// アドレスに使う名前に正規化する
///
/// NFKC → 英字は小文字 → `/` は `-` → 空白と記号を削除（`-` `_` `.` は残す）
pub fn slug(name: &str) -> String {
    name.nfkc()
        .flat_map(char::to_lowercase)
        .filter_map(|c| match c {
            '/' => Some('-'),
            '-' | '_' | '.' => Some(c),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .collect()
}

/// あいまい照合用に、区切りの違いとカタカナ・ひらがなの違いを無視した形にする
pub fn compact(name: &str) -> String {
    slug(name)
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | '.'))
        .map(|c| match c {
            'ァ'..='ヶ' => char::from_u32(c as u32 - 0x60).unwrap_or(c),
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_follows_design_examples() {
        assert_eq!(slug("小夜/SAYO"), "小夜-sayo");
        assert_eq!(slug("†聖騎士 紅桜†"), "聖騎士紅桜");
        assert_eq!(slug("ナースロボ＿タイプＴ"), "ナースロボ_タイプt");
        assert_eq!(slug("セクシー／あん子"), "セクシー-あん子");
        assert_eq!(slug("No.7"), "no.7");
        assert_eq!(slug("人間（怒り）ver."), "人間怒りver.");
    }

    #[test]
    fn compact_ignores_separators_and_kana_type() {
        assert_eq!(compact("ずんだもん/ツンツン"), "ずんだもんつんつん");
        assert_eq!(compact("No.7"), "no7");
    }

    #[test]
    fn parses_voice_with_params() {
        let a = parse("voicevox:ずんだもん/あまあま?speed=1.2&pitch=0.05").unwrap();
        assert_eq!(a.target, Target::Voice { engine: "voicevox".into(), speaker: "ずんだもん/あまあま".into() });
        assert_eq!(a.params, vec![("speed".into(), "1.2".into()), ("pitch".into(), "0.05".into())]);
    }

    #[test]
    fn parses_other_targets() {
        assert_eq!(parse("default").unwrap().target, Target::Default);
        assert_eq!(parse("").unwrap().target, Target::Default);
        assert_eq!(parse("?speed=1.5").unwrap().params, vec![("speed".into(), "1.5".into())]);
        assert_eq!(parse("preset:早口ずんだもん").unwrap().target, Target::Preset("早口ずんだもん".into()));
        assert_eq!(parse("zundamon").unwrap().target, Target::Bare("zundamon".into()));
        assert_eq!(parse("voicevox:"), Err(AddressError::EmptySpeaker));
    }
}
