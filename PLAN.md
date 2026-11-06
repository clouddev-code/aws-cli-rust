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
2. **モデルはバイナリに埋め込む (P5)。** ビルド時に最新 API バージョンだけを事前変換 (sdk-extras のマージ + minify) し、ファイル単位で zstd 圧縮する。`AWSR_DATA_DIR` を指定すればデータディレクトリを優先する。
3. **公式テスト資産で互換性を確認する。** botocore の Apache-2.0 テストフィクスチャを `tests/fixtures/` にコピーし、データ駆動テストにする。
4. **I/O を分離する。** HTTP は `Transport` トレイトに隠す。テストでは偽の Transport で送信リクエストを検証するので、ポートのバインドは不要。
5. **単一クレート (lib + bin)。** 規模が大きくなったらワークスペースに分割する。

## 3. アーキテクチャ (実装済み)

```
src/
  bin/awsr.rs        エントリポイント (実 Transport・環境変数・時計・端末幅を注入)。awsr_completer として起動されると補完
  driver.rs          コマンド解決 → パラメータ構築 → リージョン/認証/エンドポイント → 送信 → 出力
  customize.rs       handlers_registry 相当: 引数の改名・非表示エイリアス, コマンドの削除・改名, 非表示引数
  validate.rs        botocore ParamValidator (必須, 未知, 型, min, hostLabel, tagged union, document)
  waiter.rs          waiters-2.json による wait コマンド (botocore Waiter)
  skeleton.rs        --generate-cli-skeleton (input / yaml-input / output)
  help.rs            help ページ (man 出力相当のテキスト整形, HTML / reST の変換)
  doc.rs             モデル文書 (HTML) のテキスト化と最初の一文
  completion.rs      シェル補完 (aws_completer 相当, ローカルのモデルのみ)
  table.rs           --output table (awscli table.py)
  yaml.rs            --output yaml / yaml-stream (libyaml エミッタ相当)
  eaw.rs             East Asian Width 表 (table の桁計算)
  naming.rs          xform_name (+ 本家の上書き表 44 件)
  model.rs           ServiceModel / Operation / Shape (生 JSON 上のゼロコピービュー)
  loader.rs          埋め込みモデル / データディレクトリの読み込み, sdk-extras のマージ, コマンド名の改名, S3/S3Control のモデル書き換え
  value.rs           パラメータ値 (Bytes / Timestamp を持つ独自型)
  timestamp.rs       iso8601 / rfc822 / unixTimestamp, Python isoformat 互換出力
  endpoint/mod.rs    エンドポイントルールエンジン (標準関数 12 種)
  sigv4.rs           AWS Signature V4 (通常 / S3 フレーバー) と SigV4a (ECDSA P-256)
  checksum.rs        フレキシブルチェックサム (CRC32/CRC32C/CRC64NVME/SHA1/SHA256/SHA512/XXHASH64), aws-chunked
  http.rs            Request / Response, Transport トレイト, ureq 実装
  protocol/
    ser.rs           query / ec2 / json / rest-json / rest-xml / smithy-rpc-v2-cbor シリアライザ
    parse.rs         同 6 プロトコルのレスポンス・エラーパーサ, event stream の解析
    cbor.rs          CBOR エンコーダ / デコーダ (デコード結果は JSON 値に寄せて JSON のシェイプ変換を再利用)
    eventstream.rs   application/vnd.amazon.eventstream のフレーム (CRC 検証付き)
    xml.rs           quick-xml ベースの XML ツリー
  config.rs          ~/.aws/config, ~/.aws/credentials (INI + ネスト)
  context.rs         外部依存 (Transport, env, 時計, 入力, スリープ, ブラウザ, TTY) の注入口
  client.rs          モデル駆動の API クライアント (driver と認証情報プロバイダが共用)。検証, standard リトライ
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
  output.rs          json / text / table / yaml / yaml-stream / off 出力, --query (JMESPath)
build.rs             モデルと examples の事前変換 + zstd 埋め込み (feature embed-data)
```

依存関係の向き: `driver` → (`cli`, `credentials`, `endpoint`, `protocol`, `paginate`, `output`) → (`model`, `value`, `util`)。
I/O は `driver::Context` に集約しています (`Transport`、環境変数、共有設定、時計、UUID)。テストではこれらを差し替えます。

## 4. フェーズ計画と進捗

