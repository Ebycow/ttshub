//! エンジンプラグイン

mod aivis_cloud;
mod ebyroid;
mod voicevox;

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

use crate::config::EngineConfig;
use crate::params::Params;

pub type ByteStream = BoxStream<'static, Result<Bytes, std::io::Error>>;

/// PCM（符号付き 16bit リトルエンディアン）の形式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// プラグインが返す生の音声
pub enum AudioBody {
    /// WAV（ヘッダから形式を読む）
    Wav(ByteStream),
    /// ヘッダなしの PCM
    Pcm(PcmFormat, ByteStream),
}

impl AudioBody {
    /// 本体のストリームに手を加える（同時合成数の枠を持たせ続けるなど）
    pub fn map_stream(self, f: impl FnOnce(ByteStream) -> ByteStream) -> Self {
        match self {
            AudioBody::Wav(s) => AudioBody::Wav(f(s)),
            AudioBody::Pcm(fmt, s) => AudioBody::Pcm(fmt, f(s)),
        }
    }
}

/// エンジンが申告する話者
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoiceEntry {
    /// エンジン内部の ID（VOICEVOX なら style id）
    pub internal_id: String,
    pub character: String,
    pub style: Option<String>,
    /// エンジンが申告するデフォルト話者か
    #[serde(default)]
    pub is_default: bool,
    /// エンジンが申告するクレジット表記（規約の文面から読み取ったもの）
    #[serde(default)]
    pub credit: Option<String>,
    /// その話者の利用規約の URL
    #[serde(default)]
    pub terms_url: Option<String>,
}

/// プラグインが自分で処理できるパラメータ。できないものは共通変換で処理するか無視する
#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    pub speed: bool,
    pub pitch: bool,
    pub volume: bool,
    pub intonation: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// 接続できない、時間切れ、5xx など。別のバックエンドや別の声で再試行する
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// エンジンが要求を受け付けなかった（4xx など）
    #[error("rejected: {0}")]
    Rejected(String),
}

impl From<reqwest::Error> for EngineError {
    fn from(e: reqwest::Error) -> Self {
        EngineError::Unavailable(e.without_url().to_string())
    }
}

/// 状態コードからエラーの種類を決める
pub(crate) fn status_error(status: reqwest::StatusCode, what: &str) -> EngineError {
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        EngineError::Unavailable(format!("{what}: HTTP {status}"))
    } else {
        EngineError::Rejected(format!("{what}: HTTP {status}"))
    }
}

/// バックエンド 1 台への接続情報
pub struct BackendClient {
    pub base: String,
    pub http: reqwest::Client,
    /// クラウド TTS の API キー
    pub api_key: Option<String>,
}

impl BackendClient {
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }
}

#[async_trait]
pub trait EnginePlugin: Send + Sync {
    /// エンジン固有パラメータの接頭辞（`x-<prefix>-<キー>`）
    fn param_prefix(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    async fn list_voices(&self, backend: &BackendClient) -> Result<Vec<VoiceEntry>, EngineError>;

    /// 合成して生の音声を返す。応答が始まった時点で返し、本体はストリームで読む
    async fn synthesize(
        &self,
        backend: &BackendClient,
        voice: &VoiceEntry,
        text: &str,
        params: &Params,
    ) -> Result<AudioBody, EngineError>;

    async fn health(&self, backend: &BackendClient) -> bool;
}

pub fn create(config: &EngineConfig) -> Result<Arc<dyn EnginePlugin>, String> {
    match config.plugin.as_str() {
        "ebyroid" => Ok(Arc::new(ebyroid::Ebyroid::new(config.voices.clone()))),
        "voicevox-compatible" => Ok(Arc::new(voicevox::Voicevox::default())),
        "aivis-cloud" => {
            if config.models.is_empty() {
                return Err("aivis-cloud には models（使うモデルの UUID）が必要".into());
            }
            Ok(Arc::new(aivis_cloud::AivisCloud::new(config.models.clone())))
        }
        other => Err(format!("知らないプラグイン {other}")),
    }
}

/// 規約や説明の文面から、「VOICEVOX:ずんだもん」「AivisSpeech: まお」のような鉤括弧付きのクレジット表記を探す
pub(crate) fn find_credit(text: &str) -> Option<String> {
    text.split('「').skip(1).filter_map(|s| s.split_once('」')).map(|(inner, _)| inner.trim()).find(|inner| {
        // 区切りの前が英数字で始まるブランド名（「※非公式です」などの鉤括弧は除く）
        inner.split_once([':', '：']).is_some_and(|(brand, name)| {
            let brand = brand.trim();
            brand.starts_with(|c: char| c.is_ascii_alphanumeric())
                && !brand.contains(char::is_whitespace)
                && !name.trim().is_empty()
                && !inner.contains('\n')
        })
    })
    .map(str::to_owned)
}

/// 文面の最後の URL（規約の文面は「詳細は以下をご確認ください」の後に URL を置くことが多い）
pub(crate) fn find_last_url(text: &str) -> Option<String> {
    let start = text.rfind("https://").or_else(|| text.rfind("http://"))?;
    let url: String = text[start..]
        .chars()
        .take_while(|c| !c.is_whitespace() && !matches!(c, ')' | '>' | '」' | '）' | '"'))
        .collect();
    Some(url)
}

/// HTTP レスポンスの本体をストリームにする
pub(crate) fn body_stream(resp: reqwest::Response) -> ByteStream {
    use futures::TryStreamExt;
    Box::pin(resp.bytes_stream().map_err(|e| std::io::Error::other(e.without_url())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_credit_in_voicevox_policy() {
        let policy = "ずんだもんの音声ライブラリを用いて生成した音声は、\n「VOICEVOX:ずんだもん」とそれぞれクレジットを記載すれば、商用・非商用で利用可能です。\n\n利用規約の詳細は以下をご確認ください。  \nhttps://zunko.jp/con_ongen_kiyaku.html\n";
        assert_eq!(find_credit(policy).as_deref(), Some("VOICEVOX:ずんだもん"));
        assert_eq!(find_last_url(policy).as_deref(), Some("https://zunko.jp/con_ongen_kiyaku.html"));
    }

    #[test]
    fn keeps_cv_and_fullwidth_parts_of_credit() {
        let policy = "「VOICEVOX:もち子(cv 明日葉よもぎ)」とクレジットを記載すれば…「[もちぞら模型店](https://vtubermochio.wixsite.com/mochizora/contact)」に対し…\nhttps://vtubermochio.wixsite.com/mochizora/利用規約";
        assert_eq!(find_credit(policy).as_deref(), Some("VOICEVOX:もち子(cv 明日葉よもぎ)"));
        assert_eq!(find_last_url(policy).as_deref(), Some("https://vtubermochio.wixsite.com/mochizora/利用規約"));
        assert_eq!(find_credit("「VOICEVOX:里石ユカ（つぼみ）」と").as_deref(), Some("VOICEVOX:里石ユカ（つぼみ）"));
    }

    #[test]
    fn finds_credit_in_aivis_description_and_skips_other_brackets() {
        let desc = "「※非公式です」と明記…いたずらっ子な小悪魔ボイス。\nクレジットして頂ける際は「AivisSpeech: まお」をご利用ください。";
        assert_eq!(find_credit(desc).as_deref(), Some("AivisSpeech: まお"));
        assert_eq!(find_credit("クレジット表記は任意です。"), None);
    }
}
