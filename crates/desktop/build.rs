//! Embed the application icon in the Windows desktop executable. GPUI supplies
//! its manifest separately; this resource contains only icon group 1.

use std::{env, path::Path};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("desktop crate resides inside the workspace");
    let resource = root.join("packaging/windows/xylo.rc");
    let icon_directory = root.join("packaging/icons");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", resource.display());
    println!(
        "cargo:rerun-if-changed={}",
        icon_directory.join("app.ico").display()
    );
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    embed_resource::compile_for(
        resource,
        ["agent-workbench"],
        embed_resource::ParamsIncludeDirs([icon_directory]),
    )
    .manifest_required()
    .expect("Windows desktop icon resource must compile successfully");
}
