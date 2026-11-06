# aws-cli Rust 書き換え方針 (awsr)

## 1. 現状分析 (aws-cli v2 / Python)

| 領域 | Python 実装 | 規模・特徴 |
|---|---|---|
| エントリ | `awscli/clidriver.py` (`CLIDriver` → `ServiceCommand` → `ServiceOperation` → `CLIOperationCaller`) | コマンド表はモデルから動的生成 |
| サービスモデル | `awscli/botocore/data/<svc>/<ver>/service-2.json` ほか | 436 サービス / 114 MB の非圧縮 JSON |
| 引数 | `argprocess.py`, `shorthand.py`, `paramfile.py` | `xform_name` で kebab-case 化 (上書き表 約 88 件) |
| 認証情報 | `botocore/credentials.py` | 12 段のプロバイダチェーン |
| 署名 | `botocore/auth.py` | SigV4 / S3 SigV4 / SigV4a (awscrt) / Bearer |
| シリアライズ | `botocore/serialize.py` | json, rest-json, rest-xml, query, ec2, smithy-rpc-v2-cbor |
| エンドポイント | `botocore/endpoint_provider.py` (約 740 行) | 汎用ルールエンジン + `partitions.json` |
| 出力 | `awscli/formatter.py` | json / text / table / yaml, JMESPath `--query` |
| カスタマイズ | `awscli/customizations/` (75 個) | `s3` (7.6k 行), `configure`, `sso` など。イベントハンドラで引数を改名・削除 |

プロトコル別サービス数: rest-json 259 / json 130 / query 17 / rest-xml 4 / ec2 1 / cbor 専用 少数

## 2. 基本方針

1. **データ駆動コアを先に移植する。** botocore の JSON モデルをそのまま読み込み、436 サービスを一括で扱う。サービスごとのコード生成はしない。
2. **モデルは実行時に読み込む (MVP)。** `AWSR_DATA_DIR` で指定し、未指定なら `reference/aws-cli/awscli/botocore/data` を使う。バイナリ埋め込み (圧縮 + 事前変換) は後のフェーズで行う。
3. **公式テスト資産で互換性を確認する。** botocore の Apache-2.0 テストフィクスチャを `tests/fixtures/` にコピーし、データ駆動テストにする。
4. **I/O を分離する。** HTTP は `Transport` トレイトに隠す。テストでは偽の Transport で送信リクエストを検証するので、ポートのバインドは不要。
5. **単一クレート (lib + bin)。** 規模が大きくなったらワークスペースに分割する。

## 3. アーキテクチャ (実装済み)

```
src/
  bin/awsr.rs        エントリポイント (実 Transport・環境変数・時計を注入)
  driver.rs          コマンド解決 → パラメータ構築 → リージョン/認証/エンドポイント → 送信 → 出力
  naming.rs          xform_name (+ 本家の上書き表 44 件)
  model.rs           ServiceModel / Operation / Shape (生 JSON 上のゼロコピービュー)
  loader.rs          データディレクトリ探索, コマンド名の改名, S3/S3Control のモデル書き換え
  value.rs           パラメータ値 (Bytes / Timestamp を持つ独自型)
  timestamp.rs       iso8601 / rfc822 / unixTimestamp, Python isoformat 互換出力
  endpoint/mod.rs    エンドポイントルールエンジン (標準関数 12 種)
  sigv4.rs           AWS Signature V4 (通常 / S3 フレーバー)
  http.rs            Request / Response, Transport トレイト, ureq 実装
  protocol/
    ser.rs           query / ec2 / json / rest-json / rest-xml シリアライザ
    parse.rs         同 5 プロトコルのレスポンス・エラーパーサ
    xml.rs           quick-xml ベースの XML ツリー
  config.rs          ~/.aws/config, ~/.aws/credentials (INI + ネスト)
  context.rs         外部依存 (Transport, env, 時計, 入力, スリープ, ブラウザ, TTY) の注入口
  client.rs          モデル駆動の API クライアント (driver と認証情報プロバイダが共用)
  commands.rs        login, sso login / sso logout
  oauth.rs           PKCE, 認可コードのローカルコールバック受信 (127.0.0.1), JWT クレーム読み取り
  credentials/
    mod.rs           チェーン本体, assume-role (source_profile / credential_source / MFA), web identity
    sso.rs           SSO プロバイダ, トークン更新 (SSO-OIDC), デバイスコードログイン, logout
    login.rs         aws login セッション (キャッシュ読込 + DPoP による更新)
    metadata.rs      コンテナ認証情報 (ホスト制限付き), IMDSv2 (+ IMDS リージョン)
    process.rs       credential_process
    cache.rs         AWS CLI 互換の JSON キャッシュ (0600) とキャッシュキー
  cli/
    mod.rs           グローバル引数, モデル駆動の引数表, ページング引数の統合
    params.rs        CLI 文字列 → 値 (file://, fileb://, JSON, スカラー, blob の base64)
    shorthand.rs     shorthand 構文パーサ + BackCompat 変換
  paginate.rs        paginators-1.json による自動ページング, --max-items / --starting-token
  output.rs          json / text / off 出力, --query (JMESPath)
```

依存関係の向き: `driver` → (`cli`, `credentials`, `endpoint`, `protocol`, `paginate`, `output`) → (`model`, `value`, `util`)。
I/O は `driver::Context` に集約しています (`Transport`、環境変数、共有設定、時計、UUID)。テストではこれらを差し替えます。

