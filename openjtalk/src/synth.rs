//! open_jtalk コマンドを呼んで合成する
//!
//! open_jtalk は入力の 1 行目しか読まず、1 行も 1023 バイトで切り捨てるので、文に分けて 1 文ずつ合成し、
//! できた文から順にヘッダなしの PCM（16bit モノラル）で返す

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

/// open_jtalk に 1 回で渡す上限。open_jtalk は 1 行を 1023 バイトで切るので、余裕を持たせる
const MAX_PIECE_BYTES: usize = 600;
/// 声のファイルを探す深さ
const MAX_SCAN_DEPTH: usize = 3;

/// 受け付ける open_jtalk のオプションと、値の範囲。
/// サンプリング周波数（-s）やフレーム周期（-p）は PCM の形式が変わるので受け付けない
pub const OPTIONS: [(&str, f64, f64); 8] = [
    ("a", 0.0, 1.0),     // オールパス定数（声の太さ）
    ("b", 0.0, 1.0),     // ポストフィルタ係数
    ("r", 0.1, 10.0),    // 話速
    ("fm", -24.0, 24.0), // 音高（半音単位で足す）
    ("u", 0.0, 1.0),     // 有声/無声の閾値
    ("jm", 0.0, 10.0),   // スペクトルの GV の重み
    ("jf", 0.0, 10.0),   // 対数 F0 の GV の重み
    ("g", -100.0, 20.0), // 音量（dB）
];

pub type PcmStream = BoxStream<'static, Result<Bytes, std::io::Error>>;

/// 話者。id は声のディレクトリからの相対パス（拡張子なし）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Voice {
    pub id: String,
    pub character: String,
    pub style: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SynthError {
    /// 知らない声
    UnknownVoice(String),
    /// 読む文がない
    EmptyText,
    /// open_jtalk が動かない、失敗したなど
    Failed(String),
}

/// open_jtalk コマンドと、辞書・声の場所
pub struct OpenJTalk {
    bin: PathBuf,
    dic: PathBuf,
    voice_dir: PathBuf,
}

impl OpenJTalk {
    /// 環境変数 `OPENJTALK_BIN`、`OPENJTALK_DIC`、`OPENJTALK_VOICE_DIR` から。既定はイメージの中の場所
    pub fn from_env() -> Self {
        let env = |key: &str, default: &str| PathBuf::from(std::env::var(key).unwrap_or_else(|_| default.into()));
        Self::new(
            env("OPENJTALK_BIN", "open_jtalk"),
            env("OPENJTALK_DIC", "/var/lib/mecab/dic/open-jtalk/naist-jdic"),
            env("OPENJTALK_VOICE_DIR", "/usr/share/hts-voice"),
        )
    }

    pub fn new(bin: PathBuf, dic: PathBuf, voice_dir: PathBuf) -> Self {
        Self { bin, dic, voice_dir }
    }

    /// 声のディレクトリにある日本語の声。キャラ名、スタイル名（normal が先頭）の順
    pub fn voices(&self) -> Vec<Voice> {
        scan_voices(&self.voice_dir)
    }

    /// open_jtalk と辞書と声がそろっているか
    pub fn healthy(&self) -> bool {
        self.dic.join("sys.dic").is_file() && find_command(&self.bin) && !self.voices().is_empty()
    }

    /// 合成して、サンプリング周波数と PCM のストリームを返す。
    /// 最初の文は返す前に合成し、失敗したらエラーで返す（ttshub が代わりの声に回せるように）
    pub async fn synthesize(
        self: &Arc<Self>,
        voice_id: &str,
        text: &str,
        options: &HashMap<String, f64>,
    ) -> Result<(u32, PcmStream), SynthError> {
        // 一覧にある声だけを受け付ける（声のディレクトリの外を指せないように）
        if !self.voices().iter().any(|v| v.id == voice_id) {
            return Err(SynthError::UnknownVoice(voice_id.to_owned()));
        }
        let voice_path = self.voice_dir.join(format!("{voice_id}.htsvoice"));
        let sample_rate = read_header(&voice_path)
            .ok()
            .and_then(|h| h.get("SAMPLING_FREQUENCY").and_then(|v| v.parse().ok()))
            .ok_or_else(|| SynthError::Failed(format!("声 {} を読めない", voice_path.display())))?;
        let args = to_args(&clamp_options(options));
        let pieces = split_text(text);
        if pieces.is_empty() {
            return Err(SynthError::EmptyText);
        }

        let first =
            self.synthesize_piece(&voice_path, sample_rate, &pieces[0], &args).await.map_err(SynthError::Failed)?;
        let rest = futures::stream::unfold(
            (self.clone(), voice_path, pieces.into_iter().skip(1), args),
            move |(this, voice_path, mut pieces, args)| async move {
                let piece = pieces.next()?;
                let result = this
                    .synthesize_piece(&voice_path, sample_rate, &piece, &args)
                    .await
                    .map(Bytes::from)
                    .map_err(std::io::Error::other);
                Some((result, (this, voice_path, pieces, args)))
            },
        );
        let stream = futures::stream::once(async move { Ok(Bytes::from(first)) }).chain(rest);
        Ok((sample_rate, Box::pin(stream)))
    }

