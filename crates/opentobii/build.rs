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
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../helper/sesp_relay.c");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("sesp_relay.exe");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-env-changed=PATH");

    let built = Command::new(COMPILER)
        .args(["-O2", "-Wall", "-Wextra", "-o"])
        .arg(&out)
        .arg(&source)
        .args(["-lws2_32", "-lcfgmgr32", "-ladvapi32", "-static"])
        .status();
    match built {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("{COMPILER} failed to build the helper: {status}"),
        Err(_) => {
            println!("cargo:warning={COMPILER} not found: building without the prefix helper");
            std::fs::write(&out, b"").expect("OUT_DIR is writable");
        }
    }
}
