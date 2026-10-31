use super::*;
#[test]
fn official_iam_list_users_signature_matches_aws_reference_vector() {
    // AWS IAM SigV4 example (2015-08-30), public synthetic credentials.
    let headers = sign_get(
        &Url::parse("https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08").unwrap(),
        "us-east-1",
        "iam",
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        None,
        "20150830T123600Z",
        BTreeMap::from([(
            "content-type".into(),
            "application/x-www-form-urlencoded; charset=utf-8".into(),
        )]),
    )
    .unwrap();
    assert!(
        headers["authorization"].to_str().unwrap().ends_with(
            "Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        )
    );
}
