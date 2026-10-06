//! ttshub: 複数の TTS エンジンを束ねる音声合成ゲートウェイ

mod address;
mod api;
mod audio;
mod catalog;
mod config;
mod engine;
mod hub;
mod params;
mod reqlog;
mod router;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::hub::Hub;

/// 起動時にエンジンの応答を待つ上限。応答がなくても起動し、あとで取り直す
const STARTUP_WAIT: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    // コンテナのヘルスチェック用（イメージに curl を入れずに済ませる）
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(healthcheck().await);
    }

    let path: PathBuf = std::env::var("TTSHUB_CONFIG").unwrap_or_else(|_| "/etc/ttshub/ttshub.yaml".into()).into();
    let config = match Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}");
            std::process::exit(1);
        }
    };
    let listen = config.listen.clone();
    let hub = match Hub::new(config) {
        Ok(h) => Arc::new(h),
        Err(e) => {
            tracing::error!("{e}");
            std::process::exit(1);
        }
    };

    hub.load_snapshots();
    let initial = async {
        check_health(&hub).await;
        hub.refresh_all().await;
    };
    if tokio::time::timeout(STARTUP_WAIT, initial).await.is_err() {
        tracing::warn!("起動時のエンジン確認が時間内に終わらなかった。バックグラウンドで続ける");
    }
    let h = hub.clone();
    tokio::spawn(async move { h.fill_all_credits().await });
    spawn_background(hub.clone());

    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(listen, "待ち受けできない: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(listen, engines = ?hub.order, "ttshub を起動");
    axum::serve(listener, api::router(hub)).with_graceful_shutdown(shutdown_signal()).await.unwrap();
}

async fn check_health(hub: &Hub) {
    futures::future::join_all(hub.engines.values().map(|e| e.check_health())).await;
}

fn spawn_background(hub: Arc<Hub>) {
    let h = hub.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(h.config.timeouts.health_interval_ms));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            check_health(&h).await;
            // カタログがまだ取れていないエンジンが応答するようになったら、すぐ取る
            for (name, engine) in &h.engines {
                if h.catalog.engine(name).is_none() && engine.is_available() {
                    if let Ok(n) = h.refresh_catalog(name).await {
                        tracing::info!(engine = %name, voices = n, "カタログを取得");
                        let (h, name) = (h.clone(), name.clone());
                        tokio::spawn(async move { h.fill_credits(&name).await });
                    }
                }
            }
        }
    });
    tokio::spawn(async move {
        let period = Duration::from_secs(hub.config.timeouts.catalog_refresh_secs);
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            hub.refresh_all().await;
            hub.fill_all_credits().await;
        }
    });
}

async fn healthcheck() -> i32 {
    let url = std::env::var("TTSHUB_HEALTH_URL").unwrap_or_else(|_| "http://127.0.0.1:8790/v1/health".into());
    let client = reqwest::Client::builder().timeout(Duration::from_secs(3)).build().unwrap();
    match client.get(url).send().await {
        Ok(r) if r.status().is_success() => 0,
        _ => 1,
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("終了します");
}
