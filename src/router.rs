//! 論理エンジンとバックエンド

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::Serialize;
use tokio::sync::{oneshot, Semaphore};
use tokio::time::timeout;

use crate::config::{Balance, EngineConfig, Timeouts};
use crate::engine::{self, AudioBody, BackendClient, ByteStream, Credit, EngineError, EnginePlugin, VoiceEntry};
use crate::params::Params;

/// 連続でこの回数失敗したら切り離す
const FAILS_TO_DOWN: u32 = 2;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
/// クレジットの問い合わせの上限（裏で行うので長くてよい。AivisSpeech は規約 1 件に数秒かかる）
const CREDITS_TIMEOUT: Duration = Duration::from_secs(300);
/// 同時合成数の空き待ちがこれより長かったら記録する
const SLOW_QUEUE_WAIT: Duration = Duration::from_secs(1);
/// 打ち切った合成をエンジンがまだ計算しているとき、この間隔で記録する
const OVERRUN_REPORT_EVERY: Duration = Duration::from_secs(60);

pub struct Backend {
    pub url: String,
    client: BackendClient,
    healthy: AtomicBool,
    fails: AtomicU32,
    active: Arc<AtomicUsize>,
    /// 打ち切った（利用者がもう待っていない）合成をエンジンがまだ計算しているものの、打ち切った時刻
    overrun: Arc<Mutex<Vec<Instant>>>,
    slots: Arc<Semaphore>,
    last_error: Mutex<Option<String>>,
}

impl Backend {
    fn mark_ok(&self) {
        self.fails.store(0, Ordering::Relaxed);
        if !self.healthy.swap(true, Ordering::Relaxed) {
            tracing::info!(backend = %self.url, "バックエンドが回復");
        }
    }

