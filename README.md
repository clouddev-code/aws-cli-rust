# awsr

AWS CLI v2 (Python) のデータ駆動コアを Rust に移植した実験的な実装です。
botocore のサービスモデル (`service-2.json` ほか) をそのまま読み込み、436 サービスのうち 433 サービスのコマンドを扱えます。
方針、進捗、既知の非互換は [PLAN.md](PLAN.md) にまとめています。

## 必要なもの

- Rust 1.92 以降 (edition 2024)
- ビルド時に botocore のデータ (`reference/aws-cli/awscli/botocore/data`、または `AWSR_EMBED_DATA_DIR`) と awscli の examples。ビルドスクリプトが事前変換と zstd 圧縮をしてバイナリに埋め込むので、できたバイナリ (約 21 MB) は単体で動きます。

実行時は以下の順にモデルを探します。

1. 環境変数 `AWSR_DATA_DIR` (botocore の `data` ディレクトリ)
2. 埋め込みモデル
3. 実行ファイルと同じ場所の `data/`
4. `reference/aws-cli/awscli/botocore/data` (開発用チェックアウト)

`cargo build --no-default-features` で埋め込みなしのバイナリになります (3 と 4 を使う)。

開発用チェックアウトの取得 (fish):

```fish
git clone --depth 1 https://github.com/aws/aws-cli reference/aws-cli
```

## ビルドとテスト

```fish
make build      # cargo build
make test       # cargo test --release (CARGO_HOME をプロジェクト内に設定)
make test-debug # cargo test (依存 crate は opt-level 2 でビルド)
make release    # target/release/awsr
make clippy
```

`make` を使わない場合 (fish):

```fish
set -x CARGO_HOME (pwd)/.cargo-home
cargo test --release
```

テストはネットワークに接続しません。HTTP は `Transport` トレイトの偽実装に差し替えています。
実モデルを使うテストは、データディレクトリが見つからない場合はスキップします。

## 使い方

コマンド体系は `aws` と同じです。

```fish
set -x AWS_PROFILE dev
./target/release/awsr sts get-caller-identity
./target/release/awsr ec2 describe-instances --filters Name=instance-state-name,Values=running --query 'Reservations[].Instances[].InstanceId' --output text
./target/release/awsr dynamodb list-tables --max-items 10
./target/release/awsr s3api list-objects-v2 --bucket my-bucket --prefix logs/
./target/release/awsr lambda list-functions --endpoint-url http://localhost:4566 --region us-east-1
./target/release/awsr login --profile console   # コンソールの認証情報でログイン (ブラウザが開く)
./target/release/awsr sso login --profile dev   # IAM Identity Center にログイン
./target/release/awsr s3 ls s3://my-bucket/logs/ --human-readable --summarize
./target/release/awsr s3 cp ./dist s3://my-bucket/site/ --recursive --exclude "*.map"
./target/release/awsr s3 cp s3://my-bucket/big.iso .
./target/release/awsr s3 presign s3://my-bucket/report.pdf --expires-in 600
./target/release/awsr ec2 describe-instances --output table
./target/release/awsr sts get-caller-identity --output yaml
./target/release/awsr ec2 wait instance-running --instance-ids i-1234567890abcdef0
./target/release/awsr dynamodb create-table --generate-cli-skeleton yaml-input > table.yaml
./target/release/awsr dynamodb create-table --cli-input-yaml file://table.yaml
./target/release/awsr help                      # aws help 相当 (ページャで表示)
./target/release/awsr sts help                  # サービスのコマンド一覧
./target/release/awsr sts assume-role help      # オプション, 例, 出力の説明
```

シェル補完 (本家の `aws_completer` と同じ方式)。`awsr_completer` という名前で `awsr` へのリンクを作るか、`awsr __complete` を使います (fish):

```fish
ln -s (pwd)/target/release/awsr ~/.local/bin/awsr_completer
# fish
complete -c awsr -f -a '(begin; set -lx COMP_LINE (commandline -cp); awsr_completer; end)'
# bash: complete -C awsr_completer awsr
# zsh:  autoload bashcompinit && bashcompinit && complete -C awsr_completer awsr
```