    /// 1 文を合成して PCM を返す。読める文字がない文は空の PCM になる
    async fn synthesize_piece(
        &self,
        voice: &Path,
        sample_rate: u32,
        text: &str,
        args: &[String],
    ) -> Result<Vec<u8>, String> {
        let mut child = tokio::process::Command::new(&self.bin)
            .arg("-x")
            .arg(&self.dic)
            .arg("-m")
            .arg(voice)
            .args(args)
            .args(["-ow", "/dev/stdout"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 利用者が切断して音声が捨てられたら、合成も止める
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("{} を起動できない: {e}", self.bin.display()))?;
        let mut stdin = child.stdin.take().expect("stdin は piped");
        let input = format!("{text}\n");
        let write = async move {
            // 書き終える前に open_jtalk が終わっても（読めない文字で止まったときなど）、終了の状態で判断する
            let _ = stdin.write_all(input.as_bytes()).await;
        };
        let (_, output) = tokio::join!(write, child.wait_with_output());
        let output = output.map_err(|e| format!("open_jtalk の出力を読めない: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // 記号だけの文など、読める文字がないと open_jtalk は失敗する。無音として扱う
            if stderr.contains("cannot be synthesized") {
                return Ok(Vec::new());
            }
            let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
            return Err(format!("open_jtalk が失敗した（{}）: {last}", output.status));
        }
        let wav = parse_wav(&output.stdout)?;
        if wav.sample_rate != sample_rate || wav.channels != 1 || wav.bits != 16 {
            return Err(format!(
                "open_jtalk の出力が想定と違う: {}Hz {}ch {}bit（{sample_rate}Hz モノラル 16bit のはず）",
                wav.sample_rate, wav.channels, wav.bits
            ));
        }
        Ok(output.stdout[wav.data].to_vec())
    }
}

/// コマンドがあるか。パスで書いてあればそのファイル、名前だけなら PATH から探す
fn find_command(bin: &Path) -> bool {
    if bin.components().count() > 1 {
        return bin.is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
}

/// 受け付けるオプションだけを残し、範囲に丸める。知らないオプションと数でない値は捨てる
fn clamp_options(options: &HashMap<String, f64>) -> BTreeMap<&'static str, f64> {
    OPTIONS
        .iter()
        .filter_map(|&(name, min, max)| {
            let v = *options.get(name)?;
            v.is_finite().then(|| (name, v.clamp(min, max)))
        })
        .collect()
}

/// `{"r": 1.2}` → `["-r", "1.2"]`
fn to_args(options: &BTreeMap<&str, f64>) -> Vec<String> {
    options.iter().flat_map(|(k, v)| [format!("-{k}"), v.to_string()]).collect()
}

/// 文の終わり
fn is_terminator(c: char) -> bool {
    matches!(c, '。' | '．' | '！' | '？' | '!' | '?' | '♪')
}

fn is_opener(c: char) -> bool {
    matches!(c, '「' | '『' | '（' | '(' | '【' | '〈' | '《')
}

fn is_closer(c: char) -> bool {
    matches!(c, '」' | '』' | '）' | ')' | '】' | '〉' | '》')
}

/// 文に分ける。括弧の中では切らない。改行でも区切り、長すぎる文は読点で、それでも長ければ文字の境目で切る
fn split_text(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut ended = false;
    let mut depth = 0usize;
    for c in text.chars() {
        if c == '\n' || c == '\r' {
            sentences.push(std::mem::take(&mut current));
            ended = false;
            depth = 0;
            continue;
        }
        if ended && !is_terminator(c) && !is_closer(c) {
            sentences.push(std::mem::take(&mut current));
            ended = false;
        }
        current.push(if c.is_control() { ' ' } else { c });
        if is_opener(c) {
            depth += 1;
        } else if is_closer(c) {
            depth = depth.saturating_sub(1);
        } else if is_terminator(c) && depth == 0 {
            ended = true;
        }
    }
    sentences.push(current);
    sentences.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).flat_map(|s| split_long(s, MAX_PIECE_BYTES)).collect()
}

/// `max` バイトを超える文を、読点などの後ろで切る。区切りがなければ文字の境目で切る
fn split_long(sentence: &str, max: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = sentence;
    while rest.len() > max {
        let mut cut = None;
        for (i, c) in rest.char_indices() {
            let end = i + c.len_utf8();
            if end > max {
                break;
            }
            if matches!(c, '、' | '，' | ',' | ' ' | '　') {
                cut = Some(end);
            }
        }
        let cut = cut.unwrap_or_else(|| {
            let mut end = max;
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            end
        });
        let (head, tail) = rest.split_at(cut);
        if !head.trim().is_empty() {
            pieces.push(head.trim().to_owned());
        }
        rest = tail;
    }
    if !rest.trim().is_empty() {
        pieces.push(rest.trim().to_owned());
    }
    pieces
}

struct WavInfo {
    sample_rate: u32,
    channels: u16,
    bits: u16,
    data: std::ops::Range<usize>,
}

/// RIFF WAV の fmt と data を読む
fn parse_wav(bytes: &[u8]) -> Result<WavInfo, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("open_jtalk の出力が WAV でない".into());
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let mut format = None;
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " && size >= 16 && body + 16 <= bytes.len() {
            format = Some((u16_at(body + 2), u32_at(body + 4), u16_at(body + 14)));
        } else if id == b"data" {
            let (channels, sample_rate, bits) = format.ok_or("WAV の fmt が data より後にある")?;
            let end = (body + size).min(bytes.len());
            return Ok(WavInfo { sample_rate, channels, bits, data: body..end });
        }
        pos = body + size + (size & 1);
    }
    Err("WAV に data がない".into())
}

