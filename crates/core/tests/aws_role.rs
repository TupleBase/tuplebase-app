//! A profile that assumes a role, against the fake `aws` service in `docker-compose.yml`.
//! Its own test binary, because it sets environment variables for the whole process.
//! Skipped unless `TUPLEBASE_IT=1` (see docs/DEVELOPMENT.md).

use aws_sdk_ssm::config::ProvideCredentials;
use tuplebase_core::cloud::aws;

const FAKE_AWS: &str = "http://127.0.0.1:54566";

#[tokio::test]
async fn a_profile_that_assumes_a_role_signs_in_through_sts() {
    if std::env::var("TUPLEBASE_IT").as_deref() != Ok("1") {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    // The region is on the role's profile only, as is usual; STS needs it.
    std::fs::write(
        &config,
        "[profile base]\naws_access_key_id = test\naws_secret_access_key = test\n\n\
         [profile admin]\nrole_arn = arn:aws:iam::123456789012:role/admin\n\
         source_profile = base\nregion = eu-west-1\n",
    )
    .unwrap();
    // SAFETY: the only test in this binary, before anything reads the environment.
    unsafe {
        std::env::set_var("AWS_CONFIG_FILE", &config);
        std::env::set_var(
            "AWS_SHARED_CREDENTIALS_FILE",
            dir.path().join("credentials"),
        );
        std::env::set_var("AWS_ENDPOINT_URL", FAKE_AWS);
        for name in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_PROFILE",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
        ] {
            std::env::remove_var(name);
        }
    }
    for region in ["", "us-east-1"] {
        let config = aws::sdk_config("admin", region).await.unwrap_or_else(|e| {
            panic!("signing in with a role, region {region:?}: {e:#}");
        });
        let provider = config.credentials_provider().unwrap();
        let credentials = provider.provide_credentials().await.unwrap();
        // The role's own keys, from STS, not the source profile's.
        assert_ne!(credentials.access_key_id(), "test");
        assert!(credentials.session_token().is_some());
    }
}