対応している主な機能:

- プロトコル: query, ec2, json, rest-json, rest-xml, smithy-rpc-v2-cbor、event stream (S3 Select など)
- SigV4 署名 (S3 フレーバーを含む)、SigV4a (S3 マルチリージョンアクセスポイント、EventBridge グローバルエンドポイントなど)、エンドポイントルールエンジン
- フレキシブルチェックサム: 既定で CRC64NVME (PutObject / UploadPart は aws-chunked トレーラ)、`--checksum-algorithm`、GetObject のレスポンス検証。`request_checksum_calculation` / `response_checksum_validation` (`AWS_REQUEST_CHECKSUM_CALCULATION` / `AWS_RESPONSE_CHECKSUM_VALIDATION`) と `sigv4a_signing_region_set` に対応
- `outfile` 位置引数 (`s3api get-object ... out.bin`、`s3api select-object-content ... out.csv`)
- 認証情報 (本家と同じ順序): 環境変数、assume-role (`source_profile` / `credential_source` / MFA / ロールチェーン)、web identity、IAM Identity Center (SSO)、`aws login` セッション、`~/.aws/credentials`、`credential_process`、`~/.aws/config`、コンテナ、EC2 インスタンスメタデータ (IMDSv2)
- 一時認証情報は本家と同じ `~/.aws/cli/cache` / `~/.aws/sso/cache` / `~/.aws/login/cache` を読み書きするので、本家 `aws` とキャッシュを共有できます
- `login` (ブラウザ + PKCE、`--remote` でコード貼り付け) — 本家 `aws login` と同じキャッシュとプロファイル設定 (`login_session`) を作成
- `sso login` (sso-session 構成では PKCE、`--use-device-code` でデバイスコード方式) / `sso logout`
- ページャ (`AWS_PAGER` → `cli_pager` → `PAGER` → `less`、端末出力のときだけ、`--no-cli-pager` で無効)
- 引数: スカラー、リスト、JSON、shorthand 構文、`file://` / `fileb://`、`--cli-input-json` / `--cli-input-yaml`、blob の base64 (`--cli-binary-format`)
- 本家の引数・コマンドの改名と非表示 (`ec2 create-image --reboot`、`sns subscribe --notification-endpoint`、削除されたコマンドなど)
- 送信前のモデル検証 (本家と同じメッセージ、終了コード 252)
- リトライ: standard モード (最大 3 回、指数バックオフ + ジッタ、リトライクォータ)。`retry_mode` / `max_attempts` (`AWS_RETRY_MODE` / `AWS_MAX_ATTEMPTS`)
- 自動ページング、`--max-items` / `--page-size` / `--starting-token`、`--no-paginate`
- `wait` コマンド (waiters-2.json の全 waiter)
- `aws s3`: ls / mb / rb / rm / presign / cp (`--recursive`、`--include` / `--exclude`、マルチパート・並列転送。`[profile] s3 =` の `max_concurrent_requests` / `multipart_threshold` / `multipart_chunksize`)
- `--generate-cli-skeleton` (`input` / `yaml-input` / `output`)
- 出力: json / text / table / yaml / yaml-stream / off、`--query` (JMESPath)、`--color` (table の色付け)
- help: 本家の man ページと同じ構成 (説明、構文、オプション、グローバルオプション、例、出力)
- 終了コード: 0 / 252 (引数エラー) / 253 (設定エラー) / 254 (サービスエラー) / 255 (その他)

## ライセンス

Apache-2.0。`tests/fixtures/` 以下は aws-cli / botocore のテストフィクスチャ (Apache-2.0) の複製と、本家 CLI の出力から生成したフィクスチャです。`assets/cli.json` は aws-cli の `awscli/data/cli.json` の複製です。`tests/fixtures/aws-signing-test-suite-v4a/` は aws-c-auth (Apache-2.0、同ディレクトリに LICENSE を同梱) のテストスイートの複製です。
