//! 共通変換
//!
//! プラグインの出力 → WAV の解析 → デコード → リサンプル（FFT）→ チャンネル変換・音量 → 末尾無音の除去 → エンコード

use bytes::{BufMut, Bytes, BytesMut};
use futures::StreamExt;
use rubato::{FftFixedIn, Resampler};
use serde::Deserialize;

use crate::engine::{AudioBody, ByteStream, PcmFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    PcmS16le,
    Wav,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFormat {
    pub codec: Codec,
    pub sample_rate: u32,
    pub channels: u16,
}

/// 無音とみなす振幅（±256 / 32768 ≒ -42dB。hanako の TrailingSilenceTrimmer と同じ）
const SILENCE_THRESHOLD: i16 = 256;
const RESAMPLE_CHUNK: usize = 1024;
const MAX_WAV_HEADER: usize = 64 * 1024;

fn invalid(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

/// 生の音声を要求された形式のストリームに変換する
pub fn convert(body: AudioBody, out: OutputFormat, gain: Option<f32>) -> ByteStream {
    let (input, converter) = match body {
        AudioBody::Wav(s) => (s, Converter::new(out, gain, None)),
        AudioBody::Pcm(fmt, s) => (s, Converter::new(out, gain, Some(fmt))),
    };
    Box::pin(futures::stream::unfold(Some((input, converter)), |state| async move {
        let (mut input, mut conv) = state?;
        loop {
            let result = match input.next().await {
                Some(Ok(chunk)) => conv.push(&chunk),
                Some(Err(e)) => return Some((Err(e), None)),
                None => {
                    return match conv.finish() {
                        Ok(b) if b.is_empty() => None,
                        r => Some((r, None)),
                    }
                }
            };
            match result {
                Ok(b) if b.is_empty() => continue,
                Ok(b) => return Some((Ok(b), Some((input, conv)))),
                Err(e) => return Some((Err(e), None)),
            }
        }
    }))
}

pub struct Converter {
    out: OutputFormat,
    gain: f32,
    wav: Option<WavHeaderParser>,
    stage: Option<Stage>,
    header_sent: bool,
    trimmer: SilenceTrimmer,
}

impl Converter {
    pub fn new(out: OutputFormat, gain: Option<f32>, input: Option<PcmFormat>) -> Self {
        let mut c = Self {
            out,
            gain: gain.unwrap_or(1.0),
            wav: None,
            stage: None,
            header_sent: false,
            trimmer: SilenceTrimmer::new(out.channels as usize),
        };
        match input {
            Some(fmt) => c.stage = Some(Stage::new(fmt, out)),
            None => c.wav = Some(WavHeaderParser::default()),
        }
        c
    }

    pub fn push(&mut self, data: &[u8]) -> Result<Bytes, std::io::Error> {
        let mut pcm: &[u8] = data;
        let owned;
        if let Some(parser) = &mut self.wav {
            match parser.push(data)? {
                Some((fmt, rest)) => {
                    self.wav = None;
                    self.stage = Some(Stage::new(fmt, self.out));
                    owned = rest;
                    pcm = &owned;
                }
                None => return Ok(Bytes::new()),
            }
        }
        let stage = self.stage.as_mut().expect("形式が決まっている");
        let frames = stage.push(pcm);
        let encoded = encode(&frames, self.out.channels as usize, self.gain);
        let trimmed = self.trimmer.push(&encoded);
        Ok(self.with_header(trimmed))
    }

    pub fn finish(&mut self) -> Result<Bytes, std::io::Error> {
        if self.wav.is_some() {
            return Err(invalid("WAV の data チャンクが見つからない"));
        }
        let stage = self.stage.as_mut().expect("形式が決まっている");
        let frames = stage.finish();
        let encoded = encode(&frames, self.out.channels as usize, self.gain);
        // 末尾の無音は捨てる
        let trimmed = self.trimmer.push(&encoded);
        Ok(self.with_header(trimmed))
    }

    fn with_header(&mut self, body: Bytes) -> Bytes {
        if self.out.codec != Codec::Wav || self.header_sent {
            return body;
        }
        self.header_sent = true;
        let mut b = BytesMut::with_capacity(44 + body.len());
        b.put_slice(&wav_header(self.out.sample_rate, self.out.channels));
        b.put_slice(&body);
        b.freeze()
    }
}

/// デコードとリサンプル
struct Stage {
    in_fmt: PcmFormat,
    /// リサンプルするチャンネル数（入力と出力の少ないほう）
    channels: usize,
    carry: Vec<u8>,
    input: Vec<Vec<f32>>,
    resampler: Option<FftFixedIn<f32>>,
    /// リサンプラの遅延ぶん、出力の先頭を捨てる
    skip: usize,
    in_frames: u64,
    out_frames: u64,
    out_rate: u32,
}

impl Stage {
    fn new(in_fmt: PcmFormat, out: OutputFormat) -> Self {
        let channels = in_fmt.channels.min(out.channels).max(1) as usize;
        let resampler = (in_fmt.sample_rate != out.sample_rate).then(|| {
            FftFixedIn::new(in_fmt.sample_rate as usize, out.sample_rate as usize, RESAMPLE_CHUNK, 2, channels)
                .expect("リサンプラを作れる")
        });
        let skip = resampler.as_ref().map(|r| r.output_delay()).unwrap_or(0);
        Self {
            in_fmt,
            channels,
            carry: Vec::new(),
            input: vec![Vec::new(); channels],
            resampler,
            skip,
            in_frames: 0,
            out_frames: 0,
            out_rate: out.sample_rate,
        }
    }

    fn push(&mut self, data: &[u8]) -> Vec<Vec<f32>> {
        self.decode(data);
        let mut out = vec![Vec::new(); self.channels];
        let Some(rs) = self.resampler.as_mut() else {
            for (o, i) in out.iter_mut().zip(self.input.iter_mut()) {
                o.append(i);
            }
            self.out_frames += out[0].len() as u64;
            return out;
        };
        let mut consumed = 0;
        loop {
            let need = rs.input_frames_next();
            if self.input[0].len() - consumed < need {
                break;
            }
            let chunk: Vec<&[f32]> = self.input.iter().map(|c| &c[consumed..consumed + need]).collect();
            let res = rs.process(chunk.as_slice(), None).expect("チャンクの長さは合っている");
            append(&mut out, res);
            consumed += need;
        }
        for c in &mut self.input {
            c.drain(..consumed);
        }
        self.take_output(out)
    }

    fn finish(&mut self) -> Vec<Vec<f32>> {
        let mut out = vec![Vec::new(); self.channels];
        let Some(rs) = self.resampler.as_mut() else {
            return out;
        };
        let rest = std::mem::replace(&mut self.input, vec![Vec::new(); self.channels]);
        if !rest[0].is_empty() {
            append(&mut out, rs.process_partial(Some(rest.as_slice()), None).expect("長さは上限以下"));
        }
        // 遅延ぶん残っている出力を押し出す
        let expected = self.in_frames * self.out_rate as u64 / self.in_fmt.sample_rate as u64;
        for _ in 0..4 {
            if self.out_frames + (out[0].len().saturating_sub(self.skip)) as u64 >= expected {
                break;
            }
            append(&mut out, rs.process_partial::<Vec<f32>>(None, None).expect("空の入力"));
        }
        let mut out = self.take_output(out);
        let over = self.out_frames.saturating_sub(expected) as usize;
        for c in &mut out {
            let keep = c.len().saturating_sub(over);
            c.truncate(keep);
        }
        out
    }

    /// 先頭の遅延ぶんを捨てて出力を数える
    fn take_output(&mut self, mut out: Vec<Vec<f32>>) -> Vec<Vec<f32>> {
        if self.skip > 0 {
            let n = self.skip.min(out[0].len());
            for c in &mut out {
                c.drain(..n);
            }
            self.skip -= n;
        }
        self.out_frames += out[0].len() as u64;
        out
    }

    fn decode(&mut self, data: &[u8]) {
        let in_ch = self.in_fmt.channels as usize;
        let frame = in_ch * 2;
        let mut buf: &[u8] = data;
        let joined;
        if !self.carry.is_empty() {
            let mut v = std::mem::take(&mut self.carry);
            v.extend_from_slice(data);
            joined = v;
            buf = &joined;
        }
        let whole = buf.len() / frame * frame;
        for f in buf[..whole].chunks_exact(frame) {
            let s = |i: usize| i16::from_le_bytes([f[i * 2], f[i * 2 + 1]]) as f32 / 32768.0;
            if self.channels == in_ch {
                for ch in 0..in_ch {
                    self.input[ch].push(s(ch));
                }
            } else {
                // 出力がモノラルなら入力を混ぜる
                let mix = (0..in_ch).map(s).sum::<f32>() / in_ch as f32;
                self.input[0].push(mix);
            }
        }
        self.in_frames += (whole / frame) as u64;
        self.carry = buf[whole..].to_vec();
    }
}

fn append(out: &mut [Vec<f32>], res: Vec<Vec<f32>>) {
    for (o, r) in out.iter_mut().zip(res) {
        o.extend_from_slice(&r);
    }
}

/// リサンプル後のフレームを s16le のインターリーブにする。モノラルから増やすときは複製する
fn encode(frames: &[Vec<f32>], out_channels: usize, gain: f32) -> Bytes {
    let n = frames.first().map_or(0, Vec::len);
    let mut b = BytesMut::with_capacity(n * out_channels * 2);
    for i in 0..n {
        for ch in 0..out_channels {
            let x = frames[ch.min(frames.len() - 1)][i] * gain;
            b.put_i16_le((x * 32768.0).round().clamp(-32768.0, 32767.0) as i16);
        }
    }
    b.freeze()
}

/// 末尾の無音を除去する。無音のフレームは、後ろに音が続いたときだけ出力する
struct SilenceTrimmer {
    frame: usize,
    pending: BytesMut,
}

impl SilenceTrimmer {
    fn new(channels: usize) -> Self {
        Self { frame: channels * 2, pending: BytesMut::new() }
    }

    fn push(&mut self, data: &[u8]) -> Bytes {
        let end = data
            .chunks_exact(self.frame)
            .rposition(|f| f.chunks_exact(2).any(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs() > SILENCE_THRESHOLD as u16))
            .map(|i| (i + 1) * self.frame);
        match end {
            Some(end) => {
                let mut out = std::mem::take(&mut self.pending);
                out.put_slice(&data[..end]);
                self.pending.put_slice(&data[end..]);
                out.freeze()
            }
            None => {
                self.pending.put_slice(data);
                Bytes::new()
            }
        }
    }
}

/// WAV のヘッダを読み、形式と data チャンクの先頭以降を返す
#[derive(Default)]
struct WavHeaderParser {
    buf: Vec<u8>,
    fmt: Option<PcmFormat>,
}

impl WavHeaderParser {
    fn push(&mut self, data: &[u8]) -> Result<Option<(PcmFormat, Vec<u8>)>, std::io::Error> {
        self.buf.extend_from_slice(data);
        if self.buf.len() < 12 {
            return Ok(None);
        }
        if &self.buf[0..4] != b"RIFF" || &self.buf[8..12] != b"WAVE" {
            return Err(invalid("WAV ではない"));
        }
        let mut pos = 12;
        loop {
            if self.buf.len() < pos + 8 {
                break;
            }
            let id = &self.buf[pos..pos + 4];
            let size = u32::from_le_bytes(self.buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
            let body = pos + 8;
            if id == b"data" {
                let fmt = self.fmt.ok_or_else(|| invalid("WAV の fmt チャンクがない"))?;
                return Ok(Some((fmt, self.buf[body..].to_vec())));
            }
            if self.buf.len() < body + size {
                break;
            }
            if id == b"fmt " {
                if size < 16 {
                    return Err(invalid("WAV の fmt チャンクが短い"));
                }
                let f = &self.buf[body..body + 16];
                let tag = u16::from_le_bytes([f[0], f[1]]);
                let channels = u16::from_le_bytes([f[2], f[3]]);
                let sample_rate = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
                let bits = u16::from_le_bytes([f[14], f[15]]);
                if !(tag == 1 || tag == 0xFFFE) || bits != 16 || !(1..=2).contains(&channels) || sample_rate == 0 {
                    return Err(invalid(format!("対応していない WAV 形式 tag={tag} {bits}bit {channels}ch")));
                }
                self.fmt = Some(PcmFormat { sample_rate, channels });
            }
            pos = body + size + (size & 1);
        }
        if self.buf.len() > MAX_WAV_HEADER {
            return Err(invalid("WAV のヘッダが長すぎる"));
        }
        Ok(None)
    }
}

/// 長さ不明のストリーム用の WAV ヘッダ（サイズ欄は 0xFFFFFFFF）
pub(crate) fn wav_header(sample_rate: u32, channels: u16) -> [u8; 44] {
    let mut h = [0u8; 44];
    let block_align = channels * 2;
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&(sample_rate * block_align as u32).to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 正弦波の WAV（16bit）を作り、末尾に無音を付ける
    fn sine_wav(rate: u32, channels: u16, secs: f32, silence_secs: f32) -> Vec<u8> {
        let n = (rate as f32 * secs) as usize;
        let s = (rate as f32 * silence_secs) as usize;
        let mut pcm = Vec::new();
        for i in 0..n + s {
            let v = if i < n { ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 12000.0) as i16 } else { 0 };
            for _ in 0..channels {
                pcm.extend_from_slice(&v.to_le_bytes());
            }
        }
        let mut wav = wav_header(rate, channels).to_vec();
        wav[4..8].copy_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
        wav[40..44].copy_from_slice(&(pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(&pcm);
        wav
    }

    fn run(conv: &mut Converter, data: &[u8], piece: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for c in data.chunks(piece) {
            out.extend_from_slice(&conv.push(c).unwrap());
        }
        out.extend_from_slice(&conv.finish().unwrap());
        out
    }

    const PCM48: OutputFormat = OutputFormat { codec: Codec::PcmS16le, sample_rate: 48000, channels: 2 };

    #[test]
    fn resamples_wav_24k_mono_to_48k_stereo_and_trims_silence() {
        let wav = sine_wav(24000, 1, 1.0, 0.5);
        // 半端なバイト数で区切っても結果は同じ長さになる
        for piece in [7, 1000, wav.len()] {
            let mut conv = Converter::new(PCM48, None, None);
            let out = run(&mut conv, &wav, piece);
            let frames = out.len() / 4;
            // 1 秒の音 → 48000 フレーム前後。0.5 秒の無音は除かれる
            assert!((47_000..=48_100).contains(&frames), "piece={piece} frames={frames}");
            // 左右は同じ値
            assert_eq!(out[1000 * 4..1000 * 4 + 2], out[1000 * 4 + 2..1000 * 4 + 4]);
        }
    }

    #[test]
    fn keeps_silence_between_sounds() {
        let mut wav = sine_wav(48000, 2, 0.1, 0.2);
        let tail = sine_wav(48000, 2, 0.1, 0.0);
        wav.extend_from_slice(&tail[44..]);
        let mut conv = Converter::new(PCM48, None, None);
        let out = run(&mut conv, &wav, 4096);
        // 間の無音は残る（末尾のゼロ交差付近の数フレームだけは除かれうる）
        assert!((19_100..=19_200).contains(&(out.len() / 4)), "frames={}", out.len() / 4);
    }

    #[test]
    fn pcm_input_and_wav_output() {
        let wav = sine_wav(22050, 1, 0.5, 0.0);
        let out_fmt = OutputFormat { codec: Codec::Wav, sample_rate: 48000, channels: 1 };
        let mut conv = Converter::new(out_fmt, Some(0.5), Some(PcmFormat { sample_rate: 22050, channels: 1 }));
        let out = run(&mut conv, &wav[44..], 999);
        assert_eq!(&out[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(out[24..28].try_into().unwrap()), 48000);
        let frames = (out.len() - 44) / 2;
        assert!((23_500..=24_100).contains(&frames), "frames={frames}");
        // 音量 0.5 倍で振幅は 6000 前後になる
        let peak = out[44..].chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs()).max().unwrap();
        assert!((5_500..=6_500).contains(&peak), "peak={peak}");
    }

    #[test]
    fn stereo_input_is_mixed_down_for_mono_output() {
        let wav = sine_wav(48000, 2, 0.2, 0.0);
        let out_fmt = OutputFormat { codec: Codec::PcmS16le, sample_rate: 48000, channels: 1 };
        let mut conv = Converter::new(out_fmt, None, None);
        let out = run(&mut conv, &wav, 1234);
        assert!((9_500..=9_600).contains(&(out.len() / 2)), "frames={}", out.len() / 2);
    }

    #[test]
    fn skips_unknown_wav_chunks_and_rejects_non_wav() {
        let wav = sine_wav(24000, 1, 0.1, 0.0);
        let mut with_list = wav[..36].to_vec();
        with_list.extend_from_slice(b"LIST");
        with_list.extend_from_slice(&3u32.to_le_bytes());
        with_list.extend_from_slice(b"abc\0");
        with_list.extend_from_slice(&wav[36..]);
        let mut conv = Converter::new(PCM48, None, None);
        assert!(!run(&mut conv, &with_list, 5).is_empty());

        let mut conv = Converter::new(PCM48, None, None);
        assert!(conv.push(b"NOT A WAVE FILE").is_err());
    }
}
