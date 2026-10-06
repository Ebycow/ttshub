//! 合成リクエストの追跡ログ（DESIGN §6.3）
//!
//! 1 つのリクエストについて、受付・応答開始・終了を同じ ID で記録する。
//! 終了の記録は、利用者の切断で途中で捨てられた場合も含めて必ず 1 回出す。
//!
//! **本文は決して記録しない**（プライバシーポリシー）。記録するのは文字数だけにする。
//! エラーの内容を記録するときも、本文が混ざりうるもの（JSON の解析エラーの文言、本文をクエリに含む URL など）は渡さない。

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::Stream;

use crate::engine::ByteStream;

/// リクエストの終わり方
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 最後まで流し終えた
    Completed,
    /// 利用者が切断した（応答の前でも、流している途中でも）
    ClientDisconnected,
    /// 流している途中でエンジンの音声が壊れた・途切れた
    StreamError,
    /// どの候補でも合成できなかった（時間切れ・エンジンの停止など）
    Unavailable,
    /// 要求が正しくない（知らない声、長すぎる本文など）
    Rejected,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::ClientDisconnected => "client_disconnected",
            Outcome::StreamError => "stream_error",
            Outcome::Unavailable => "unavailable",
            Outcome::Rejected => "rejected",
        }
    }
}

/// 利用者が渡した ID として受け入れる形（英数字・`-`・`_` で 64 文字まで）
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// 起動時刻と通し番号から、このプロセスの中で重ならない ID を作る
pub fn new_id() -> String {
    static BOOT: OnceLock<u64> = OnceLock::new();
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let boot = *BOOT.get_or_init(|| SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    format!("{boot:x}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// 1 つのリクエストの記録。終了を記録しないまま捨てられたら、利用者の切断として記録する
pub struct RequestLog {
    id: String,
    started: Instant,
    first_audio: Option<Duration>,
    audio_bytes: u64,
    /// 出力の 1 秒あたりのバイト数（音声の長さを出すため）
    bytes_per_sec: u64,
    finished: bool,
}

impl RequestLog {
    /// 受付を記録する
    pub fn start(id: String, started: Instant, client: &str, chars: usize, requested: &str) -> Self {
        tracing::info!(id = %id, client, chars, requested, "受付");
        Self { id, started, first_audio: None, audio_bytes: 0, bytes_per_sec: 0, finished: false }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// 終了を記録する（2 回目以降は何もしない）
    /// `error` に本文を含めてはいけない
    pub fn finish(&mut self, outcome: Outcome, error: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let elapsed_ms = self.elapsed_ms();
        let first_audio_ms = self.first_audio.map_or_else(|| "-".to_owned(), |d| d.as_millis().to_string());
        let audio_ms = (self.audio_bytes * 1000).checked_div(self.bytes_per_sec).unwrap_or(0);
        let error = error.unwrap_or("-");
        let outcome = outcome.as_str();
        if outcome == Outcome::Completed.as_str() {
            tracing::info!(id = %self.id, outcome, elapsed_ms, first_audio_ms = %first_audio_ms, audio_ms, "終了");
        } else {
            tracing::warn!(id = %self.id, outcome, elapsed_ms, first_audio_ms = %first_audio_ms, audio_ms, error, "終了");
        }
    }

    /// 流した音声を数え、流し終えたとき・失敗したとき・捨てられたときに終了を記録するストリームにする
    pub fn track(mut self, inner: ByteStream, bytes_per_sec: u64) -> ByteStream {
        self.bytes_per_sec = bytes_per_sec;
        Box::pin(Tracked { inner, log: self })
    }
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        self.finish(Outcome::ClientDisconnected, None);
    }
}

struct Tracked {
    inner: ByteStream,
    log: RequestLog,
}

impl Stream for Tracked {
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let item = self.inner.as_mut().poll_next(cx);
        let log = &mut self.log;
        match &item {
            Poll::Ready(Some(Ok(chunk))) if !chunk.is_empty() => {
                if log.first_audio.is_none() {
                    log.first_audio = Some(log.started.elapsed());
                }
                log.audio_bytes += chunk.len() as u64;
            }
            Poll::Ready(Some(Err(e))) => log.finish(Outcome::StreamError, Some(&e.to_string())),
            Poll::Ready(None) => log.finish(Outcome::Completed, None),
            _ => {}
        }
        item
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::StreamExt;

    fn log() -> RequestLog {
        RequestLog::start(new_id(), Instant::now(), "-", 3, "default")
    }

    #[test]
    fn ids_are_unique_and_valid() {
        let (a, b) = (new_id(), new_id());
        assert_ne!(a, b);
        assert!(valid_id(&a) && valid_id(&b));
    }

    #[test]
    fn rejects_ids_that_could_break_log_lines() {
        assert!(valid_id("hanako-123_abc"));
        assert!(!valid_id(""));
        assert!(!valid_id("a b"));
        assert!(!valid_id("a\nb"));
        assert!(!valid_id(&"a".repeat(65)));
    }

    #[tokio::test]
    async fn counts_audio_until_the_end() {
        let inner: ByteStream = Box::pin(futures::stream::iter(vec![
            Ok(Bytes::from(vec![0u8; 100])),
            Ok(Bytes::new()),
            Ok(Bytes::from(vec![0u8; 92])),
        ]));
        let mut s = Box::pin(Tracked { inner, log: log() });
        while s.next().await.is_some() {}
        assert!(s.log.finished);
        assert_eq!(s.log.audio_bytes, 192);
        assert!(s.log.first_audio.is_some());
    }

    #[tokio::test]
    async fn records_stream_errors_once() {
        let inner: ByteStream = Box::pin(futures::stream::iter(vec![Err(std::io::Error::other("broken"))]));
        let mut s = Box::pin(Tracked { inner, log: log() });
        assert!(s.next().await.unwrap().is_err());
        assert!(s.log.finished);
    }

    #[test]
    fn dropping_without_finishing_counts_as_disconnect() {
        // 捨てても panic せず、終了の記録は 1 回だけ（finish 済みなら Drop では何もしない）
        let mut l = log();
        l.finish(Outcome::Rejected, Some("empty_text"));
        assert!(l.finished);
        drop(l);
        drop(log());
    }
}
