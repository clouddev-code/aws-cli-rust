# awsr

AWS CLI v2 (Python) のデータ駆動コアを Rust に移植した実験的な実装です。
botocore のサービスモデル (`service-2.json` ほか) をそのまま読み込み、436 サービスのうち 433 サービスのコマンドを扱えます。
方針、進捗、既知の非互換は [PLAN.md](PLAN.md) にまとめています。

## 必要なもの

- Rust 1.92 以降 (edition 2024)
- botocore のデータディレクトリ。以下の順に探します。
  1. 環境変数 `AWSR_DATA_DIR`
  2. 実行ファイルと同じ場所の `data/`
  3. `reference/aws-cli/awscli/botocore/data` (開発用チェックアウト)

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
./target/release/awsr help                      # サービス一覧
./target/release/awsr sts help                  # オペレーション一覧
./target/release/awsr sts assume-role help      # 引数一覧
```

対応している主な機能:

- プロトコル: query, ec2, json, rest-json, rest-xml
- SigV4 署名 (S3 フレーバーを含む)、エンドポイントルールエンジン
- 認証情報 (本家と同じ順序): 環境変数、assume-role (`source_profile` / `credential_source` / MFA / ロールチェーン)、web identity、IAM Identity Center (SSO)、`aws login` セッション、`~/.aws/credentials`、`credential_process`、`~/.aws/config`、コンテナ、EC2 インスタンスメタデータ (IMDSv2)
- 一時認証情報は本家と同じ `~/.aws/cli/cache` / `~/.aws/sso/cache` / `~/.aws/login/cache` を読み書きするので、本家 `aws` とキャッシュを共有できます
- `login` (ブラウザ + PKCE、`--remote` でコード貼り付け) — 本家 `aws login` と同じキャッシュとプロファイル設定 (`login_session`) を作成
- `sso login` (sso-session 構成では PKCE、`--use-device-code` でデバイスコード方式) / `sso logout`
- ページャ (`AWS_PAGER` → `cli_pager` → `PAGER` → `less`、端末出力のときだけ、`--no-cli-pager` で無効)
- 引数: スカラー、リスト、JSON、shorthand 構文、`file://` / `fileb://`、`--cli-input-json`、blob の base64 (`--cli-binary-format`)
- 自動ページング、`--max-items` / `--page-size` / `--starting-token`、`--no-paginate`
- 出力: json / text / off、`--query` (JMESPath)
- 終了コード: 0 / 252 (引数エラー) / 253 (設定エラー) / 254 (サービスエラー) / 255 (その他)

## ライセンス

Apache-2.0。`tests/fixtures/` 以下は aws-cli / botocore のテストフィクスチャ (Apache-2.0) の複製です。
