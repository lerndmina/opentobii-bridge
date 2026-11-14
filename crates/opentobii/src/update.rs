//! Updating opentobii from its GitHub releases.
//!
//! An update is three files from one release: the program, the list of
//! checksums, and a signature over that list. The signature is checked
//! against the public key built into this program before anything else is
//! believed, so a release that was not signed by the project's key is
//! refused whoever published it. Only a copy installed in the home folder
//! updates itself; a packaged copy belongs to the package manager.

use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::install::{self, Kind, Layout};

/// The project's release-signing key. Its secret half signs each release's
/// checksum list.
pub const PUBLIC_KEY: &str = include_str!("../../../packaging/update-key.pub");

const REPOSITORY: &str = "lerndmina/opentobii-bridge";
const LATEST: &str = "https://api.github.com/repos/lerndmina/opentobii-bridge/releases/latest";
/// Every file of a release is downloaded from under here and nowhere else.
const DOWNLOADS: &str = "https://github.com/lerndmina/opentobii-bridge/releases/download/";
const CHECKSUMS: &str = "SHA256SUMS";
const SIGNATURE: &str = "SHA256SUMS.minisig";

/// How a fetch reports an address that does not exist.
pub const NOT_FOUND: &str = "not found: ";

const SMALL_FILE: usize = 256 * 1024;
const PROGRAM_LIMIT: usize = 200 * 1024 * 1024;

/// The version this program is.
#[must_use]
pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the package version is three numbers")
}

/// A version: three numbers, compared in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// Read `1.2.3` or `v1.2.3`. Anything else, including a pre-release
    /// suffix, is not a version this updater installs.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.strip_prefix('v').unwrap_or(text).split('.');
        let mut number = || parts.next()?.parse::<u32>().ok();
        let version = Self(number()?, number()?, number()?);
        parts.next().is_none().then_some(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The name a version's program file has among a release's files.
#[must_use]
pub fn program_name(version: Version) -> String {
    format!("opentobii-{version}-x86_64-linux")
}

/// What is known about a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// What changed, as written on the release page.
    pub notes: String,
    /// The release's page, for a person to read.
    pub page: String,
    /// The release's files: name and download address.
    pub assets: Vec<(String, String)>,
}

impl Release {
    fn address(&self, name: &str) -> Result<&str, String> {
        let (_, address) = self
            .assets
            .iter()
            .find(|(asset, _)| asset == name)
            .ok_or_else(|| format!("release {} has no file named {name}", self.version))?;
        if !address.starts_with(DOWNLOADS) {
            return Err(format!(
                "release {} points {name} at an address outside the project",
                self.version
            ));
        }
        Ok(address)
    }
}

/// Fetches the bytes at an address. A trait so tests need no network.
pub trait Fetch {
    fn get(&self, address: &str, limit: usize) -> Result<Vec<u8>, String>;
}

/// The real thing: HTTPS with a time limit and a size limit.
#[derive(Debug, Default, Clone, Copy)]
pub struct Http;

impl Fetch for Http {
    fn get(&self, address: &str, limit: usize) -> Result<Vec<u8>, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .https_only(true)
            .build()
            .into();
        let agent_name = format!(
            "opentobii/{} (+https://github.com/{REPOSITORY})",
            env!("CARGO_PKG_VERSION")
        );
        let mut response = agent
            .get(address)
            .header("User-Agent", &agent_name)
            .header(
                "Accept",
                "application/vnd.github+json, application/octet-stream",
            )
            .call()
            .map_err(|err| match err {
                ureq::Error::StatusCode(404) => format!("{NOT_FOUND}{address}"),
                other => format!("{address}: {other}"),
            })?;
        response
            .body_mut()
            .with_config()
            .limit(limit as u64)
            .read_to_vec()
            .map_err(|err| format!("{address}: {err}"))
    }
}

