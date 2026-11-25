//! Windows takes a program without a manifest for an installer if its file
//! name contains a word such as "update", and then refuses to start it
//! without elevation (`update-lore-git-md.exe` ran into this).  A manifest
//! that asks for no elevation turns that guess off.
//!
//! Only the MSVC targets get a manifest, embedded with the same linker
//! options as the `embed-manifest` crate uses.

use std::{env, fs, path::PathBuf};

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#;

fn main() -> std::io::Result<()> {
    println!("cargo::rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return Ok(());
    }
    let manifest =
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR")).join("manifest.xml");
    fs::write(&manifest, MANIFEST)?;
    println!("cargo::rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo::rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
    println!("cargo::rustc-link-arg-bins=/MANIFESTUAC:NO");
    Ok(())
}
