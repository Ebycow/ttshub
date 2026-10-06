# ttshub

複数の TTS エンジンを束ねて、読み上げ Bot やチャットボットに 1 つの API で音声合成を提供するゲートウェイ。

- 話者は `voicevox:ずんだもん/あまあま?speed=1.2` のようなアドレスで指定する
- エンジンごとの出力形式の違いを吸収し、要求された形式（Discord 向けの 48kHz ステレオ PCM など）で返す
- 同じエンジンの複数ホストへの振り分けと、止まったときの代わりの声へのフォールバック
- 全エンジンの話者一覧の検索と、あいまい照合（「ずんだもん あまあま」→ `voicevox:ずんだもん/あまあま`）

## 対応エンジン

| プラグイン | エンジン |
|---|---|
| `voicevox-compatible` | VOICEVOX、AivisSpeech などの VOICEVOX 互換エンジン |
| `ebyroid` | [ebyroid](https://github.com/Ebycow/ebyroid)（VOICEROID+ / VOICEROID2） |
| `aivis-cloud` | [Aivis Cloud API](https://api.aivis-project.com/v1/docs)（従量課金。使うモデルを `models` に UUID で並べる） |

VOICEVOX 互換のエンジンは、コードを変えずに設定ファイルに書き足すだけで使える。

## 使い方

```sh
cp ttshub.example.yaml ttshub.yaml        # 設定を作る
docker compose --profile voicevox up -d   # ttshub と VOICEVOX を起動
```

エンジンを別に動かしているなら profile は不要（`docker compose up -d`）。ebyroid も一緒に動かすなら `--profile ebyroid` を付ける（VOICEROID の用意が必要。[ebyroid の README](https://github.com/Ebycow/ebyroid) を参照）。

```sh
# 合成（48kHz ステレオの PCM が返る）
curl -X POST localhost:8790/v1/speech -H 'Content-Type: application/json' \
  -d '{"text": "こんにちは", "voice": "voicevox:ずんだもん/あまあま"}' -o out.pcm

# WAV で受け取る
curl -X POST localhost:8790/v1/speech -H 'Content-Type: application/json' \
  -d '{"text": "こんにちは", "voice": "zundamon", "format": {"codec": "wav"}}' -o out.wav

# 話者の検索とあいまい照合
curl -G localhost:8790/v1/voices --data-urlencode q=ずんだ
curl -G localhost:8790/v1/voices/resolve --data-urlencode 'q=ずんだもん あまあま'
```

## アドレス

```
<エンジン>:<キャラ>[/<スタイル>][?<パラメータ>]    voicevox:ずんだもん/あまあま?speed=1.2
preset:<プリセット名>                               preset:早口ずんだもん
<別名>                                              zundamon
default                                             利用者ごと、または全体のデフォルト
```

- スタイルを省略すると、そのキャラの先頭のスタイルになる
- キャラ名とスタイル名は、全角・半角と英字の大小を区別しない。`/` などの記号は `-` に置き換わるか削除される（`小夜/SAYO` → `小夜-sayo`）
- パラメータ（エンジンが対応しているものだけ効く）: `speed`（0.5〜2.0）、`pitch`（-0.15〜0.15）、`volume`（0.0〜2.0）、`intonation`（0.0〜2.0）。エンジン固有のものは `x-<プラグイン>-<キー>` の形で渡す（例: `x-voicevox-prePhonemeLength=0.2`）

## API

| エンドポイント | 内容 |
|---|---|
| `POST /v1/speech` | 合成。本文は `{ "text", "voice", "format": { "codec", "sample_rate", "channels" }, "fallback" }`。`codec` は `pcm_s16le`（既定）か `wav`。`fallback` を `none` にすると、代わりの声を使わずにエラーを返す。`deadline_ms` を付けると、受付からその時間内に音声を返し始める（間に合いそうになければ代わりの声に切り替え、それでも間に合わなければ `503`） |
| `GET /v1/voices` | 話者の一覧と検索（`q`, `engine`, `limit`, `cursor`） |
| `GET /v1/voices/resolve` | あいまい照合（`q`）。一致した話者と、見つからなかったときの候補を返す |
| `GET /v1/health` | ttshub の稼働確認 |
| `GET /admin/engines` | エンジンとバックエンドの状態 |
| `POST /admin/catalog/refresh` | 話者一覧の取り直し |

合成のレスポンスヘッダ:

| ヘッダ | 内容 |
|---|---|
| `X-TTS-Resolved-Voice` | 実際に使った話者 |
| `X-TTS-Fallback` | `none` / `backend_unavailable` / `engine_unavailable` / `unknown_voice`（代わりの声に切り替えた理由） |
| `X-TTS-Credit` | 必要なクレジット表記 |
| `X-TTS-Warning` | パラメータを丸めた・無視したなどの警告 |
| `X-TTS-PCM-Sample-Rate` / `-Bit-Depth` / `-Channels` | PCM のときの形式 |

日本語を含むヘッダの値は RFC 8187 形式（`UTF-8''<パーセントエンコード>`）。

## 設定

`ttshub.yaml`（例は [ttshub.example.yaml](ttshub.example.yaml)）。

| 項目 | 内容 |
|---|---|
| `default_voice` | 全体のデフォルト話者 |
| `engines.<名前>.plugin` | プラグインの種類 |
| `engines.<名前>.backends` | エンジンの URL。複数書くと振り分ける |
| `engines.<名前>.balance` | `round_robin`（既定）/ `least_connections` / `primary_backup` |
| `engines.<名前>.max_concurrency_per_backend` | バックエンドごとの同時合成数（既定 3）。1 件ずつしか合成しないエンジン（VOICEVOX など）は 1 にする。多くしてもエンジンの中で順番を待つだけで、ttshub から待ちが見えなくなる |
| `engines.<名前>.credits` | キャラ名 → クレジット表記。エンジンが申告する表記より優先する。申告のないエンジン（ebyroid）や、規約が表記を指定しないキャラに書く |
| `engines.<名前>.terms_url` | 利用規約の URL。エンジンが話者ごとの URL を申告しないときに使う |
| `engines.<名前>.default_voice` | そのエンジンのデフォルト話者（`キャラ[/スタイル]`） |
| `engines.<名前>.billable` | 課金のあるエンジン。フォールバック先にしない |
| `engines.<名前>.models` | 使う音声モデル（`aivis-cloud` ではモデルの UUID） |
| `engines.<名前>.api_key_env` | API キーを入れた環境変数名（クラウドのエンジン用） |
| `engines.<名前>.response_start_ms` | 応答開始までの上限（既定 15000）。音声を一括で返すエンジンは長めにする |
| `timeouts` | `queue_wait_ms`: 同時合成数の空きを待つ上限（既定 10000）。`response_start_ms`: 応答開始までの上限の既定（既定 15000）。`fallback_reserve_ms`: `deadline_ms` を指定されたとき、代わりの声を試すために残しておく時間（既定 2000） |
| `equivalents` | 同じキャラの別エンジンでの対応。エンジンが止まったときの代わりの声になる（同じスタイルがあればそれを使う） |
| `aliases` | 別名 → アドレス |
| `presets` | プリセット名 → パラメータ付きのアドレス |
| `clients` | 利用者ごとの鍵（`key_env` に鍵を入れた環境変数名）。書くとすべての API に `Authorization: Bearer <鍵>` が必要になる |
| `admin_key_env` | 管理 API の鍵を入れた環境変数名 |

鍵などの秘密は `.env` に書く。

## 開発

テストは Rust のコンテナで実行する（依存とビルド成果物は Docker ボリュームに置く）。

```sh
docker run --rm -v "$PWD":/src -w /src \
  -v ttshub-cargo-registry:/usr/local/cargo/registry -v ttshub-target:/target -e CARGO_TARGET_DIR=/target \
  rust:1-slim-bookworm cargo test
```
