//! 設定ファイル（ttshub.yaml）

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// 全体のデフォルト話者（アドレス）
    pub default_voice: String,
    /// 論理エンジン。記述順がカタログの並び順になる
    pub engines: IndexMap<String, EngineConfig>,
    /// 同じキャラの別エンジンでの対応（フォールバックに使う）
    #[serde(default)]
    pub equivalents: Vec<Vec<String>>,
    /// 別名 → アドレス
    #[serde(default)]
    pub aliases: IndexMap<String, String>,
    /// プリセット名 → パラメータ付きアドレス
    #[serde(default)]
    pub presets: IndexMap<String, String>,
    /// 利用者。1 つでも書けば、すべての API に鍵が必要になる
    #[serde(default)]
    pub clients: HashMap<String, ClientConfig>,
    /// 管理 API の鍵を入れた環境変数。なければ管理 API は鍵なしで使える
    #[serde(default)]
    pub admin_key_env: Option<String>,
    /// カタログのスナップショットなどを置く場所
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_max_text_chars")]
    pub max_text_chars: usize,
    #[serde(default)]
    pub timeouts: Timeouts,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    /// プラグインの種類（ebyroid, voicevox-compatible）
    pub plugin: String,
    #[serde(default)]
    pub balance: Balance,
    pub backends: Vec<String>,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency_per_backend: usize,
    /// 課金のあるエンジン。フォールバック先にしない
    #[serde(default)]
    pub billable: bool,
    /// クレジット表記のテンプレート。{character} と {style} を置き換える
    #[serde(default)]
    pub credit_template: Option<String>,
    #[serde(default)]
    pub terms_url: Option<String>,
    /// このエンジンのデフォルト話者（`キャラ[/スタイル]`）。なければエンジンが申告したもの、それもなければ先頭
    #[serde(default)]
    pub default_voice: Option<String>,
    /// エンジンが話者一覧を返せないときに使う一覧（古い ebyroid など）
    #[serde(default)]
    pub voices: Vec<String>,
    /// 使う音声モデル（Aivis Cloud API のモデル UUID など、モデルを選んで使うエンジン用）
    #[serde(default)]
    pub models: Vec<String>,
    /// API キーを入れた環境変数（クラウド TTS 用）
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// 応答開始までの上限をこのエンジンだけ変える。一括で返すエンジン（VOICEVOX）は長文だと時間がかかる
    #[serde(default)]
    pub response_start_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Balance {
    #[default]
    RoundRobin,
    LeastConnections,
    PrimaryBackup,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// 鍵を入れた環境変数
    pub key_env: String,
    #[serde(default)]
    pub default_voice: Option<String>,
    /// 課金のあるエンジンへのフォールバックを許すか
    #[serde(default)]
    pub allow_billable_fallback: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    /// エンジンが応答を始めるまでの上限
    pub response_start_ms: u64,
    /// バックエンドの同時合成数の空きを待つ上限
    pub queue_wait_ms: u64,
    /// ヘルスチェックの間隔
    pub health_interval_ms: u64,
    /// カタログを取り直す間隔
    pub catalog_refresh_secs: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self { response_start_ms: 15_000, queue_wait_ms: 10_000, health_interval_ms: 10_000, catalog_refresh_secs: 600 }
    }
}

fn default_listen() -> String {
    "0.0.0.0:8790".into()
}

fn default_state_dir() -> PathBuf {
    "/var/lib/ttshub".into()
}

fn default_max_text_chars() -> usize {
    2000
}

fn default_max_concurrency() -> usize {
    3
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("設定ファイル {0} を読めない: {1}")]
    Read(PathBuf, std::io::Error),
    #[error("設定ファイルの形式が正しくない: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("設定が正しくない: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Read(path.to_owned(), e))?;
        let config: Config = serde_yaml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.engines.is_empty() {
            return Err(ConfigError::Invalid("engines が空".into()));
        }
        for (name, engine) in &self.engines {
            if name.is_empty() || name.contains([':', '/', '?']) || name == "preset" {
                return Err(ConfigError::Invalid(format!("エンジン名 {name:?} は使えない")));
            }
            if engine.backends.is_empty() {
                return Err(ConfigError::Invalid(format!("エンジン {name} の backends が空")));
            }
            if engine.max_concurrency_per_backend == 0 {
                return Err(ConfigError::Invalid(format!("エンジン {name} の max_concurrency_per_backend が 0")));
            }
        }
        Ok(())
    }
}
