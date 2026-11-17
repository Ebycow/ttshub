//! Open JTalk（ttshub と同じコンテナの中で open_jtalk コマンドを動かす）
//!
//! ネットワークの先にあるエンジンではないので、設定に backends は書かない（設定を読むときに `local` の 1 台になる）。
//! 同時合成数（max_concurrency_per_backend）が、同時に動かす open_jtalk の数になる
//!
//! open_jtalk は入力の 1 行目しか読まず、1 行も 1023 バイトで切り捨てるので、文に分けて 1 文ずつ合成し、
//! できた文から順にヘッダなしの PCM（16bit モノラル）で返す
//!
//! 共通パラメータは open_jtalk のオプションに置き換える（speed → -r、pitch → -fm、volume → -g）。
//! intonation は扱わない。近いのは -jf（対数 F0 の GV の重み）だが、Mei や Takumi の声は F0 の GV を持たず、何も変わらない。
//! `x-openjtalk-<オプション名>` で open_jtalk のオプション（a, b, r, fm, u, jm, jf, g）をそのまま指定できる
//!
//! 辞書と声の場所は環境変数で変えられる（括弧内は既定値。イメージの中の場所）:
//! `OPENJTALK_BIN`（open_jtalk）、`OPENJTALK_DIC`（/var/lib/mecab/dic/open-jtalk/naist-jdic）、
//! `OPENJTALK_VOICE_DIR`（/usr/share/hts-voice）

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use super::{AudioBody, BackendClient, Capabilities, EngineError, EnginePlugin, PcmFormat, VoiceEntry};
use crate::params::Params;

/// open_jtalk に 1 回で渡す上限。open_jtalk は 1 行を 1023 バイトで切るので、余裕を持たせる
const MAX_PIECE_BYTES: usize = 600;
/// 声のファイルを探す深さ
const MAX_SCAN_DEPTH: usize = 3;

/// 渡してよい open_jtalk のオプションと、値の範囲。
/// サンプリング周波数（-s）やフレーム周期（-p）は PCM の形式が変わるので渡さない
const OPTIONS: [(&str, f64, f64); 8] = [
    ("a", 0.0, 1.0),     // オールパス定数（声の太さ）
    ("b", 0.0, 1.0),     // ポストフィルタ係数
    ("r", 0.1, 10.0),    // 話速
    ("fm", -24.0, 24.0), // 音高（半音単位で足す）
    ("u", 0.0, 1.0),     // 有声/無声の閾値
    ("jm", 0.0, 10.0),   // スペクトルの GV の重み
    ("jf", 0.0, 10.0),   // 対数 F0 の GV の重み
    ("g", -100.0, 20.0), // 音量（dB）
];

pub struct OpenJTalk {
    runner: Arc<Runner>,
}

/// open_jtalk コマンドと、辞書・声の場所
struct Runner {
    bin: PathBuf,
    dic: PathBuf,
    voice_dir: PathBuf,
}

impl OpenJTalk {
    pub fn from_env() -> Self {
        let env = |key: &str, default: &str| PathBuf::from(std::env::var(key).unwrap_or_else(|_| default.into()));
        Self::new(
            env("OPENJTALK_BIN", "open_jtalk"),
            env("OPENJTALK_DIC", "/var/lib/mecab/dic/open-jtalk/naist-jdic"),
            env("OPENJTALK_VOICE_DIR", "/usr/share/hts-voice"),
        )
    }

    fn new(bin: PathBuf, dic: PathBuf, voice_dir: PathBuf) -> Self {
        Self { runner: Arc::new(Runner { bin, dic, voice_dir }) }
    }
}

