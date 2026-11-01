use super::*;

#[test]
fn normal_launch_uses_profile_discovery_and_information_commands_do_not_launch() {
    assert_eq!(
        Command::parse([]).unwrap(),
        Command::Launch {
            data: None,
            debug: false
        }
    );
    assert_eq!(
        Command::parse(["--version".into()]).unwrap(),
        Command::Version
    );
    assert_eq!(Command::parse(["--help".into()]).unwrap(), Command::Help);
}

#[test]
fn explicit_profiles_require_one_absolute_path_without_creating_it() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("isolated profile");
    assert_eq!(
        Command::parse(["--data-dir".into(), data.clone().into_os_string()]).unwrap(),
        Command::Launch {
            debug: false,
            data: Some(data.clone())
        }
    );
    assert!(
        !data.exists(),
        "Parsing must never create or open a profile"
    );
    for arguments in [
        vec!["--data-dir"],
        vec!["--data-dir", "relative/path"],
        vec!["--version", "--data-dir"],
        vec!["--help", "unexpected"],
        vec!["--unknown"],
    ] {
        assert!(Command::parse(arguments.into_iter().map(OsString::from)).is_err());
    }
}

#[test]
fn debug_can_be_combined_with_an_explicit_profile_in_either_order() {
    for arguments in [
        vec!["--debug", "--data-dir", "/tmp/xylo-debug"],
        vec!["--data-dir", "/tmp/xylo-debug", "--debug"],
    ] {
        assert_eq!(
            Command::parse(arguments.into_iter().map(OsString::from)).unwrap(),
            Command::Launch {
                data: Some("/tmp/xylo-debug".into()),
                debug: true
            }
        );
    }
}
