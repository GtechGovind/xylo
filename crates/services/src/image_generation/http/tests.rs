use super::*;
#[test]
fn download_address_policy_excludes_local_and_metadata_targets() {
    for value in [
        "127.0.0.1",
        "10.2.3.4",
        "192.168.2.3",
        "169.254.169.254",
        "100.64.1.2",
        "::1",
        "fc00::1",
        "fe80::2",
        "::ffff:127.0.0.1",
    ] {
        assert!(!public_ip(value.parse().unwrap()), "{value}");
    }
    assert!(public_ip("8.8.8.8".parse().unwrap()));
    assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
}
