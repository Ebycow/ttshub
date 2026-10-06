//! 論理エンジンとバックエンド

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::time::timeout;

use crate::config::{Balance, EngineConfig, Timeouts};
use crate::engine::{self, AudioBody, BackendClient, ByteStream, EngineError, EnginePlugin, VoiceEntry};
use crate::params::Params;

/// 連続でこの回数失敗したら切り離す
const FAILS_TO_DOWN: u32 = 2;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
/// 同時合成数の空き待ちがこれより長かったら記録する
const SLOW_QUEUE_WAIT: Duration = Duration::from_secs(1);

pub struct Backend {
    pub url: String,
    client: BackendClient,
    healthy: AtomicBool,
    fails: AtomicU32,
    active: Arc<AtomicUsize>,
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
}

#[derive(Serialize)]
pub struct BackendStatus {
    pub url: String,
    pub healthy: bool,
    pub active: usize,
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
                    slots: Arc::new(Semaphore::new(config.max_concurrency_per_backend)),
                    last_error: Mutex::new(None),
                })
            })
            .collect();
        Ok(Self { name: name.to_owned(), config, plugin, backends, rr: AtomicUsize::new(0) })
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
    /// 戻り値: (音声, 使ったバックエンド, 最初に試したバックエンドから切り替えたか)
    pub async fn synthesize(
        &self,
        voice: &VoiceEntry,
        text: &str,
        params: &Params,
        t: &Timeouts,
    ) -> Result<(AudioBody, String, bool), EngineError> {
        let mut last = EngineError::Unavailable("バックエンドがない".into());
        // ここで記録するものに本文を含めない（エラーの文言は URL を除いてある）
        for (i, b) in self.order().into_iter().enumerate() {
            let waiting = Instant::now();
            let permit = match timeout(Duration::from_millis(t.queue_wait_ms), b.slots.clone().acquire_owned()).await {
                Ok(Ok(p)) => p,
                _ => {
                    tracing::warn!(backend = %b.url, wait_ms = t.queue_wait_ms, "同時合成数の空き待ちが時間切れ");
                    last = EngineError::Unavailable(format!("{}: 同時合成数の空き待ちが時間切れ", b.url));
                    continue;
                }
            };
            let waited = waiting.elapsed();
            if waited >= SLOW_QUEUE_WAIT {
                tracing::warn!(backend = %b.url, wait_ms = waited.as_millis() as u64, "同時合成数の空き待ちが長かった");
            }
            let active = ActiveGuard::new(b.active.clone());
            let synthesizing = Instant::now();
            let elapsed_ms = || synthesizing.elapsed().as_millis() as u64;
            match timeout(self.response_timeout(t), self.plugin.synthesize(&b.client, voice, text, params)).await {
                Ok(Ok(body)) => {
                    b.mark_ok();
                    // 同時合成数の枠は、音声を最後まで流し終えるまで持ち続ける
                    let body = body.map_stream(|s| hold(s, (permit, active)));
                    return Ok((body, b.url.clone(), i > 0));
                }
                Ok(Err(EngineError::Rejected(m))) => {
                    tracing::warn!(backend = %b.url, error = %m, elapsed_ms = elapsed_ms(), "エンジンが要求を受け付けなかった");
                    return Err(EngineError::Rejected(m));
                }
                Ok(Err(e)) => {
                    tracing::warn!(backend = %b.url, error = %e, elapsed_ms = elapsed_ms(), "バックエンドで合成に失敗");
                    b.mark_fail(&e.to_string());
                    last = e;
                }
                Err(_) => {
                    tracing::warn!(backend = %b.url, elapsed_ms = elapsed_ms(), "バックエンドの応答開始が時間切れ");
                    b.mark_fail("応答開始の時間切れ");
                    last = EngineError::Unavailable(format!("{}: 応答開始の時間切れ", b.url));
                }
            }
        }
        Err(last)
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

/// ストリームが破棄されるまで guard を持ち続ける
fn hold<G: Send + 'static>(s: ByteStream, guard: G) -> ByteStream {
    Box::pin(s.map(move |item| {
        let _held = &guard;
        item
    }))
}