| フェーズ | 内容 | 状態 |
|---|---|---|
| **P1** | 上記モジュール全体の MVP | **完了** |
| **P2** | `--generate-cli-skeleton`, `--cli-input-yaml`, table/yaml/yaml-stream 出力 (`--color`), waiters, モデル入力バリデーション, リトライ (standard), ヘルプ文書の整形 | **完了** |
| **P3** | assume-role, web-identity, SSO, `aws login` セッション, IMDS/コンテナ認証情報, ページャ, `login` (PKCE / `--remote`), `sso login` (PKCE / デバイスコード), `sso logout` | **完了** (`logout` コマンドは未実装) |
| P4 | S3 のフレキシブルチェックサム (CRC32 等), `aws s3` 高レベルコマンド, smithy-rpc-v2-cbor, event stream, SigV4a | **下回り 4 項目は完了** (チェックサム / CBOR / event stream / SigV4a、`outfile` 引数を含む)。`aws s3` 高レベルコマンドは次段階 |
| **P5** | モデル埋め込み (zstd 圧縮 + 事前変換), `handlers_registry` 相当の改名・非表示表, 補完 | **完了** |

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
| `tests/protocol_input.rs` / `protocol_output.rs` (P4 追加分) | smithy-rpc-v2-cbor (通常 / query 互換 / 非 query 互換)。CBOR はエンコード方法が複数あり得るため、デコード結果で比較 | 入力 24 / 出力 41 ケース 全件成功 |
| `tests/sigv4a_suite.rs` | aws-c-auth の SigV4a テストスイート (botocore が awscrt 経由で使う実装)。導出公開鍵・正規リクエスト・署名対象文字列の一致と ECDSA 署名の検証 | 28 ケース 全件成功 (botocore で到達しない組み合わせ 10 件と期待値の無い 2 件を除外)。S3 フレーバー (正規化なし + X-Amz-Content-SHA256) のケースはスイートに無く、MRAP の SigV4a は S3 SigV4 と共通の経路で担保している |
| `tests/cli_e2e.rs` (P4 追加分) | 必須チェックサム (CRC64NVME), PutObject の aws-chunked トレーラ, `--checksum-algorithm`, `when_required`, 設定値の検証, GetObject の `outfile` とレスポンス検証, EventBridge グローバルエンドポイントの SigV4a, S3 Select / Lambda / Kinesis の event stream | 計 23 テスト 成功 |
| `tests/output_formats.rs` (P2) | 本家の TableFormatter / YAMLFormatter / StreamedYAMLFormatter で生成したフィクスチャ (`tests/fixtures/output/`) との一致。YAML 1.2 の引用規則, 80 桁折り返し, 二重引用のエスケープ, 128 バイト超のキー, 表の縦変換, 全角幅 | 59 ケース 全件一致 |
| `tests/cli_e2e.rs` (P2/P5) | 改名・非表示エイリアス・削除コマンド, 非表示のページングトークン, バリデーション (本家と同文), standard リトライ (503 / スロットリング / 接続エラー / max_attempts / legacy), table / yaml 出力, `--generate-cli-skeleton` (本家 CLI で生成した 33 ケースと一致), `--cli-input-yaml`, waiters (成功 / 失敗状態 / 未対応エラー / 上限回数, メッセージは botocore で確認), help ページ (本家の man 出力と比較) | 計 32 テスト 成功 |
| `tests/all_models.rs` (P5) | 埋め込みアーカイブと元のデータディレクトリが全サービス・全ファイルで同じ文書になること | 成功 |
| `tests/completion.rs` (P5) | コマンド, オペレーション, waiter, 引数名 (非表示・使用済みを除く), 値 (選択肢, プロファイル, リージョン, enum) | 2 テスト 成功 |
| チェックサム単体テスト | botocore `_CHECKSUM_DIGEST_CASES` と同じ期待値 (XXHASH3/128 以外), aws-chunked の形式 | 成功 |
| event stream 単体テスト | botocore `test_eventstream.py` の正常系・異常系ベクタ (CRC 不一致, 重複ヘッダ, ヘッダ長超過) | 成功 |
| `oauth::tests::local_callback_server_receives_redirect` | 実際に 127.0.0.1 で待ち受けるコールバックサーバ (ポートを使うため `--ignored`) | サンドボックス外で成功 |

実通信の確認 (P3b): 偽の refresh token で実際の Sign-In `/v1/token` に DPoP 付きで送り、`ValidationException` (終了コード 255) が解析されることを確認済み。
| 単体テスト (`src/**`) | xform_name, 設定, 認証情報, タイムスタンプ, text 出力, ページング, DPoP 署名検証, コンテナのホスト制限 など | 24 テスト 成功 |

実通信の確認: 偽の認証情報で `awsr sts get-caller-identity` を実行し、AWS から `InvalidClientTokenId` (終了コード 254) が返ることを確認済み (TLS・送受信・エラー解析の経路)。

プロトコル準拠テストは本家の比較方法 (JSON は構造比較、XML は正規化比較、query はバイト列比較) と ignore list に合わせています。
実モデルが必要なテストは、データディレクトリが無い場合はメッセージを出してスキップします。

## 5. 既知の非互換・未対応