#[async_trait]
impl EnginePlugin for OpenJTalk {
    fn param_prefix(&self) -> &'static str {
        "openjtalk"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { speed: true, pitch: true, volume: true, intonation: false }
    }

    async fn list_voices(&self, _: &BackendClient) -> Result<Vec<VoiceEntry>, EngineError> {
        let voices = scan_voices(&self.runner.voice_dir);
        if voices.is_empty() {
            return Err(EngineError::Unavailable(format!(
                "{} に Open JTalk の声（*.htsvoice）がない",
                self.runner.voice_dir.display()
            )));
        }
        Ok(voices)
    }

    async fn synthesize(
        &self,
        _: &BackendClient,
        voice: &VoiceEntry,
        text: &str,
        params: &Params,
    ) -> Result<AudioBody, EngineError> {
        let voice_path = self.runner.voice_dir.join(format!("{}.htsvoice", voice.internal_id));
        let sample_rate = read_header(&voice_path)
            .ok()
            .and_then(|h| h.get("SAMPLING_FREQUENCY").and_then(|v| v.parse().ok()))
            .ok_or_else(|| EngineError::Unavailable(format!("声 {} を読めない", voice_path.display())))?;
        let args = to_args(&options(params, self.param_prefix()));
        let pieces = split_text(text);
        if pieces.is_empty() {
            return Err(EngineError::Rejected("text が空".into()));
        }

        // 最初の文は返す前に合成し、失敗したらエラーで返す（ハブが代わりの声に回せるように）
        let first = self
            .runner
            .synthesize_piece(&voice_path, sample_rate, &pieces[0], &args)
            .await
            .map_err(EngineError::Unavailable)?;
        let rest = futures::stream::unfold(
            (self.runner.clone(), voice_path, pieces.into_iter().skip(1), args),
            move |(runner, voice_path, mut pieces, args)| async move {
                let piece = pieces.next()?;
                let result = runner
                    .synthesize_piece(&voice_path, sample_rate, &piece, &args)
                    .await
                    .map(Bytes::from)
                    .map_err(std::io::Error::other);
                Some((result, (runner, voice_path, pieces, args)))
            },
        );
        let stream = futures::stream::once(async move { Ok(Bytes::from(first)) }).chain(rest);
        Ok(AudioBody::Pcm(PcmFormat { sample_rate, channels: 1 }, Box::pin(stream)))
    }

    async fn health(&self, _: &BackendClient) -> bool {
        let runner = &self.runner;
        runner.dic.join("sys.dic").is_file() && find_command(&runner.bin) && !scan_voices(&runner.voice_dir).is_empty()
    }
}

