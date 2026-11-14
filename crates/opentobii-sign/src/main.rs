//! Signs opentobii's release checksums with a minisign key.
//!
//!   opentobii-sign keygen
//!       Print a new key pair: the public key, then the secret key.
//!
//!   opentobii-sign sign FILE
//!       Sign FILE with the secret key in the OPENTOBII_SIGNING_KEY
//!       environment variable and write FILE.minisig beside it.
//!
//! The secret key is not protected by a password: it lives as a secret in
//! the release pipeline, which could not type one. Anyone who holds it can
//! sign updates that every installed copy will accept, so it is kept
//! nowhere else but a backup its owner controls.

use std::io::Cursor;
use std::process::ExitCode;

use minisign::{KeyPair, SecretKeyBox};

const KEY_VARIABLE: &str = "OPENTOBII_SIGNING_KEY";

fn keygen() -> Result<(), String> {
    let pair = KeyPair::generate_unencrypted_keypair().map_err(|err| err.to_string())?;
    let public = pair.pk.to_box().map_err(|err| err.to_string())?;
    let secret = pair.sk.to_box(None).map_err(|err| err.to_string())?;
    println!("--- public key (goes in the repository) ---");
    println!("{}", public.to_string().trim_end());
    println!("--- secret key (goes in the pipeline's secrets and a backup, nowhere else) ---");
    println!("{}", secret.to_string().trim_end());
    Ok(())
}

fn sign(path: &str) -> Result<(), String> {
    let key = std::env::var(KEY_VARIABLE).map_err(|_| format!("{KEY_VARIABLE} is not set"))?;
    let secret = SecretKeyBox::from_string(&key)
        .and_then(SecretKeyBox::into_unencrypted_secret_key)
        .map_err(|err| format!("{KEY_VARIABLE} is not a usable secret key: {err}"))?;
    let contents = std::fs::read(path).map_err(|err| format!("{path}: {err}"))?;
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    let trusted = format!("file:{file_name}");
    let signature = minisign::sign(
        None,
        &secret,
        Cursor::new(&contents),
        Some(&trusted),
        Some("opentobii release"),
    )
    .map_err(|err| err.to_string())?;
    let out = format!("{path}.minisig");
    std::fs::write(&out, signature.to_string()).map_err(|err| format!("{out}: {err}"))?;
    println!("{out}");
    Ok(())
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let result = match (args.next().as_deref(), args.next()) {
        (Some("keygen"), None) => keygen(),
        (Some("sign"), Some(path)) => sign(&path),
        _ => Err("usage: opentobii-sign keygen | opentobii-sign sign FILE".to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("opentobii-sign: {reason}");
            ExitCode::FAILURE
        }
    }
}