- カスタマイズコマンド (`aws s3`, `configure`, `ec2 run-instances --count` など) は未実装 (`login` / `sso login` / `sso logout` は実装済み)。
- 改名・非表示は `argrename.py`, `removals.py`, `signin.py`, `cloudwatch.py`, `kinesis.py`, ページング (入力トークンの非表示) を反映済み。独自コマンドを作るカスタマイズ (`rds add-option-to-option-group` / `remove-option-from-option-group`, `ses send-email`, `ec2 authorize-security-group-ingress` の簡易引数など) は未実装で、元のオペレーションのまま使う。メンバー名がグローバル引数と同じ場合 (改名表に無いもの) はグローバル側が優先される。
- S3 Express (CreateSession によるセッション認証) は未対応。
- リトライ: `standard` は本家 (AWS_NEW_RETRIES_2026 未設定時) と同じ。`adaptive` はクライアント側のレート制限が無く `standard` と同じ動作。`legacy` は本家 v2 と同じくリトライしない。`amz-sdk-request` の `ttl` は送らない。
- `yaml-stream` でページングした場合、本家はページごとに出力するが、awsr は集約した結果を 1 要素として出力する。
- `--generate-cli-skeleton output` は本家と違い、生成した応答をモデルで検証しない (本家は検証に失敗して出力できない操作がある)。
- waiter で `--starting-token` / `--page-size` / `--max-items` を指定しても無視する。
- help: 太字・下線は再現しない。本家 (docutils) の癖のうち、リンク直後の ` .` や docutils の警告文は再現しない。HTML 内の見出し (`<h3>` 等) の位置が少し違う。`aws help topics` は未実装。
- 補完はローカルのモデルだけを使う (本家 v2 のサーバー側補完 = リソース名の候補は無い)。
- `auth_scheme_preference` (`AWS_AUTH_SCHEME_PREFERENCE`) は未対応で、エンドポイント / モデルが示す先頭の対応方式を使う。
- `logout` (aws login セッションの削除) は未実装。
- `login` の対話はプレーンな標準入力で行う (本家のリージョン補完付きプロンプトや agent toolkit の案内は無い)。
- `sso login` の vanity URL 解決 (`resolve_start_url`) は未対応で、設定の `sso_start_url` をそのまま使う。
- 認証情報が何も無い環境では、本家と同様に IMDS へ問い合わせるため約 1 秒かかる。
- チェックサムの XXHASH3 / XXHASH128 は未実装 (本家は awscrt)。指定すると `Unsupported checksum algorithm` になり、レスポンス検証では次の優先度のアルゴリズムを使う。
- aws-chunked トレーラ付きの送信は、本文をメモリに持つため `Transfer-Encoding: chunked` ではなくエンコード後の `Content-Length` で送る (S3 はどちらも受け付ける)。
- HTTP 本文はメモリに読み切る。`s3api get-object` の `outfile` や event stream は応答全体を受け取ってから処理するため、`logs start-live-tail` のような終わりの無いストリームは扱えない (`start-live-tail` 自体も未実装)。
- 本家 CLI が削除している event stream 操作 (`lambda invoke-with-response-stream`, `bedrock-runtime converse-stream` など) も表示され、イベントの一覧を JSON で出力する。
- `--no-verify-ssl`, `--ca-bundle`, `--cli-error-format` は受け付けるが効果がない。
- text 出力で `--query` を使う場合、本家はページごとに適用するが、awsr は集約後に 1 回適用する。

## 6. 開発環境メモ

- サンドボックス内では `~/.cargo` に書き込めないので、`CARGO_HOME=$PWD/.cargo-home` を使う (`make test` で設定済み)。
- 依存 crate の一部 (ureq-proto) が `.vscode/settings.json` を含むため、初回の `cargo fetch` はサンドボックス外で実行した。
- モデルは build.rs がビルド時に埋め込む (各サービスの最新 API バージョンだけ, sdk-extras をマージして minify, ファイル単位の zstd フレーム)。110 MB の JSON が約 14 MB になり、バイナリは約 21 MB。圧縮はビルド 1 回あたり (プロファイルごと) 約 40 秒。`AWSR_EMBED_LEVEL` で zstd レベル (既定 19)、`AWSR_EMBED_DATA_DIR` で元データを指定できる。`--no-default-features` で埋め込みなし。
- 実行時の探索順: `AWSR_DATA_DIR` → 埋め込み → 実行ファイル横の `data/` → `reference/aws-cli/awscli/botocore/data`。
- 本家との突き合わせ用フィクスチャ (`tests/fixtures/{output,skeleton,help}`) は、本家 CLI を入れた Python 環境で再生成できる (fish):

  ```fish
  uv venv -p 3.12 /tmp/awscli-venv
  set -x VIRTUAL_ENV /tmp/awscli-venv; uv pip install -e reference/aws-cli
  /tmp/awscli-venv/bin/python tests/fixtures/output/generate.py > tests/fixtures/output/cases.json
  set -x AWS_DEFAULT_REGION us-east-1; /tmp/awscli-venv/bin/python tests/fixtures/skeleton/generate.py > tests/fixtures/skeleton/cases.json
  ```
- `assets/cli.json` は aws-cli の `awscli/data/cli.json` (グローバル引数の説明) の複製。