/// 声のディレクトリから日本語の *.htsvoice を探し、キャラ名、スタイル名（normal が先頭）の順に並べる
fn scan_voices(dir: &Path) -> Vec<Voice> {
    let mut files = Vec::new();
    collect_htsvoice(dir, 0, &mut files);
    let mut voices: Vec<Voice> = files
        .into_iter()
        .filter_map(|path| {
            let rel = path.strip_prefix(dir).ok()?.with_extension("");
            let id = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            let header = read_header(&path).ok()?;
            let japanese = header.get("FULLCONTEXT_FORMAT").map(String::as_str) == Some("HTS_TTS_JPN");
            if !japanese || !header.contains_key("SAMPLING_FREQUENCY") {
                return None;
            }
            let (character, style) = character_and_style(&id);
            Some(Voice { id, character, style })
        })
        .collect();
    voices.sort_by(|a, b| {
        let key = |v: &Voice| (v.character.clone(), v.style.as_deref() != Some("normal"), v.style.clone());
        key(a).cmp(&key(b))
    });
    voices
}

fn collect_htsvoice(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth < MAX_SCAN_DEPTH {
                collect_htsvoice(&path, depth + 1, out);
            }
        } else if path.extension().is_some_and(|e| e == "htsvoice") {
            out.push(path);
        }
    }
}

/// htsvoice の先頭の [GLOBAL] 節を読む
fn read_header(path: &Path) -> std::io::Result<HashMap<String, String>> {
    use std::io::Read;
    let mut buf = vec![0; 4096];
    let n = std::fs::File::open(path)?.read(&mut buf)?;
    let text = String::from_utf8_lossy(&buf[..n]);
    Ok(text
        .lines()
        .skip_while(|l| l.trim() != "[GLOBAL]")
        .skip(1)
        .take_while(|l| !l.starts_with('['))
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect())
}

