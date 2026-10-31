use super::{SshHost, default_known_hosts, known_host, parse_config};
use std::path::{Path, PathBuf};

#[test]
fn config_hosts_keep_first_values_and_skip_wildcards() {
    let config = r#"
# personal
Host build-box staging
    HostName build.example.com
    User deploy
    Port 2222
    IdentityFile ~/.ssh/id_build
    Port 22

Host *.internal !secret
    User nobody

Host "quoted"
  hostname=10.0.0.5
Match host other
    User ignored
"#;
    let hosts = parse_config(config, Path::new("/home/me"));
    assert_eq!(hosts.len(), 3);
    let build = SshHost {
        alias: "build-box".into(),
        host_name: Some("build.example.com".into()),
        user: Some("deploy".into()),
        port: Some(2222),
        identity_file: Some(PathBuf::from("/home/me/.ssh/id_build")),
    };
    assert_eq!(hosts[0], build);
    assert_eq!(hosts[1].alias, "staging");
    assert_eq!(hosts[1].address(), "build.example.com");
    assert_eq!(hosts[2].address(), "10.0.0.5");
    assert_eq!(hosts[2].user, None, "Match blocks do not leak into hosts");
}

#[test]
fn known_hosts_match_plain_names_ports_and_report_hashed_files_as_unknown() {
    let plain = "build.example.com,10.0.0.5 ssh-ed25519 AAAA\n[box.example.com]:2222 ssh-rsa AAAA\n@cert-authority other.example.com ssh-rsa AAAA\n";
    assert_eq!(known_host(plain, "build.example.com", 22), Some(true));
    assert_eq!(known_host(plain, "box.example.com", 2222), Some(true));
    assert_eq!(known_host(plain, "box.example.com", 22), Some(false));
    assert_eq!(known_host(plain, "other.example.com", 22), Some(true));
    let hashed = "|1|abc=|def= ssh-ed25519 AAAA\n";
    assert_eq!(known_host(hashed, "build.example.com", 22), None);
}

#[test]
fn known_hosts_defaults_to_the_users_file_only_when_it_exists() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(default_known_hosts(home.path()), None);
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    std::fs::write(home.path().join(".ssh/known_hosts"), "").unwrap();
    assert_eq!(
        default_known_hosts(home.path()),
        Some(home.path().join(".ssh/known_hosts"))
    );
}
