use super::*;
use jsonwebtoken::{EncodingKey, Header, encode};
use serde_json::json;

fn jwks() -> JwkSet {
    // Public modulus of the generated NONPRODUCTION fixture private key.
    let modulus = "A823867816F5C1917F9D3863DD8EFD2500340B75C33534788CAC01D8DDD725C4B5A8188D6DEB9AF12E89D22E523EEE9A723B8FDEC57B3F1B2C202291616B3049CCFEC9BED3B3CB9D3BC37E75D2C6595FD714C4DB055F955B3F5C126C7EEE7B3ED025F89315D9F84D5AE9177877DE96B8E5E2FF09C3EEE1D29B55956EC5F5B685BA1324DB5EAA6A956BD0D9EDFBFF8DC171C4D138FEF97B3BF2B57948CEBF4D0A69849C37CCA30D1B56FEF7FFD51DE1A23D89E274BA2B23560F239FA3E42148FAE10B8E8CF26967AAD6B1AA6887D183F06B00EAF7EF71C61E454F48B4C06BF29B9759A5C6F7C18022BD813C9C05D5330CF892305B1324B116E56CD0D1E09425A5";
    let bytes = (0..modulus.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&modulus[index..index + 2], 16).unwrap())
        .collect::<Vec<_>>();
    serde_json::from_value(json!({"keys":[{"kty":"RSA","kid":"xylo-test-only","use":"sig","alg":"RS256","n":URL_SAFE_NO_PAD.encode(bytes),"e":"AQAB"}]})).unwrap()
}
fn signed(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("xylo-test-only".into());
    encode(
        &header,
        claims,
        &EncodingKey::from_rsa_pem(include_bytes!("fixtures/chatgpt-test-key.pem")).unwrap(),
    )
    .unwrap()
}

#[test]
fn signed_identity_requires_published_key_issuer_audience_expiry_and_nonce() {
    let claims = json!({"iss":AUTHORITY,"sub":"fixture-account","aud":"oaiapp_fixture","exp":xylo_core::now_ms()/1000+3600,"nonce":"attempt-nonce","email":"fixture@example.invalid"});
    let identity = validate_identity(
        &signed(&claims),
        "oaiapp_fixture",
        Some("attempt-nonce"),
        &jwks(),
    )
    .unwrap();
    assert_eq!(identity.sub, "fixture-account");
    assert!(
        validate_identity(
            &signed(&claims),
            "different-client",
            Some("attempt-nonce"),
            &jwks()
        )
        .is_err()
    );
    assert!(
        validate_identity(
            &signed(&claims),
            "oaiapp_fixture",
            Some("different-nonce"),
            &jwks()
        )
        .is_err()
    );
    for (key, value) in [
        ("iss", json!("https://untrusted.example")),
        ("exp", json!(1)),
    ] {
        let mut invalid = claims.clone();
        invalid[key] = value;
        assert!(
            validate_identity(
                &signed(&invalid),
                "oaiapp_fixture",
                Some("attempt-nonce"),
                &jwks()
            )
            .is_err()
        );
    }
    let unsigned = encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(b"not-an-openai-key"),
    )
    .unwrap();
    assert!(
        validate_identity(&unsigned, "oaiapp_fixture", Some("attempt-nonce"), &jwks()).is_err()
    );
}

#[test]
fn dynamic_registration_binds_state_callback_and_issued_client_id() {
    let host = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    let pending =
        PendingChatGptSignIn::begin(&host, "http://127.0.0.1:44321/auth/callback", None).unwrap();
    let url = Url::parse(&pending.authorization_url).unwrap();
    let fields = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(fields["client_id"], DYNAMIC_CLIENT);
    assert_eq!(fields["agent_name_hint"], "Xylo");
    assert_eq!(fields["ext_agent_host_id"], host);
    assert_eq!(
        fields["code_challenge"],
        URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()))
    );
    let callback = format!(
        "{}?code=fixture-code&state={}&client_id=oaiapp_issued",
        pending.redirect_uri, pending.state
    );
    assert_eq!(
        pending.callback(&callback).unwrap(),
        ("oaiapp_issued".into(), "fixture-code".into())
    );
    assert!(
        pending
            .callback(&callback.replace(&pending.state, "wrong-state"))
            .is_err()
    );
    assert!(
        pending
            .callback(&callback.replace("oaiapp_issued", DYNAMIC_CLIENT))
            .is_err()
    );
    assert!(
        pending
            .callback(&callback.replace("127.0.0.1", "localhost"))
            .is_err()
    );
    assert!(
        pending
            .callback(&format!("{callback}&state=duplicate"))
            .is_err()
    );
    assert!(
        PendingChatGptSignIn::begin(&host, "http://localhost:44321/auth/callback", None).is_err()
    );
}

#[test]
fn validated_identity_does_not_imply_plan_permission() {
    let identity = Identity {
        sub: "account".into(),
        iss: AUTHORITY.into(),
        nonce: None,
        email: None,
    };
    let tokens = json!({"id_token":"fixture-id","access_token":"fixture-access","token_type":"Bearer","scope":"openid profile email","expires_in":3600});
    let retained = credential(tokens, "oaiapp_issued".into(), "host".into(), identity).unwrap();
    assert!(!retained.plan_usage_authorized());
    assert_eq!(retained.subject, "account");
}
#[test]
fn long_token_lifetimes_are_accepted_and_capped() {
    let identity = || Identity {
        sub: "account".into(),
        iss: AUTHORITY.into(),
        nonce: None,
        email: None,
    };
    let tokens = |expires_in: i64| json!({"id_token":"fixture-id","access_token":"fixture-access","token_type":"Bearer","scope":"openid","expires_in":expires_in});
    let before = xylo_core::now_ms();
    let two_days = credential(tokens(172_800), "c".into(), "h".into(), identity()).unwrap();
    assert!(two_days.expires_at_ms >= before + 172_800_000);
    let capped = credential(tokens(i64::MAX / 2), "c".into(), "h".into(), identity()).unwrap();
    assert!(capped.expires_at_ms <= xylo_core::now_ms() + MAX_ACCESS_TOKEN_LIFETIME_SECS * 1000);
    assert!(credential(tokens(0), "c".into(), "h".into(), identity()).is_err());
}
#[test]
fn enabling_previously_declined_plan_usage_requests_consent_without_forcing_ordinary_sign_in() {
    let host = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    for authorized in [false, true] {
        let scope = if authorized {
            "openid profile email chatgpt.tokens.use.direct"
        } else {
            "openid profile email"
        };
        let previous = credential(json!({"id_token":"fixture-id","access_token":"fixture-access","token_type":"Bearer","scope":scope,"expires_in":3600}), "oaiapp_issued".into(), host.clone(), Identity { sub:"account".into(), iss:AUTHORITY.into(), nonce:None, email:None }).unwrap();
        let pending = PendingChatGptSignIn::begin(
            &host,
            "http://127.0.0.1:44321/auth/callback",
            Some(&previous),
        )
        .unwrap();
        let fields = Url::parse(&pending.authorization_url)
            .unwrap()
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(fields["client_id"], "oaiapp_issued");
        assert_eq!(
            fields.get("prompt").map(String::as_str),
            if authorized { None } else { Some("consent") }
        );
        assert!(fields["scope"].contains("chatgpt.tokens.use.direct"));
    }
}
