//! SigV4a test suite (aws-c-auth tests/aws-signing-test-suite/v4a, the
//! implementation botocore uses through awscrt): derived public key,
//! canonical request, string to sign and a verifiable ECDSA signature.

mod common;

use awsr::sigv4::{Credentials, Signer, v4a_signing_key};
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use serde_json::Value as J;

fn read(dir: &std::path::Path, file: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(file))
        .ok()
        .map(|s| s.replace('\r', ""))
}

#[test]
fn aws_signing_test_suite_v4a() {
    let root = common::fixtures().join("aws-signing-test-suite-v4a");
    let mut cases: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !p.file_name().unwrap().to_string_lossy().starts_with('.'))
        .collect();
    cases.sort();
    let mut ran = 0;
    for dir in &cases {
        let case = dir.file_name().unwrap().to_string_lossy().to_string();
        let creq = read(dir, "header-canonical-request.txt").unwrap();
        let sts = read(dir, "header-string-to-sign.txt").unwrap();
        let public = read(dir, "public-key.json").unwrap();
        let ctx: J = serde_json::from_str(&read(dir, "context.json").unwrap()).unwrap();
        // botocore couples "no path normalization" with an
        // X-Amz-Content-SHA256 header (S3, `CrtS3SigV4AsymAuth`) and does
        // neither otherwise, so the suite's cases that set only one of
        // `normalize: false` / `sign_body: true` are unreachable.
        // `omit_session_token` (sign without the token, add it afterwards)
        // is a CRT option botocore never sets.
        let s3_like = ctx["normalize"] == J::Bool(false);
        if s3_like != (ctx["sign_body"] == J::Bool(true))
            || ctx["omit_session_token"] == J::Bool(true)
        {
            continue;
        }
        let c = &ctx["credentials"];
        let creds = Credentials {
            access_key: c["access_key_id"].as_str().unwrap().into(),
            secret_key: c["secret_access_key"].as_str().unwrap().into(),
            session_token: c["token"].as_str().map(str::to_string),
        };
        let public: J = serde_json::from_str(&public).unwrap();
        let point = v4a_signing_key(&creds)
            .verifying_key()
            .to_encoded_point(false);
        assert_eq!(
            (
                hex::encode(point.x().unwrap()),
                hex::encode(point.y().unwrap())
            ),
            (
                public["X"].as_str().unwrap().to_lowercase(),
                public["Y"].as_str().unwrap().to_lowercase()
            ),
            "{case}: derived public key"
        );

        let now = chrono::DateTime::parse_from_rfc3339(ctx["timestamp"].as_str().unwrap())
            .unwrap()
            .with_timezone(&chrono::Utc);
        let signer = Signer {
            credentials: &creds,
            region: ctx["region"].as_str().unwrap(),
            service: ctx["service"].as_str().unwrap(),
            s3: s3_like,
            payload: None,
            asymmetric: true,
        };
        let mut req = common::parse_raw(&read(dir, "request.txt").unwrap());
        let sig = signer.sign(&mut req, now);
        assert_eq!(
            sig.canonical_request,
            creq.trim_end(),
            "{case}: canonical request"
        );
        assert_eq!(sig.string_to_sign, sts.trim_end(), "{case}: string to sign");

        let der = sig.authorization.rsplit("Signature=").next().unwrap();
        let signature = Signature::from_der(&hex::decode(der).unwrap()).unwrap();
        let key: VerifyingKey = *v4a_signing_key(&creds).verifying_key();
        key.verify(sig.string_to_sign.as_bytes(), &signature)
            .unwrap_or_else(|e| panic!("{case}: signature does not verify: {e}"));
        ran += 1;
    }
    eprintln!("sigv4a cases: {ran}");
    assert!(ran >= 25, "only {ran} cases ran");
}
