//! Builds the two helpers this program carries inside it.
//!
//! The login helper (`crates/opentobii-confirm`) is built with a second
//! cargo into its own target folder under `OUT_DIR`, so that
//! `opentobii login-helper install` needs nothing but this binary and an
//! update brings the matching helper with it.
//!
//! Builds the Windows helper that goes into a game's prefix and embeds it in
//! the binary, so `opentobii install` needs nothing but itself.
//!
//! The helper is C and needs the MinGW cross compiler. Without it the build
//! still succeeds, with an empty helper: `opentobii install` then says so
//! and takes `--helper PATH` instead.

use std::path::PathBuf;
use std::process::Command;

const COMPILER: &str = "x86_64-w64-mingw32-gcc";

fn main() {
    #[cfg(feature = "gui")]
    {
        let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
        slint_build::compile_with_config("ui/app.slint", config).expect("ui/app.slint compiles");
    }
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../helper/sesp_relay.c");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("sesp_relay.exe");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=OPENTOBII_REQUIRE_HELPER");

    let built = Command::new(COMPILER)
        .args(["-O2", "-Wall", "-Wextra", "-o"])
        .arg(&out)
        .arg(&source)
        .args(["-lws2_32", "-lcfgmgr32", "-ladvapi32", "-static"])
        .status();
    login_helper();
    match built {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("{COMPILER} failed to build the helper: {status}"),
        // A release build sets this, so it can never ship without the helper.
        Err(_) if std::env::var_os("OPENTOBII_REQUIRE_HELPER").is_some() => {
            panic!("{COMPILER} not found, and OPENTOBII_REQUIRE_HELPER is set");
        }
        Err(_) => {
            println!("cargo:warning={COMPILER} not found: building without the prefix helper");
            std::fs::write(&out, b"").expect("OUT_DIR is writable");
        }
    }
}

fn login_helper() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../opentobii-confirm");
    println!(
        "cargo:rerun-if-changed={}",
        source.join("src/main.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        source.join("Cargo.toml").display()
    );
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let target = out.join("login-helper-target");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--release", "--locked", "-p", "opentobii-confirm"])
        .arg("--target-dir")
        .arg(&target)
        .current_dir(&source)
        // This build's own target folder and target must not leak into it.
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET")
        .status()
        .expect("cargo runs to build the login helper");
    assert!(
        status.success(),
        "building the login helper failed: {status}"
    );
    std::fs::copy(
        target.join("release/opentobii-confirm"),
        out.join("opentobii-confirm"),
    )
    .expect("the login helper was built");
}