/// `mei/mei_happy` → (mei, happy)、`nitech-jp-atr503-m001/nitech_jp_atr503_m001` → (nitech-jp-atr503-m001, なし)。
/// ディレクトリの外に置いた声は、ファイル名をキャラ名にする
fn character_and_style(name: &str) -> (String, Option<String>) {
    let Some((dir, stem)) = name.split_once('/') else { return (name.to_owned(), None) };
    let stem = stem.rsplit('/').next().unwrap_or(stem);
    let normalize = |s: &str| s.replace('-', "_").to_ascii_lowercase();
    let (n_dir, n_stem) = (normalize(dir), normalize(stem));
    if n_stem == n_dir {
        return (dir.to_owned(), None);
    }
    // 置き換えは 1 バイトずつなので、正規化しても長さは変わらない
    let style = match n_stem.strip_prefix(&format!("{n_dir}_")) {
        Some(rest) if !rest.is_empty() => &stem[stem.len() - rest.len()..],
        _ => stem,
    };
    (dir.to_owned(), Some(style.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeOpenJTalk;

    #[test]
    fn options_are_filtered_and_clamped() {
        let options = HashMap::from([
            ("r".to_owned(), 0.8),
            ("a".to_owned(), 3.0),
            ("s".to_owned(), 16000.0),
            ("g".to_owned(), f64::NAN),
        ]);
        let clamped = clamp_options(&options);
        assert_eq!(clamped, BTreeMap::from([("a", 1.0), ("r", 0.8)]));
        assert_eq!(to_args(&clamped), vec!["-a", "1", "-r", "0.8"]);
    }

    #[test]
    fn splits_sentences_and_lines() {
        assert_eq!(
            split_text("こんにちは。今日は「晴れ！」ですね？！\n二行目\r\n\n  "),
            vec!["こんにちは。", "今日は「晴れ！」ですね？！", "二行目"]
        );
        assert_eq!(split_text("「おはよう！」と言った。（笑）次。"), vec!["「おはよう！」と言った。", "（笑）次。"]);
        assert_eq!(split_text("あ\tい"), vec!["あ い"]);
        assert!(split_text(" \n ").is_empty());
    }

    #[test]
    fn splits_long_sentences_at_commas_then_anywhere() {
        let long = format!("{}、{}", "あ".repeat(150), "い".repeat(150));
        assert_eq!(split_text(&long), vec![format!("{}、", "あ".repeat(150)), "い".repeat(150)]);
        assert_eq!(split_long(&"う".repeat(250), MAX_PIECE_BYTES), vec!["う".repeat(200), "う".repeat(50)]);
    }

    #[test]
    fn names_characters_and_styles_from_paths() {
        assert_eq!(character_and_style("mei/mei_happy"), ("mei".into(), Some("happy".into())));
        assert_eq!(
            character_and_style("nitech-jp-atr503-m001/nitech_jp_atr503_m001"),
            ("nitech-jp-atr503-m001".into(), None)
        );
        assert_eq!(character_and_style("tohoku-f01/tohoku-f01-angry"), ("tohoku-f01".into(), Some("angry".into())));
        assert_eq!(character_and_style("voices/other"), ("voices".into(), Some("other".into())));
        assert_eq!(character_and_style("solo"), ("solo".into(), None));
    }

    #[test]
    fn reads_wav_format_and_data() {
        let mut bytes = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\xbb\0\0\0\x77\x01\0\x02\0\x10\0".to_vec();
        bytes.extend_from_slice(b"data\x06\0\0\0\x01\0\xfe\xff\x03\0");
        let info = parse_wav(&bytes).unwrap();
        assert_eq!((info.sample_rate, info.channels, info.bits), (48000, 1, 16));
        assert_eq!(&bytes[info.data], &[1, 0, 0xfe, 0xff, 3, 0]);
        assert!(parse_wav(b"not a wav").is_err());
    }

    #[test]
    fn lists_japanese_voices_with_normal_first() {
        let fake = FakeOpenJTalk::new();
        let ids: Vec<_> = fake.engine().voices().into_iter().map(|v| (v.id, v.style)).collect();
        assert_eq!(
            ids,
            vec![("mei/mei_normal".into(), Some("normal".into())), ("mei/mei_happy".into(), Some("happy".into()))]
        );
        assert!(fake.engine().healthy());
    }

    #[tokio::test]
    async fn synthesizes_each_sentence_and_skips_unspeakable_ones() {
        let fake = FakeOpenJTalk::new();
        let engine = Arc::new(fake.engine());
        let options = HashMap::from([("r".to_owned(), 1.2)]);
        let (rate, stream) = engine.synthesize("mei/mei_normal", "あいう。\n！！\nえお", &options).await.unwrap();
        assert_eq!(rate, 48000);
        let chunks: Vec<Bytes> = stream.map(|c| c.unwrap()).collect().await;
        // 偽の open_jtalk は文のバイト数だけサンプルを出す。「あいう。」12 + 「！！」なし + 「えお」6
        assert_eq!(chunks.iter().map(|c| c.len()).collect::<Vec<_>>(), vec![24, 0, 12]);

        let err = engine.synthesize("mei/mei_normal", "失敗する", &options).await.err().unwrap();
        assert!(matches!(err, SynthError::Failed(_)), "{err:?}");
        let err = engine.synthesize("../mei/mei_normal", "あ", &options).await.err().unwrap();
        assert_eq!(err, SynthError::UnknownVoice("../mei/mei_normal".into()));
        let err = engine.synthesize("mei/mei_normal", " \n ", &options).await.err().unwrap();
        assert_eq!(err, SynthError::EmptyText);
    }

    #[test]
    fn missing_command_or_voices_is_unhealthy() {
        let fake = FakeOpenJTalk::new();
        let missing = OpenJTalk::new(fake.dir().join("nope"), fake.dir().to_owned(), fake.dir().join("voices"));
        assert!(!missing.healthy());
        let empty = OpenJTalk::new(fake.dir().join("open_jtalk"), fake.dir().to_owned(), fake.dir().join("none"));
        assert!(!empty.healthy());
    }
}