impl Runner {
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

/// 共通パラメータとエンジン固有パラメータを open_jtalk のオプションにする。エンジン固有のものが優先。範囲外の値は丸める
fn options(params: &Params, prefix: &str) -> BTreeMap<&'static str, f64> {
    let mut opts = BTreeMap::new();
    if let Some(speed) = params.speed {
        opts.insert("r", speed as f64);
    }
    // VOICEVOX の pitch は対数 F0（自然対数）に足す量。半音に直す（0.15 ≒ 2.6 半音）
    if let Some(pitch) = params.pitch {
        opts.insert("fm", pitch as f64 * 12.0 / std::f64::consts::LN_2);
    }
    // 倍率を dB に直す。0 は無音（下限の -100dB）
    if let Some(volume) = params.volume {
        opts.insert("g", if volume > 0.0 { 20.0 * (volume as f64).log10() } else { -100.0 });
    }
    for (key, raw) in params.extra_for(prefix) {
        if let (Some(&(name, _, _)), Ok(value)) = (OPTIONS.iter().find(|(o, _, _)| *o == key), raw.parse::<f64>()) {
            if value.is_finite() {
                opts.insert(name, value);
            }
        }
    }
    for (name, min, max) in OPTIONS {
        if let Some(v) = opts.get_mut(name) {
            *v = v.clamp(min, max);
        }
    }
    opts
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

/// 声のディレクトリから日本語の *.htsvoice を探し、キャラ名、スタイル名（normal が先頭）の順に並べる。
/// internal_id は声のディレクトリからの相対パス（拡張子なし）
fn scan_voices(dir: &Path) -> Vec<VoiceEntry> {
    let mut files = Vec::new();
    collect_htsvoice(dir, 0, &mut files);
    let mut voices: Vec<VoiceEntry> = files
        .into_iter()
        .filter_map(|path| {
            let rel = path.strip_prefix(dir).ok()?.with_extension("");
            let name = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            let header = read_header(&path).ok()?;
            let japanese = header.get("FULLCONTEXT_FORMAT").map(String::as_str) == Some("HTS_TTS_JPN");
            if !japanese || !header.contains_key("SAMPLING_FREQUENCY") {
                return None;
            }
            let (character, style) = character_and_style(&name);
            Some(VoiceEntry { internal_id: name, character, style, is_default: false })
        })
        .collect();
    voices.sort_by(|a, b| {
        let key = |v: &VoiceEntry| (v.character.clone(), v.style.as_deref() != Some("normal"), v.style.clone());
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

    #[test]
    fn common_params_become_open_jtalk_options() {
        let params =
            Params { speed: Some(1.5), pitch: Some(0.0), volume: Some(0.0), intonation: Some(1.2), extra: vec![] };
        let opts = options(&params, "openjtalk");
        assert_eq!(opts, BTreeMap::from([("fm", 0.0), ("g", -100.0), ("r", 1.5)]));
        assert_eq!(to_args(&opts), vec!["-fm", "0", "-g", "-100", "-r", "1.5"]);

        let opts = options(&Params { pitch: Some(0.15), volume: Some(2.0), ..Default::default() }, "openjtalk");
        assert!((opts["fm"] - 2.597).abs() < 0.01);
        assert!((opts["g"] - 6.02).abs() < 0.01);
    }

    #[test]
    fn engine_specific_options_override_are_clamped_and_unknown_ones_are_dropped() {
        let params = Params {
            speed: Some(1.5),
            extra: vec![
                ("x-openjtalk-r".into(), "0.8".into()),
                ("x-openjtalk-a".into(), "3".into()),
                ("x-openjtalk-s".into(), "16000".into()),
                ("x-openjtalk-b".into(), "soft".into()),
                ("x-voicevox-r".into(), "2".into()),
            ],
            ..Default::default()
        };
        assert_eq!(options(&params, "openjtalk"), BTreeMap::from([("a", 1.0), ("r", 0.8)]));
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

    /// テスト用の一時ディレクトリ（依存を増やさないための最小限のもの）
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("ttshub-openjtalk-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 偽の open_jtalk と声を置いたプラグイン。
    /// 偽の open_jtalk は、読んだ文のバイト数だけサンプルを持つ WAV を出し、「失敗」を含む文では失敗する
    #[cfg(unix)]
    fn fake_plugin(tmp: &TempDir) -> OpenJTalk {
        use std::os::unix::fs::PermissionsExt;
        let script = tmp.0.join("open_jtalk");
        std::fs::write(
            &script,
            r#"#!/bin/sh
read -r line
case "$line" in
  *失敗*) echo "Error: something broke" >&2; exit 2 ;;
  *！！*) echo "Error: waveform cannot be synthesized." >&2; exit 1 ;;
esac
bytes=$(($(printf '%s' "$line" | wc -c) * 2))
le32() { printf "\\$(printf %o $(($1 & 255)))\\$(printf %o $((($1 >> 8) & 255)))\\$(printf %o $((($1 >> 16) & 255)))\\$(printf %o $((($1 >> 24) & 255)))"; }
printf 'RIFF'; le32 $((36 + bytes)); printf 'WAVEfmt '; le32 16
printf '\001\000\001\000'; le32 48000; le32 96000; printf '\002\000\020\000'
printf 'data'; le32 $bytes
head -c $bytes /dev/zero
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let voices = tmp.0.join("voices");
        std::fs::create_dir_all(voices.join("mei")).unwrap();
        let header =
            |format: &str| format!("[GLOBAL]\nSAMPLING_FREQUENCY:48000\nFULLCONTEXT_FORMAT:{format}\n[STREAM]\n");
        for style in ["mei_happy", "mei_normal"] {
            std::fs::write(voices.join(format!("mei/{style}.htsvoice")), header("HTS_TTS_JPN")).unwrap();
        }
        std::fs::write(voices.join("english.htsvoice"), header("HTS_TTS_ENG")).unwrap();
        std::fs::write(tmp.0.join("sys.dic"), "").unwrap();
        OpenJTalk::new(script, tmp.0.clone(), voices)
    }

    fn backend() -> BackendClient {
        BackendClient { base: "local".into(), http: reqwest::Client::new(), api_key: None }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lists_japanese_voices_with_normal_first() {
        let tmp = TempDir::new();
        let plugin = fake_plugin(&tmp);
        let voices = plugin.list_voices(&backend()).await.unwrap();
        let names: Vec<_> = voices.iter().map(|v| (v.internal_id.as_str(), v.style.as_deref())).collect();
        assert_eq!(names, vec![("mei/mei_normal", Some("normal")), ("mei/mei_happy", Some("happy"))]);
        assert!(plugin.health(&backend()).await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn synthesizes_each_sentence_and_skips_unspeakable_ones() {
        let tmp = TempDir::new();
        let plugin = fake_plugin(&tmp);
        let voice = plugin.list_voices(&backend()).await.unwrap().remove(1);
        let params = Params { speed: Some(1.2), ..Default::default() };
        let AudioBody::Pcm(format, stream) =
            plugin.synthesize(&backend(), &voice, "あいう。\n！！\nえお", &params).await.unwrap()
        else {
            panic!("PCM で返るはず");
        };
        assert_eq!(format, PcmFormat { sample_rate: 48000, channels: 1 });
        let chunks: Vec<Bytes> = stream.map(|c| c.unwrap()).collect().await;
        // 「あいう。」12 バイト + 「！！」なし + 「えお」6 バイト = 18 サンプル
        assert_eq!(chunks.iter().map(|c| c.len()).collect::<Vec<_>>(), vec![24, 0, 12]);

        let err = plugin.synthesize(&backend(), &voice, "失敗する", &params).await.err().unwrap();
        assert!(matches!(err, EngineError::Unavailable(_)), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_command_or_voices_is_unhealthy() {
        let tmp = TempDir::new();
        let missing = OpenJTalk::new(tmp.0.join("nope"), tmp.0.clone(), tmp.0.join("voices"));
        assert!(!missing.health(&backend()).await);
        assert!(missing.list_voices(&backend()).await.is_err());
    }
}