## 4. フェーズ計画と進捗

| フェーズ | 内容 | 状態 |
|---|---|---|
| **P1** | 上記モジュール全体の MVP | **完了** |
| P2 | `--generate-cli-skeleton`, table/yaml 出力, waiters, モデル入力バリデーション, リトライ (standard), ヘルプ文書の整形 | 未着手 (shorthand と `--cli-input-json` は P1 に前倒しで実装済み) |
| **P3** | assume-role, web-identity, SSO, `aws login` セッション, IMDS/コンテナ認証情報, ページャ, `login` (PKCE / `--remote`), `sso login` (PKCE / デバイスコード), `sso logout` | **完了** (`logout` コマンドは未実装) |
| P4 | S3 のフレキシブルチェックサム (CRC32 等), `aws s3` 高レベルコマンド, smithy-rpc-v2-cbor, event stream, SigV4a | 未着手 |
| P5 | モデル埋め込み (zstd 圧縮 + 事前変換), `handlers_registry` 相当の改名・非表示表, 補完 | 未着手 |

### テスト結果 (P1 完了時点)

| テスト | 内容 | 結果 |
|---|---|---|
| `tests/endpoint_rules.rs` | 全サービスの `endpoint-tests-1.json` を実モデルのルールセットで評価 | 14,049 ケース 全件成功 |
| `tests/sigv4_suite.rs` | AWS SigV4 公式テストスイート (botocore と同じ 4 件を除外) | 24 ケース 全件成功 |
| `tests/protocol_input.rs` | botocore プロトコル準拠テスト (リクエスト) | 318 ケース 全件成功 |
| `tests/protocol_output.rs` | botocore プロトコル準拠テスト (レスポンス・エラー) | 307 ケース 全件成功 |
| `tests/shorthand_cases.rs` | awscli の shorthand テスト表 (AST で抽出) | 103 ケース 全件成功 |
| `tests/all_models.rs` | 全モデルの全オペレーションで引数表構築 + ダミー値でシリアライズ | 433 サービス / 19,412 オペレーション 成功 |
| `tests/cli_e2e.rs` | 偽 Transport + 実モデルで argv → 署名済みリクエスト → 出力 | 18 テスト 成功 |
| `tests/credentials_p3.rs` | 偽 Transport + 偽コールバック + 一時 HOME で各プロバイダ, キャッシュ (0600, 本家 CLI 形式のキャッシュを再利用), login (PKCE / remote / state 検証), sso login (PKCE / デバイスコード), sso logout | 19 テスト 成功 |
| `oauth::tests::local_callback_server_receives_redirect` | 実際に 127.0.0.1 で待ち受けるコールバックサーバ (ポートを使うため `--ignored`) | サンドボックス外で成功 |

実通信の確認 (P3b): 偽の refresh token で実際の Sign-In `/v1/token` に DPoP 付きで送り、`ValidationException` (終了コード 255) が解析されることを確認済み。
| 単体テスト (`src/**`) | xform_name, 設定, 認証情報, タイムスタンプ, text 出力, ページング, DPoP 署名検証, コンテナのホスト制限 など | 24 テスト 成功 |

実通信の確認: 偽の認証情報で `awsr sts get-caller-identity` を実行し、AWS から `InvalidClientTokenId` (終了コード 254) が返ることを確認済み (TLS・送受信・エラー解析の経路)。

プロトコル準拠テストは本家の比較方法 (JSON は構造比較、XML は正規化比較、query はバイト列比較) と ignore list に合わせています。
実モデルが必要なテストは、データディレクトリが無い場合はメッセージを出してスキップします。

## 5. 既知の非互換・未対応 (P1 時点)

- カスタマイズコマンド (`aws s3`, `configure`, `sso login`, `ec2 run-instances --count` など) は未実装。
- `handlers_registry.py` / `argrename.py` による改名・非表示は未反映。本家で隠されている引数も表示され、メンバー名がグローバル引数と同じ場合 (例: `--region`) はグローバル側が優先される。
- smithy-rpc-v2-cbor のみのサービス 3 件 (cloudwatchomni, eventbridgev2, partnercentral-revenue-measurement) は未対応。
- SigV4a, S3 Express, event stream, リトライ, table/yaml 出力は未対応。
- `logout` (aws login セッションの削除) は未実装。
- `login` の対話はプレーンな標準入力で行う (本家のリージョン補完付きプロンプトや agent toolkit の案内は無い)。
- `sso login` の vanity URL 解決 (`resolve_start_url`) は未対応で、設定の `sso_start_url` をそのまま使う。
- 認証情報が何も無い環境では、本家と同様に IMDS へ問い合わせるため約 1 秒かかる。
- 必須チェックサムは Content-MD5 のみ。本家 v2 既定の CRC32 フレキシブルチェックサムは未対応。
- `--no-verify-ssl`, `--ca-bundle`, `--color`, `--cli-error-format` は受け付けるが効果がない。
- text 出力で `--query` を使う場合、本家はページごとに適用するが、awsr は集約後に 1 回適用する。

## 6. 開発環境メモ

- サンドボックス内では `~/.cargo` に書き込めないので、`CARGO_HOME=$PWD/.cargo-home` を使う (`make test` で設定済み)。
- 依存 crate の一部 (ureq-proto) が `.vscode/settings.json` を含むため、初回の `cargo fetch` はサンドボックス外で実行した。
