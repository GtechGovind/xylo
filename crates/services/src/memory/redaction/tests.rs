use super::*;

#[test]
fn provider_tokens_and_jwts_are_redacted() {
    let text = "Use sk-proj-abcdefghijklmnop1234 or ghp_ABCDEFGHIJKLMNOPQRSTUVWX12 and AKIAABCDEFGHIJKLMNOP; jwt eyJhbGciOi.eyJzdWIiOiIx.c2lnbmF0dXJl.";
    let redacted = redact_secrets(text);
    for secret in ["sk-proj", "ghp_", "AKIAABCD", "eyJhbGciOi"] {
        assert!(!redacted.contains(secret), "{secret} survived: {redacted}");
    }
    assert!(redacted.starts_with("Use [REDACTED] or [REDACTED] and [REDACTED];"));
}

#[test]
fn credential_assignments_keep_the_key_and_drop_the_value() {
    assert_eq!(
        redact_secrets("DB_PASSWORD=hunter22 PORT=5432"),
        "DB_PASSWORD=[REDACTED] PORT=5432"
    );
    assert_eq!(
        redact_secrets(r#"{"api_key":"abc123-def"}"#),
        r#"{"api_key":"[REDACTED]"}"#
    );
    assert_eq!(
        redact_secrets("api_key: \"zz9-plural-z-alpha\", token = s3cr3tvalue"),
        "api_key: \"[REDACTED]\", token = [REDACTED]"
    );
    assert_eq!(
        redact_secrets("Authorization: Bearer abc.def-123456"),
        "Authorization: Bearer [REDACTED]"
    );
}

#[test]
fn urls_keep_everything_but_the_password() {
    assert_eq!(
        redact_secrets("Connect with postgres://app:pa55word@db.local:5432/main"),
        "Connect with postgres://app:[REDACTED]@db.local:5432/main"
    );
    assert_eq!(
        redact_secrets("See https://example.com/a:b@c"),
        "See https://example.com/a:b@c"
    );
}

#[test]
fn private_key_blocks_are_removed_whole() {
    let text = "key:\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAAB3NzaC1\nabc\n-----END OPENSSH PRIVATE KEY-----\nafter";
    assert_eq!(redact_secrets(text), "key:\n[REDACTED]\nafter");
    let certificate = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
    assert_eq!(redact_secrets(certificate), certificate);
}

#[test]
fn ordinary_prose_and_code_survive() {
    for text in [
        "Tests run with cargo nextest; use sk-learn for models.",
        "The token: expires hourly, run pwd first.",
        "Bearer tokens are rotated by the gateway.",
        "max_tokens=4096 and the tokenizer=bpe setting",
        "Fact 1: the API lives in src/api.rs:42",
    ] {
        assert_eq!(redact_secrets(text), text);
    }
}

#[test]
fn redaction_is_idempotent() {
    let text = "password=hunter22 Bearer abcdef123456 sk-abcdefghijklmnopqrstu https://u:p4ss@h/x";
    let once = redact_secrets(text);
    assert_eq!(redact_secrets(&once), once);
    assert_eq!(once.matches(REDACTED).count(), 4, "{once}");
}

#[test]
fn query_string_secrets_are_redacted() {
    assert_eq!(
        redact_secrets("GET https://api.example.com/v1/items?api_key=abcd1234&page=2"),
        "GET https://api.example.com/v1/items?api_key=[REDACTED]&page=2"
    );
    assert_eq!(
        redact_secrets("jdbc:mysql://db.local/app?user=app&password=s3cretpw"),
        "jdbc:mysql://db.local/app?user=app&password=[REDACTED]"
    );
}

#[test]
fn a_token_used_as_the_url_user_name_is_redacted() {
    assert_eq!(
        redact_secrets("git clone https://ghp_ABCDEFGHIJKLMNOPQRSTUVWX12@github.com/o/r.git"),
        "git clone https://[REDACTED]@github.com/o/r.git"
    );
    let plain = "ssh://git@github.com/o/r.git";
    assert_eq!(redact_secrets(plain), plain);
}

#[test]
fn space_separated_secret_flags_are_redacted() {
    assert_eq!(
        redact_secrets("deploy --password hunter22 --api-key zz9plural --verbose"),
        "deploy --password [REDACTED] --api-key [REDACTED] --verbose"
    );
    assert_eq!(
        redact_secrets("mysql -u root -p s3cret -h db"),
        "mysql -u root -p [REDACTED] -h db"
    );
    assert_eq!(
        redact_secrets("mysqldump -u root -ps3cret app"),
        "mysqldump -u root -p[REDACTED] app"
    );
    let unrelated = "grep -p pattern and cargo test --release";
    assert_eq!(redact_secrets(unrelated), unrelated);
}

#[test]
fn semicolon_connection_strings_keep_everything_but_the_password() {
    assert_eq!(
        redact_secrets("Server=db.local;Database=app;User Id=sa;Password=Sup3rS3cret9;"),
        "Server=db.local;Database=app;User Id=sa;Password=[REDACTED];"
    );
}