    fn mark_fail(&self, reason: &str) {
        *self.last_error.lock().unwrap() = Some(reason.to_owned());
        let fails = self.fails.fetch_add(1, Ordering::Relaxed) + 1;
        if fails >= FAILS_TO_DOWN && self.healthy.swap(false, Ordering::Relaxed) {
            tracing::warn!(backend = %self.url, reason, "バックエンドを切り離す");
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// 打ち切った合成をエンジンがまだ計算している数
    fn overrun_count(&self) -> usize {
        self.overrun.lock().unwrap().len()
    }
}

#[derive(Serialize)]
pub struct BackendStatus {
    pub url: String,
    pub healthy: bool,
    pub active: usize,
    pub overrun: usize,
    /// 打ち切った合成のうち一番古いものの、打ち切ってからの経過時間
    pub overrun_oldest_ms: Option<u64>,
    pub last_error: Option<String>,
}

pub struct Engine {
    pub name: String,
    pub config: EngineConfig,
    pub plugin: Arc<dyn EnginePlugin>,
    pub backends: Vec<Arc<Backend>>,
    rr: AtomicUsize,
}

impl Engine {
    pub fn new(name: &str, config: EngineConfig, http: reqwest::Client) -> Result<Self, String> {
        let plugin = engine::create(&config)?;
        Ok(Self::with_plugin(name, config, http, plugin))
    }

    pub(crate) fn with_plugin(name: &str, config: EngineConfig, http: reqwest::Client, plugin: Arc<dyn EnginePlugin>) -> Self {
        let api_key = match &config.api_key_env {
            Some(env) => match std::env::var(env) {
                Ok(k) if !k.is_empty() => Some(k),
                _ => {
                    tracing::warn!(engine = name, env, "API キーの環境変数がないため、認証なしで接続する");
                    None
                }
            },
            None => None,
        };
        let backends = config
            .backends
            .iter()
            .map(|url| {
                Arc::new(Backend {
                    url: url.clone(),
                    client: BackendClient { base: url.clone(), http: http.clone(), api_key: api_key.clone() },
                    // 最初のヘルスチェックまでは使える前提で扱う
                    healthy: AtomicBool::new(true),
                    fails: AtomicU32::new(0),
                    active: Arc::new(AtomicUsize::new(0)),
                    overrun: Arc::new(Mutex::new(Vec::new())),
                    slots: Arc::new(Semaphore::new(config.max_concurrency_per_backend)),
                    last_error: Mutex::new(None),
                })
            })
            .collect();
        Self { name: name.to_owned(), config, plugin, backends, rr: AtomicUsize::new(0) }
    }

    /// 試す順のバックエンド。振り分け方で並べたうえで、稼働中のものを先にする
    fn order(&self) -> Vec<Arc<Backend>> {
        let n = self.backends.len();
        let mut list: Vec<Arc<Backend>> = match self.config.balance {
            Balance::PrimaryBackup => self.backends.clone(),
            Balance::RoundRobin => {
                let start = self.rr.fetch_add(1, Ordering::Relaxed) % n;
                (0..n).map(|i| self.backends[(start + i) % n].clone()).collect()
            }
            Balance::LeastConnections => {
                let mut l = self.backends.clone();
                l.sort_by_key(|b| b.active.load(Ordering::Relaxed));
                l
            }
        };
        list.sort_by_key(|b| !b.is_healthy());
        list
    }

    fn response_timeout(&self, t: &Timeouts) -> Duration {
        Duration::from_millis(self.config.response_start_ms.unwrap_or(t.response_start_ms))
    }

    /// 合成する。接続できない・時間切れなどのときは同じエンジンの別のバックエンドで再試行する。
    /// `until` があれば、空き待ちも応答開始の待ちもその時刻で打ち切る（利用者が待てる時間）。
    /// 戻り値: (音声, 使ったバックエンド, 最初に試したバックエンドから切り替えたか)
    pub async fn synthesize(
        &self,
        voice: &VoiceEntry,
        text: &str,
        params: &Params,
        t: &Timeouts,
        until: Option<Instant>,
    ) -> Result<(AudioBody, String, bool), EngineError> {
        let mut last = EngineError::Unavailable("バックエンドがない".into());
        // 期限までの残り。期限がなければ上限なし
        let left = || until.map(|u| u.saturating_duration_since(Instant::now()));
        // ここで記録するものに本文を含めない（エラーの文言は URL を除いてある）
        for (i, b) in self.order().into_iter().enumerate() {
            if left().is_some_and(|d| d.is_zero()) {
                return Err(EngineError::Unavailable("期限までに合成を始められなかった".into()));
            }
            // 打ち切った合成をまだ計算しているエンジンは、いつ空くか分からない。期限のある合成は待たせずに次へ回す
            if until.is_some() && b.overrun_count() > 0 {
                tracing::info!(backend = %b.url, "打ち切った合成をまだ計算中のため、このバックエンドを飛ばす");
                last = EngineError::Unavailable(format!("{}: 打ち切った合成をまだ計算中", b.url));
                continue;
            }
            let queue_wait = Duration::from_millis(t.queue_wait_ms).min(left().unwrap_or(Duration::MAX));
            let waiting = Instant::now();
            let permit = match timeout(queue_wait, b.slots.clone().acquire_owned()).await {
                Ok(Ok(p)) => p,
                _ => {
                    let wait_ms = queue_wait.as_millis() as u64;
                    tracing::warn!(backend = %b.url, wait_ms, "同時合成数の空き待ちが時間切れ");
                    last = EngineError::Unavailable(format!("{}: 同時合成数の空き待ちが時間切れ", b.url));
                    continue;
                }
            };
            let waited = waiting.elapsed();
            if waited >= SLOW_QUEUE_WAIT {
                tracing::warn!(backend = %b.url, wait_ms = waited.as_millis() as u64, "同時合成数の空き待ちが長かった");
            }
            let guard = (permit, ActiveGuard::new(b.active.clone()));
            let synthesizing = Instant::now();
            let elapsed_ms = || synthesizing.elapsed().as_millis() as u64;
            let engine_limit = self.response_timeout(t);
            let rx = self.spawn_synthesis(&b, voice, text, params, guard);
            // 利用者を待たせるのは、期限の残りかエンジンの上限（response_start_ms）の短いほうまで。
            // 期限で打ち切ったときの時間切れはバックエンドのせいにしない。
            // どちらで打ち切っても合成は裏で続き、エンジンが応答するまで枠を持つ（spawn_synthesis）
            let by_deadline = left().is_some_and(|d| d < engine_limit);
            let wait = left().map_or(engine_limit, |d| d.min(engine_limit));
            let received = match timeout(wait, rx).await {
                Ok(r) => r,
                Err(_) if by_deadline => {
                    tracing::warn!(backend = %b.url, elapsed_ms = elapsed_ms(), "期限までにバックエンドが応答を始めなかった");
                    return Err(EngineError::Unavailable(format!("{}: 期限までに応答が始まらなかった", b.url)));
                }
                Err(_) => {
                    tracing::warn!(backend = %b.url, elapsed_ms = elapsed_ms(), "バックエンドの応答開始が時間切れ");
                    b.mark_fail("応答開始の時間切れ");
                    last = EngineError::Unavailable(format!("{}: 応答開始の時間切れ", b.url));
                    continue;
                }
            };
            match received.unwrap_or_else(|_| Err(EngineError::Unavailable("合成のタスクが異常終了した".into()))) {
                Ok(body) => {
                    b.mark_ok();
                    return Ok((body, b.url.clone(), i > 0));
                }
                Err(EngineError::Rejected(m)) => {
                    tracing::warn!(backend = %b.url, error = %m, elapsed_ms = elapsed_ms(), "エンジンが要求を受け付けなかった");
                    return Err(EngineError::Rejected(m));
                }
                Err(e) => {
                    tracing::warn!(backend = %b.url, error = %e, elapsed_ms = elapsed_ms(), "バックエンドで合成に失敗");
                    b.mark_fail(&e.to_string());
                    last = e;
                }
            }
        }
        Err(last)
    }

    /// 合成を別のタスクで行い、結果を返す受け口を返す。同時合成数の枠（guard）はタスクが持つ。
    /// 期限や上限で受け口が捨てられても、エンジンは計算を続ける（VOICEVOX は途中で止められない）。
    /// そのため合成は捨てずに続け、上限を設けずにエンジンが応答するまで枠を持つ。
    /// 途中で枠を返すと計算中のエンジンが空いて見え、後から来た合成をエンジンの中で待たせてしまう。
    /// エンジンが固まって応答しなければ枠も戻らないが、それはエンジンが実際に使えない状態なので、そのまま見せる
    /// （エンジンを再起動すれば接続が切れて戻る）。
    /// 応答が始まったら、枠は音声を受け取り終えるまで持つ（release_when_received）
    fn spawn_synthesis<G: Send + 'static>(
        &self,
        b: &Arc<Backend>,
        voice: &VoiceEntry,
        text: &str,
        params: &Params,
        guard: G,
    ) -> oneshot::Receiver<Result<AudioBody, EngineError>> {
        let (mut tx, rx) = oneshot::channel();
        let (plugin, b) = (self.plugin.clone(), b.clone());
        let (voice, text, params) = (voice.clone(), text.to_owned(), params.clone());
        tokio::spawn(async move {
            let work = async { plugin.synthesize(&b.client, &voice, &text, &params).await };
            tokio::pin!(work);
            let result = tokio::select! {
                r = &mut work => r,
                _ = tx.closed() => {
                    // 打ち切られた。エンジンが計算を終えるまで、枠と「打ち切った合成を計算中」の印を持つ
                    let overrun = OverrunGuard::new(b.overrun.clone());
                    let r = loop {
                        match timeout(OVERRUN_REPORT_EVERY, &mut work).await {
                            Ok(r) => break r,
                            Err(_) => tracing::warn!(
                                backend = %b.url,
                                elapsed_ms = overrun.elapsed_ms(),
                                "打ち切った合成をエンジンがまだ計算している。終わるまで枠を持つ"
                            ),
                        }
                    };
                    // 応答が始まっていれば、受け取らずに接続を切る（流しながら返すエンジンはここで止まる）
                    let elapsed_ms = overrun.elapsed_ms();
                    match r {
                        Ok(_) => tracing::info!(backend = %b.url, elapsed_ms, "打ち切った合成をエンジンが終えた。枠を返す"),
                        Err(e) => tracing::warn!(backend = %b.url, elapsed_ms, error = %e, "打ち切った合成がエンジンで失敗した。枠を返す"),
                    }
                    return;
                }
            };
            let _ = tx.send(result.map(|body| body.map_stream(|s| release_when_received(s, guard))));
        });
        rx
    }

    /// 話者一覧を、稼働中のバックエンドから記述順に試して取得する
    pub async fn list_voices(&self) -> Result<Vec<VoiceEntry>, EngineError> {
        let mut list = self.backends.clone();
        list.sort_by_key(|b| !b.is_healthy());
        let mut last = EngineError::Unavailable("バックエンドがない".into());
        for b in list {
            match timeout(Duration::from_secs(10), self.plugin.list_voices(&b.client)).await {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(e)) => last = e,
                Err(_) => last = EngineError::Unavailable(format!("{}: 話者一覧の取得が時間切れ", b.url)),
            }
        }
        Err(last)
    }