/// Read GitHub's description of a release.
pub fn parse_release(json: &[u8]) -> Result<Release, String> {
    let value: serde_json::Value =
        serde_json::from_slice(json).map_err(|err| format!("unreadable release list: {err}"))?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let tag = text("tag_name");
    let version =
        Version::parse(&tag).ok_or_else(|| format!("release tag `{tag}` is not a version"))?;
    let assets = value
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .map(|assets| {
            assets
                .iter()
                .filter_map(|asset| {
                    let name = asset.get("name")?.as_str()?;
                    let address = asset.get("browser_download_url")?.as_str()?;
                    Some((name.to_owned(), address.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release {
        version,
        notes: text("body"),
        page: text("html_url"),
        assets,
    })
}

/// The newest release, if it is newer than `than`.
pub fn newer_release(fetch: &impl Fetch, than: Version) -> Result<Option<Release>, String> {
    let listing = match fetch.get(LATEST, SMALL_FILE) {
        Ok(listing) => listing,
        // No full release has been published yet: nothing to update to.
        Err(reason) if reason.starts_with(NOT_FOUND) => return Ok(None),
        Err(reason) => return Err(reason),
    };
    let release = parse_release(&listing)?;
    Ok((release.version > than).then_some(release))
}

/// Check a checksum list against its signature and a public key.
pub fn verify_checksums(
    checksums: &[u8],
    signature: &[u8],
    public_key: &str,
) -> Result<(), String> {
    let key = minisign_verify::PublicKey::decode(public_key.trim())
        .map_err(|err| format!("this program's update key is unreadable: {err}"))?;
    let signature = std::str::from_utf8(signature)
        .ok()
        .and_then(|text| minisign_verify::Signature::decode(text).ok())
        .ok_or("the release's signature is unreadable")?;
    key.verify(checksums, &signature, false)
        .map_err(|_| "the release is not signed by the opentobii project's key".to_owned())
}

/// The SHA-256 a checksum list gives for a file: lines of `hash  name`.
#[must_use]
pub fn expected_hash(checksums: &str, file_name: &str) -> Option<[u8; 32]> {
    checksums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        if name.trim_start().trim_start_matches('*') != file_name
            || hash.len() != 64
            || !hash.is_ascii()
        {
            return None;
        }
        let mut out = [0u8; 32];
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hash[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(out)
    })
}

/// Download a release's program and prove it is the one the project
/// signed: signature first, then the checksum of the program itself.
pub fn download_verified(
    fetch: &impl Fetch,
    release: &Release,
    public_key: &str,
) -> Result<Vec<u8>, String> {
    let checksums = fetch.get(release.address(CHECKSUMS)?, SMALL_FILE)?;
    let signature = fetch.get(release.address(SIGNATURE)?, SMALL_FILE)?;
    verify_checksums(&checksums, &signature, public_key)?;

    // The program's name carries its version, so a signed list from an
    // older release cannot be passed off as this one.
    let name = program_name(release.version);
    let listed = std::str::from_utf8(&checksums)
        .ok()
        .and_then(|text| expected_hash(text, &name))
        .ok_or_else(|| format!("the signed checksum list does not mention {name}"))?;
    let program = fetch.get(release.address(&name)?, PROGRAM_LIMIT)?;
    if Sha256::digest(&program).as_slice() != listed {
        return Err(format!("{name} does not match its signed checksum"));
    }
    if !program.starts_with(b"\x7fELF") {
        return Err(format!("{name} is not a Linux program"));
    }
    Ok(program)
}

/// Replace the program at `target` with `program`, keeping the old one
/// beside it as `opentobii.old`.
pub fn swap(program: &[u8], target: &Path) -> Result<(), String> {
    let mut staged = target.as_os_str().to_owned();
    staged.push(".update");
    let staged = std::path::PathBuf::from(staged);
    let mut previous = target.as_os_str().to_owned();
    previous.push(".old");
    let previous = std::path::PathBuf::from(previous);

    install::write_atomic(&staged, program, 0o755)?;
    let fail = |what: &str, err: std::io::Error| {
        let _ = std::fs::remove_file(&staged);
        format!("{what}: {err}")
    };
    // A hard link keeps the old program reachable under its second name
    // while the new one takes over the first, so there is no moment with
    // no program at all.
    let _ = std::fs::remove_file(&previous);
    std::fs::hard_link(target, &previous)
        .map_err(|err| fail("cannot keep the current version", err))?;
    std::fs::rename(&staged, target).map_err(|err| fail("cannot put the new version in place", err))
}

/// Put the kept previous version back.
pub fn roll_back(target: &Path) -> Result<(), String> {
    let mut previous = target.as_os_str().to_owned();
    previous.push(".old");
    std::fs::rename(&previous, target)
        .map_err(|err| format!("cannot restore the previous version: {err}"))
}

/// Why this copy does not update itself, if it does not.
pub fn refusal(kind: &Kind) -> Option<String> {
    match kind {
        Kind::Home => None,
        Kind::Packaged => Some(
            "this copy was installed by a package, so your package manager updates it \
             (download the new rpm or deb from the releases page)"
                .to_owned(),
        ),
        Kind::Loose => Some(
            "this copy is not installed; run `opentobii setup` first, and the installed copy can update itself"
                .to_owned(),
        ),
    }
}

/// What an update did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    AlreadyNewest(Version),
    Updated { from: Version, to: Version },
}

/// Update the installed copy at `layout.binary` to the newest release.
pub fn update(fetch: &impl Fetch, layout: &Layout, public_key: &str) -> Result<Outcome, String> {
    let from = current();
    let Some(release) = newer_release(fetch, from)? else {
        return Ok(Outcome::AlreadyNewest(from));
    };
    let program = download_verified(fetch, &release, public_key)?;
    swap(&program, &layout.binary)?;
    Ok(Outcome::Updated {
        from,
        to: release.version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Cursor;

    struct Fake(HashMap<String, Vec<u8>>);

    impl Fetch for Fake {
        fn get(&self, address: &str, limit: usize) -> Result<Vec<u8>, String> {
            let bytes = self
                .0
                .get(address)
                .ok_or_else(|| format!("{address}: not found"))?;
            if bytes.len() > limit {
                return Err(format!("{address}: too large"));
            }
            Ok(bytes.clone())
        }
    }

    struct Signer {
        pair: minisign::KeyPair,
    }

    impl Signer {
        fn new() -> Self {
            Self {
                pair: minisign::KeyPair::generate_unencrypted_keypair().unwrap(),
            }
        }
        fn public(&self) -> String {
            self.pair.pk.to_box().unwrap().to_string()
        }
        fn sign(&self, data: &[u8]) -> Vec<u8> {
            minisign::sign(None, &self.pair.sk, Cursor::new(data), None, None)
                .unwrap()
                .to_string()
                .into_bytes()
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn program(marker: &str) -> Vec<u8> {
        let mut bytes = b"\x7fELF".to_vec();
        bytes.extend_from_slice(marker.as_bytes());
        bytes
    }

    /// A release as GitHub would serve it, signed by `signer`.
    fn published(version: &str, program: &[u8], signer: &Signer) -> Fake {
        let parsed = Version::parse(version).unwrap();
        let name = program_name(parsed);
        let checksums = format!(
            "{}  {name}\n{}  something-else.rpm\n",
            hex(&Sha256::digest(program)),
            "0".repeat(64)
        );
        let base = format!("{DOWNLOADS}v{version}/");
        let json = serde_json::json!({
            "tag_name": format!("v{version}"),
            "body": "What changed.",
            "html_url": format!("https://github.com/{REPOSITORY}/releases/tag/v{version}"),
            "assets": [
                {"name": name, "browser_download_url": format!("{base}{name}")},
                {"name": CHECKSUMS, "browser_download_url": format!("{base}{CHECKSUMS}")},
                {"name": SIGNATURE, "browser_download_url": format!("{base}{SIGNATURE}")},
            ],
        });
        let mut files = HashMap::new();
        files.insert(LATEST.to_owned(), serde_json::to_vec(&json).unwrap());
        files.insert(format!("{base}{name}"), program.to_vec());
        files.insert(format!("{base}{CHECKSUMS}"), checksums.clone().into_bytes());
        files.insert(
            format!("{base}{SIGNATURE}"),
            signer.sign(checksums.as_bytes()),
        );
        Fake(files)
    }

    fn release(fake: &Fake) -> Release {
        parse_release(&fake.0[LATEST]).unwrap()
    }

    #[test]
    fn no_published_release_means_no_update_not_an_error() {
        struct Missing;
        impl Fetch for Missing {
            fn get(&self, address: &str, _: usize) -> Result<Vec<u8>, String> {
                Err(format!("{NOT_FOUND}{address}"))
            }
        }
        assert_eq!(newer_release(&Missing, Version(0, 1, 0)), Ok(None));
        struct Offline;
        impl Fetch for Offline {
            fn get(&self, address: &str, _: usize) -> Result<Vec<u8>, String> {
                Err(format!("{address}: no network"))
            }
        }
        assert!(newer_release(&Offline, Version(0, 1, 0)).is_err());
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert_eq!(Version::parse("v0.2.0"), Some(Version(0, 2, 0)));
        assert_eq!(Version::parse("1.10.3"), Some(Version(1, 10, 3)));
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        assert!(Version(1, 0, 0) > Version(0, 99, 99));
        for bad in ["", "1.2", "1.2.3.4", "1.2.x", "v1.2.3-beta", "latest"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        assert_eq!(Version(0, 2, 1).to_string(), "0.2.1");
        assert_eq!(current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn the_built_in_key_is_a_readable_public_key() {
        assert!(minisign_verify::PublicKey::decode(PUBLIC_KEY.trim()).is_ok());
    }

    #[test]
    fn a_properly_signed_release_downloads_and_verifies() {
        let signer = Signer::new();
        let fake = published("9.0.0", &program("new"), &signer);
        let newer = newer_release(&fake, Version(0, 1, 0)).unwrap().unwrap();
        assert_eq!(newer.version, Version(9, 0, 0));
        assert_eq!(newer.notes, "What changed.");
        assert_eq!(
            download_verified(&fake, &newer, &signer.public()).unwrap(),
            program("new")
        );
    }

    #[test]
    fn the_same_or_an_older_release_is_not_an_update() {
        let signer = Signer::new();
        let fake = published("0.5.0", &program("x"), &signer);
        assert_eq!(newer_release(&fake, Version(0, 5, 0)).unwrap(), None);
        assert_eq!(newer_release(&fake, Version(0, 6, 0)).unwrap(), None);
    }

    #[test]
    fn a_release_signed_by_someone_else_is_refused() {
        let ours = Signer::new();
        let theirs = Signer::new();
        let fake = published("9.0.0", &program("evil"), &theirs);
        let err = download_verified(&fake, &release(&fake), &ours.public()).unwrap_err();
        assert!(
            err.contains("not signed by the opentobii project's key"),
            "{err}"
        );
    }

    #[test]
    fn a_swapped_program_is_refused_even_with_a_good_signature() {
        let signer = Signer::new();
        let mut fake = published("9.0.0", &program("real"), &signer);
        let address = format!("{DOWNLOADS}v9.0.0/{}", program_name(Version(9, 0, 0)));
        fake.0.insert(address, program("tampered"));
        let err = download_verified(&fake, &release(&fake), &signer.public()).unwrap_err();
        assert!(err.contains("does not match its signed checksum"), "{err}");
    }

    #[test]
    fn an_edited_checksum_list_is_refused() {
        let signer = Signer::new();
        let mut fake = published("9.0.0", &program("real"), &signer);
        let tampered = program("tampered");
        let name = program_name(Version(9, 0, 0));
        fake.0
            .insert(format!("{DOWNLOADS}v9.0.0/{name}"), tampered.clone());
        let forged = format!("{}  {name}\n", hex(&Sha256::digest(&tampered)));
        fake.0.insert(
            format!("{DOWNLOADS}v9.0.0/{CHECKSUMS}"),
            forged.into_bytes(),
        );
        let err = download_verified(&fake, &release(&fake), &signer.public()).unwrap_err();
        assert!(err.contains("not signed"), "{err}");
    }

    #[test]
    fn an_old_signed_release_cannot_be_served_under_a_new_tag() {
        let signer = Signer::new();
        // Genuine, signed files of 0.2.0, republished claiming to be 9.0.0.
        let old = published("0.2.0", &program("old and vulnerable"), &signer);
        let old_base = format!("{DOWNLOADS}v0.2.0/");
        let new_base = format!("{DOWNLOADS}v9.0.0/");
        let old_name = program_name(Version(0, 2, 0));
        let new_name = program_name(Version(9, 0, 0));
        let mut files = HashMap::new();
        files.insert(
            format!("{new_base}{new_name}"),
            old.0[&format!("{old_base}{old_name}")].clone(),
        );
        files.insert(
            format!("{new_base}{CHECKSUMS}"),
            old.0[&format!("{old_base}{CHECKSUMS}")].clone(),
        );
        files.insert(
            format!("{new_base}{SIGNATURE}"),
            old.0[&format!("{old_base}{SIGNATURE}")].clone(),
        );
        let replay = Release {
            version: Version(9, 0, 0),
            notes: String::new(),
            page: String::new(),
            assets: [new_name.as_str(), CHECKSUMS, SIGNATURE]
                .iter()
                .map(|name| ((*name).to_owned(), format!("{new_base}{name}")))
                .collect(),
        };
        let err = download_verified(&Fake(files), &replay, &signer.public()).unwrap_err();
        assert!(err.contains("does not mention"), "{err}");
    }

    #[test]
    fn files_are_only_fetched_from_the_projects_own_releases() {
        let signer = Signer::new();
        let fake = published("9.0.0", &program("x"), &signer);
        let mut elsewhere = release(&fake);
        for (_, address) in &mut elsewhere.assets {
            *address = address.replace("lerndmina/opentobii-bridge", "someone/else");
        }
        let err = download_verified(&fake, &elsewhere, &signer.public()).unwrap_err();
        assert!(err.contains("outside the project"), "{err}");
    }

    #[test]
    fn a_release_missing_a_file_or_not_a_program_is_refused() {
        let signer = Signer::new();
        let fake = published("9.0.0", &program("x"), &signer);
        let mut unsigned = release(&fake);
        unsigned.assets.retain(|(name, _)| name != SIGNATURE);
        assert!(download_verified(&fake, &unsigned, &signer.public())
            .unwrap_err()
            .contains("no file named"));

        let not_a_program = published("9.0.0", b"#!/bin/sh\nrm -rf ~\n", &signer);
        let err = download_verified(&not_a_program, &release(&not_a_program), &signer.public())
            .unwrap_err();
        assert!(err.contains("not a Linux program"), "{err}");
    }

    #[test]
    fn reads_hashes_from_a_checksum_list() {
        let list = format!(
            "{}  a.rpm\n{} *b.bin\nnot a line\n",
            "ab".repeat(32),
            "cd".repeat(32)
        );
        assert_eq!(expected_hash(&list, "a.rpm"), Some([0xab; 32]));
        assert_eq!(expected_hash(&list, "b.bin"), Some([0xcd; 32]));
        assert_eq!(expected_hash(&list, "c"), None);
        assert_eq!(expected_hash("zz  a.rpm\n", "a.rpm"), None);
    }

    #[test]
    fn swapping_keeps_the_old_program_and_rolling_back_restores_it() {
        let dir = std::env::temp_dir().join(format!("opentobii-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("opentobii");
        std::fs::write(&target, b"old").unwrap();

        swap(b"new", &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::read(dir.join("opentobii.old")).unwrap(), b"old");
        assert!(!dir.join("opentobii.update").exists());

        roll_back(&target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_whole_update_replaces_the_installed_program() {
        let signer = Signer::new();
        let fake = published("9.0.0", &program("version nine"), &signer);
        let home =
            std::env::temp_dir().join(format!("opentobii-update-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let layout = Layout::new(&home, None, None);
        std::fs::create_dir_all(layout.binary.parent().unwrap()).unwrap();
        std::fs::write(&layout.binary, b"installed").unwrap();

        let outcome = update(&fake, &layout, &signer.public()).unwrap();
        assert_eq!(
            outcome,
            Outcome::Updated {
                from: current(),
                to: Version(9, 0, 0)
            }
        );
        assert_eq!(
            std::fs::read(&layout.binary).unwrap(),
            program("version nine")
        );

        // A bad release leaves the working copy exactly as it was.
        let forger = Signer::new();
        let bad = published("10.0.0", &program("forged"), &forger);
        assert!(update(&bad, &layout, &signer.public()).is_err());
        assert_eq!(
            std::fs::read(&layout.binary).unwrap(),
            program("version nine")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// Run by the release build against the files it is about to publish:
    /// the signature must check out with the key built into the program,
    /// and the list must name this version's program with the right hash.
    #[test]
    #[ignore = "needs a release folder: set OPENTOBII_RELEASE_DIR"]
    fn release_signature() {
        let dir = std::path::PathBuf::from(
            std::env::var_os("OPENTOBII_RELEASE_DIR").expect("OPENTOBII_RELEASE_DIR"),
        );
        let checksums = std::fs::read(dir.join(CHECKSUMS)).unwrap();
        let signature = std::fs::read(dir.join(SIGNATURE)).unwrap();
        verify_checksums(&checksums, &signature, PUBLIC_KEY).unwrap();
        let name = program_name(current());
        let listed = expected_hash(std::str::from_utf8(&checksums).unwrap(), &name)
            .expect("the program is listed");
        let program = std::fs::read(dir.join(&name)).unwrap();
        assert_eq!(Sha256::digest(&program).as_slice(), listed);
    }

    #[test]
    fn only_a_home_install_updates_itself() {
        assert_eq!(refusal(&Kind::Home), None);
        assert!(refusal(&Kind::Packaged)
            .unwrap()
            .contains("package manager"));
        assert!(refusal(&Kind::Loose).unwrap().contains("opentobii setup"));
    }
}
