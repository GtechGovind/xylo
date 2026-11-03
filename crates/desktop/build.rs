// The complete native application is the default Cargo build. Developers can
// opt out explicitly for fast checks with --no-default-features.
mod build_icons;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    install_windows_icon();
    #[cfg(feature = "bundled-browser")]
    build_browser();
}

fn install_windows_icon() {
    use std::{env, fs, path::PathBuf};

    println!("cargo:rerun-if-changed=build_icons.rs");
    println!("cargo:rerun-if-changed=assets/xylo-icon.png");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let icon = output.join("xylo.ico");
    let pixels = build_icons::windows_ico(include_bytes!("assets/xylo-icon.png"))
        .expect("encode the Windows application icon");
    fs::write(&icon, pixels).expect("write the Windows application icon");
    let resource = output.join("xylo-icon.rc");
    // Both GPUI Windows backends load numeric icon resource 1 from the main
    // executable. A window-pixel icon is X11-only and cannot substitute for it.
    let icon = icon
        .to_str()
        .expect("icon resource path")
        .replace('\\', "/");
    fs::write(&resource, format!("1 ICON \"{icon}\"\n"))
        .expect("write the Windows icon resource script");
    embed_resource::compile_for(&resource, ["xylo"], embed_resource::NONE)
        .manifest_required()
        .expect("embed Windows icon resource 1 in xylo.exe");
}

#[cfg(feature = "bundled-browser")]
fn build_browser() {
    use std::{env, path::PathBuf};
    use xylo_build::browser::{self, Profile};

    println!("cargo:rerun-if-changed=build.rs");
    for variable in ["CARGO_TARGET_DIR", "RUSTC", "PROCESSOR_ARCHITEW6432"] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = root.canonicalize().expect("workspace root");
    let target = env::var("TARGET").expect("Cargo TARGET");
    let host = env::var("HOST").expect("Cargo HOST");
    let profile = Profile::parse(&env::var("PROFILE").expect("Cargo PROFILE"))
        .expect("supported build profile");
    // OUT_DIR is <target-dir>[/<target>]/<profile>/build/<package>/out.
    // Using Cargo's actual artifact directory honors --target-dir, --target,
    // and custom profile directory names without guessing from environment.
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let destination = out.ancestors().nth(3).expect("Cargo artifact directory");
    browser::build(&root, destination, profile, &target, &host).unwrap_or_else(|error| {
        panic!("bundled browser build failed; refusing an incomplete app: {error:#}")
    });
    // Missing/deleted staged artifacts cause Cargo to rerun the builder.
    let executable = destination.join(if target.contains("windows") {
        "xylo-obscura.exe"
    } else {
        "xylo-obscura"
    });
    println!("cargo:rerun-if-changed={}", executable.display());
    println!(
        "cargo:rerun-if-changed={}",
        destination
            .join("obscura-licenses/XYLO-BUILD.json")
            .display()
    );
}