    /// キャラ名 → クレジットを、稼働中のバックエンドから記述順に試して取得する
    pub async fn credits(&self, characters: &[String]) -> Result<HashMap<String, Credit>, EngineError> {
        let mut list = self.backends.clone();
        list.sort_by_key(|b| !b.is_healthy());
        let mut last = EngineError::Unavailable("バックエンドがない".into());
        for b in list {
            match timeout(CREDITS_TIMEOUT, self.plugin.credits(&b.client, characters)).await {
                Ok(Ok(c)) => return Ok(c),
                Ok(Err(e)) => last = e,
                Err(_) => last = EngineError::Unavailable(format!("{}: クレジットの取得が時間切れ", b.url)),
            }
        }
        Err(last)
    }

    pub async fn check_health(&self) {
        for b in &self.backends {
            let ok = timeout(HEALTH_TIMEOUT, self.plugin.health(&b.client)).await.unwrap_or(false);
            if ok {
                b.mark_ok();
            } else {
                b.mark_fail("ヘルスチェック失敗");
            }
        }
    }

    pub fn is_available(&self) -> bool {
        self.backends.iter().any(|b| b.is_healthy())
    }

    pub fn status(&self) -> Vec<BackendStatus> {
        self.backends
            .iter()
            .map(|b| BackendStatus {
                url: b.url.clone(),
                healthy: b.is_healthy(),
                active: b.active.load(Ordering::Relaxed),
                overrun: b.overrun_count(),
                overrun_oldest_ms: b.overrun.lock().unwrap().iter().min().map(|t| t.elapsed().as_millis() as u64),
                last_error: b.last_error.lock().unwrap().clone(),
            })
            .collect()
    }
}

