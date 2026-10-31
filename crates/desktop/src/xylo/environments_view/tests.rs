use super::{environment_target, operation_state, tested_age};
use std::path::PathBuf;
use xylo_core::{ContainerRuntime, ContainerTarget, EnvironmentOperationState, EnvironmentTarget};

#[test]
fn list_subtitles_name_the_kind_and_where_it_points() {
    let ssh = EnvironmentTarget::Ssh {
        executable: PathBuf::from("/usr/bin/ssh"),
        host: "build.example.com".into(),
        user: "ci".into(),
        port: 2222,
        known_hosts: PathBuf::from("/home/ci/.ssh/known_hosts"),
        identity_file: None,
    };
    assert_eq!(environment_target(&ssh), "SSH · ci@build.example.com:2222");
    let running = EnvironmentTarget::Container {
        executable: PathBuf::from("/usr/bin/docker"),
        runtime: ContainerRuntime::Docker,
        container: ContainerTarget::Existing {
            target: "web-1".into(),
        },
    };
    assert_eq!(environment_target(&running), "Docker · web-1");
    let managed = EnvironmentTarget::Container {
        executable: PathBuf::from("/usr/bin/podman"),
        runtime: ContainerRuntime::Podman,
        container: ContainerTarget::Managed {
            image: "ubuntu:24.04".into(),
            command: Vec::new(),
        },
    };
    assert_eq!(
        environment_target(&managed),
        "Podman · New container from ubuntu:24.04"
    );
}

#[test]
fn unknown_outcomes_read_as_needing_a_check() {
    assert_eq!(
        operation_state(EnvironmentOperationState::OutcomeUnknown),
        "Result needs checking"
    );
    assert_eq!(
        operation_state(EnvironmentOperationState::Running),
        "In progress"
    );
    assert_eq!(tested_age(xylo_core::now_ms()), "just now");
}
