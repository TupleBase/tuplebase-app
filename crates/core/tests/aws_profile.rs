//! Which AWS credentials a connection signs in with. Its own test binary, because it
//! sets environment variables for the whole process. Sends no requests.

use aws_sdk_ssm::config::ProvideCredentials;
use tuplebase_core::cloud::aws;

#[tokio::test]
async fn a_chosen_profile_wins_over_keys_in_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::write(
        &config,
        "[profile chosen]\nregion = eu-west-1\naws_access_key_id = AKIDFROMPROFILE\n\
         aws_secret_access_key = profile-secret\n",
    )
    .unwrap();
    // SAFETY: the only test in this binary, before anything reads the environment.
    unsafe {
        std::env::set_var("AWS_CONFIG_FILE", &config);
        std::env::set_var(
            "AWS_SHARED_CREDENTIALS_FILE",
            dir.path().join("credentials"),
        );
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDFROMENV");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "env-secret");
        std::env::remove_var("AWS_SESSION_TOKEN");
        std::env::remove_var("AWS_PROFILE");
    }
    let key = |config: aws_config::SdkConfig| async move {
        let provider = config.credentials_provider().unwrap();
        let credentials = provider.provide_credentials().await.unwrap();
        credentials.access_key_id().to_owned()
    };
    let chosen = aws::sdk_config("chosen", "").await.unwrap();
    assert_eq!(key(chosen).await, "AKIDFROMPROFILE");
    // Without a profile, the usual order applies and the environment comes first.
    let default = aws::sdk_config("", "eu-west-1").await.unwrap();
    assert_eq!(key(default).await, "AKIDFROMENV");
    // A profile that isn't there is reported, not covered by the environment's keys.
    let missing = aws::sdk_config("typo", "eu-west-1").await.unwrap_err();
    assert!(format!("{missing:#}").contains("\"typo\""), "{missing:#}");
}