struct ActiveGuard(Arc<AtomicUsize>);

impl ActiveGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 打ち切った合成を計算している間、打ち切った時刻をバックエンドに載せておく
struct OverrunGuard {
    list: Arc<Mutex<Vec<Instant>>>,
    started: Instant,
}

impl OverrunGuard {
    fn new(list: Arc<Mutex<Vec<Instant>>>) -> Self {
        let started = Instant::now();
        list.lock().unwrap().push(started);
        Self { list, started }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

impl Drop for OverrunGuard {
    fn drop(&mut self) {
        let mut list = self.list.lock().unwrap();
        if let Some(i) = list.iter().position(|t| *t == self.started) {
            list.swap_remove(i);
        }
    }
}

/// エンジンからの受信を利用者への送信と切り離し、エンジンが送り終えたら guard を手放す。
/// 利用者は音声をゆっくり読むことがある（hanako は再生の速さで読む）。それに合わせて枠を持ち続けると、
/// 計算を終えたエンジンの枠がふさがり、ほかの利用者が待たされる。
/// 受け取った音声は利用者が読むまでメモリに溜める（本文の長さは max_text_chars で抑えている）。
/// 利用者が切断したら受信をやめ、エンジンへの接続も切る
fn release_when_received<G: Send + 'static>(mut s: ByteStream, guard: G) -> ByteStream {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _held = guard;
        loop {
            tokio::select! {
                item = s.next() => {
                    let Some(item) = item else { break };
                    let failed = item.is_err();
                    if tx.send(item).is_err() || failed {
                        break;
                    }
                }
                _ = tx.closed() => break,
            }
        }
    });
    Box::pin(futures::stream::poll_fn(move |cx| rx.poll_recv(cx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    /// 破棄されたら印を付ける
    struct Flag(Arc<AtomicBool>);

    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn releases_guard_when_engine_finishes_even_if_nobody_reads() {
        let released = Arc::new(AtomicBool::new(false));
        let chunks = vec![Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"cd"))];
        let out = release_when_received(Box::pin(futures::stream::iter(chunks)), Flag(released.clone()));
        settle().await;
        assert!(released.load(Ordering::Relaxed), "読まれる前に枠を手放す");

        let got: Vec<Bytes> = out.map(|r| r.unwrap()).collect().await;
        assert_eq!(got, vec![Bytes::from_static(b"ab"), Bytes::from_static(b"cd")]);
    }

    #[tokio::test]
    async fn stops_receiving_when_client_disconnects() {
        let released = Arc::new(AtomicBool::new(false));
        let out = release_when_received(Box::pin(futures::stream::pending()), Flag(released.clone()));
        settle().await;
        assert!(!released.load(Ordering::Relaxed), "エンジンが送っている間は枠を持つ");
        drop(out);
        settle().await;
        assert!(released.load(Ordering::Relaxed), "利用者が切断したら受信をやめて枠を手放す");
    }

    #[tokio::test]
    async fn passes_engine_error_and_stops() {
        let released = Arc::new(AtomicBool::new(false));
        let chunks = vec![Ok(Bytes::from_static(b"ab")), Err(std::io::Error::other("切れた"))];
        let out = release_when_received(Box::pin(futures::stream::iter(chunks)), Flag(released.clone()));
        let got: Vec<bool> = out.map(|r| r.is_ok()).collect().await;
        assert_eq!(got, vec![true, false]);
        assert!(released.load(Ordering::Relaxed));
    }
}
