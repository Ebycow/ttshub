//! テスト用の偽の open_jtalk と声（ttshub のテストからも使う）
//!
//! 偽の open_jtalk は、読んだ文のバイト数だけサンプルを持つ 48kHz モノラルの WAV を出す。
//! 「失敗」を含む文では失敗し、「！！」を含む文では、読める文字がないときと同じエラーを出す

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::OpenJTalk;

const SCRIPT: &str = r#"#!/bin/sh
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
"#;

/// 一時ディレクトリに置いた偽の open_jtalk、辞書、声（mei の normal と happy、英語の声）。消すと片付く
pub struct FakeOpenJTalk {
    dir: PathBuf,
}

impl FakeOpenJTalk {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ttshub-openjtalk-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let script = dir.join("open_jtalk");
        std::fs::write(&script, SCRIPT).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let voices = dir.join("voices");
        std::fs::create_dir_all(voices.join("mei")).unwrap();
        let header =
            |format: &str| format!("[GLOBAL]\nSAMPLING_FREQUENCY:48000\nFULLCONTEXT_FORMAT:{format}\n[STREAM]\n");
        for style in ["mei_happy", "mei_normal"] {
            std::fs::write(voices.join(format!("mei/{style}.htsvoice")), header("HTS_TTS_JPN")).unwrap();
        }
        std::fs::write(voices.join("english.htsvoice"), header("HTS_TTS_ENG")).unwrap();
        std::fs::write(dir.join("sys.dic"), "").unwrap();
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn engine(&self) -> OpenJTalk {
        OpenJTalk::new(self.dir.join("open_jtalk"), self.dir.clone(), self.dir.join("voices"))
    }

    /// サーバーを立てて、その URL を返す
    pub async fn serve(&self) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::router(Arc::new(self.engine()));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }
}

impl Drop for FakeOpenJTalk {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
