//! ttshub-openjtalk: Open JTalk を HTTP で呼べるようにする、ttshub 用のエンジンのサーバー
//!
//! 環境変数（括弧内は既定値）: `OPENJTALK_LISTEN`（0.0.0.0:8791）、`OPENJTALK_BIN`（open_jtalk）、
//! `OPENJTALK_DIC`（/var/lib/mecab/dic/open-jtalk/naist-jdic）、`OPENJTALK_VOICE_DIR`（/usr/share/hts-voice）

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing_subscriber::EnvFilter;
use ttshub_openjtalk::OpenJTalk;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let listen = std::env::var("OPENJTALK_LISTEN").unwrap_or_else(|_| "0.0.0.0:8791".into());

    // コンテナのヘルスチェック用（イメージに curl を入れずに済ませる）
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        let port = listen.rsplit(':').next().unwrap_or("8791");
        std::process::exit(healthcheck(&format!("127.0.0.1:{port}")).await);
    }

    let engine = Arc::new(OpenJTalk::from_env());
    if !engine.healthy() {
        // 声をあとから置くこともあるので、止めずに 503 を返し続ける
        tracing::warn!(
            "open_jtalk・辞書・声のどれかが見つからない（OPENJTALK_BIN、OPENJTALK_DIC、OPENJTALK_VOICE_DIR）"
        );
    }
    let listener = match tokio::net::TcpListener::bind(&listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(listen, "待ち受けできない: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(listen, voices = engine.voices().len(), "ttshub-openjtalk を起動");
    axum::serve(listener, ttshub_openjtalk::router(engine)).with_graceful_shutdown(shutdown_signal()).await.unwrap();
}

/// GET /health が 200 なら 0
async fn healthcheck(addr: &str) -> i32 {
    let check = async {
        let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
        stream.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n").await.ok()?;
        let mut buf = [0; 16];
        let n = stream.read(&mut buf).await.ok()?;
        let line = String::from_utf8_lossy(&buf[..n]).into_owned();
        Some(line.split_whitespace().nth(1) == Some("200"))
    };
    match tokio::time::timeout(Duration::from_secs(3), check).await {
        Ok(Some(true)) => 0,
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
