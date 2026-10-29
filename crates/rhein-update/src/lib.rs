//! Updates from GitHub releases: asks for the latest release, downloads its build for this
//! system, checks it against the release key compiled in here, and puts the new binary, desktop
//! entry and icon where the running copy was installed.
//!
//! Only the program is replaced. Settings, accounts and caches live in `~/.config/rhein` and
//! `~/.cache/rhein` and are never touched. A copy under `/usr` (a package, or `./install` as
//! root) or in a directory this user can't write is left alone: Rhein only says an update is out.
//!
//! The release workflow signs each tarball with the private half of [`PUBLIC_KEY`] (pure Ed25519
//! over the whole file, `openssl pkeyutl -sign -rawin`) and publishes the 64-byte signature as
//! `<tarball>.sig`. A release without one is never installed.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde_json::Value;

const LATEST_URL: &str = "https://api.github.com/repos/alpwrk/rhein/releases/latest";
const RELEASES_URL: &str = "https://github.com/alpwrk/rhein/releases";
const APP_ID: &str = "cc.rheinium.Rhein";

/// The release key; its private half is the `RHEIN_SIGNING_KEY` secret of the release workflow.
pub const PUBLIC_KEY: [u8; 32] = [
    0x9c, 0x72, 0x13, 0x15, 0xbf, 0x5d, 0x21, 0x89, 0x83, 0x2d, 0xb2, 0xd4, 0x93, 0xc7, 0xe3, 0x2e, //
    0xc7, 0x6e, 0x68, 0xcf, 0x3a, 0xd5, 0x6f, 0x35, 0xd7, 0x51, 0x31, 0xc7, 0x0c, 0x2d, 0x97, 0xa8,
];

/// No release build is anywhere near this; a bigger download is refused rather than kept in memory.
const MAX_DOWNLOAD: usize = 512 << 20;

/// A release newer than this build.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: Version,
    /// The release's page on GitHub, for its notes.
    pub page_url: String,
    /// The tarball for this system and its signature; `None` when the release has no signed
    /// build for it, so it can only be shown.
    pub download: Option<(String, String)>,
}

/// This build's version.
pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is semver")
}

/// Whether this build looks for updates at all. Debug builds don't, unless pointed at a test
/// server with `RHEIN_DEV_UPDATE_URL` (a `releases/latest` answer).
pub fn enabled() -> bool {
    !cfg!(debug_assertions) || dev_url().is_some()
}

fn dev_url() -> Option<String> {
    std::env::var("RHEIN_DEV_UPDATE_URL").ok().filter(|_| cfg!(debug_assertions))
}

/// The latest release, when it is newer than this build.
pub async fn latest(http: &reqwest::Client) -> Result<Option<Release>> {
    let url = dev_url().unwrap_or_else(|| LATEST_URL.into());
    let answer: Value = http
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(parse(&answer, &current()))
}

/// The release in a `releases/latest` answer, when it is newer than `current`.
pub fn parse(answer: &Value, current: &Version) -> Option<Release> {
    let tag = answer["tag_name"].as_str()?;
    let version = Version::parse(tag.trim_start_matches('v')).ok()?;
    if version <= *current || answer["draft"].as_bool() == Some(true) || answer["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let asset = |name: &str| {
        answer["assets"].as_array()?.iter().find(|a| a["name"].as_str() == Some(name))?["browser_download_url"].as_str().map(str::to_owned)
    };
    let tarball = format!("rhein-{tag}-linux-{}.tar.gz", std::env::consts::ARCH);
    let download = asset(&tarball).zip(asset(&format!("{tarball}.sig")));
    let page_url = answer["html_url"].as_str().map_or_else(|| format!("{RELEASES_URL}/tag/{tag}"), str::to_owned);
    Some(Release { version, page_url, download })
}

/// Downloads the release's tarball and checks its signature.
pub async fn download(http: &reqwest::Client, release: &Release) -> Result<Vec<u8>> {
    let (tarball_url, sig_url) = release.download.as_ref().ok_or_else(|| anyhow!("this release has no signed build for this system"))?;
    let sig = http.get(sig_url).send().await?.error_for_status()?.bytes().await?;
    let mut response = http.get(tarball_url).send().await?.error_for_status()?;
    let mut bytes = Vec::with_capacity(response.content_length().unwrap_or(0).min(MAX_DOWNLOAD as u64) as usize);
    while let Some(chunk) = response.chunk().await? {
        bytes.extend_from_slice(&chunk);
        ensure!(bytes.len() <= MAX_DOWNLOAD, "the download is far larger than any release");
    }
    verify(&bytes, &sig, &PUBLIC_KEY)?;
    Ok(bytes)
}

/// Checks a raw 64-byte Ed25519 signature over `bytes`.
pub fn verify(bytes: &[u8], sig: &[u8], key: &[u8; 32]) -> Result<()> {
    let key = VerifyingKey::from_bytes(key).context("bad release key")?;
    let sig = Signature::from_slice(sig).context("the signature is malformed")?;
    key.verify_strict(bytes, &sig).map_err(|_| anyhow!("the download's signature does not match the release key"))
}

/// Where the running copy is installed (`<prefix>/bin/rhein`), when it may update itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub prefix: PathBuf,
}

impl Target {
    /// `None` for a copy under `/usr`, outside a `bin` directory (a build in `target/`, an
    /// unpacked tarball), or in a directory this user can't write.
    pub fn detect() -> Option<Self> {
        Self::for_exe(&std::env::current_exe().ok()?)
    }

    fn for_exe(exe: &Path) -> Option<Self> {
        // Once replaced, Linux names the file still running "… (deleted)".
        let exe = PathBuf::from(exe.to_str()?.trim_end_matches(" (deleted)"));
        let bin = exe.parent()?;
        if exe.starts_with("/usr") || exe.file_name()? != "rhein" || bin.file_name()? != "bin" || !writable(bin) {
            return None;
        }
        Some(Self { prefix: bin.parent()?.to_owned() })
    }

    pub fn exe(&self) -> PathBuf {
        self.prefix.join("bin/rhein")
    }

    fn desktop(&self) -> PathBuf {
        self.prefix.join(format!("share/applications/{APP_ID}.desktop"))
    }

    fn icon(&self) -> PathBuf {
        self.prefix.join(format!("share/icons/hicolor/512x512/apps/{APP_ID}.png"))
    }
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".rhein-update-probe-{}", std::process::id()));
    let ok = fs::File::create(&probe).is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

/// Puts the binary, desktop entry and icon from a verified tarball in place. The running
/// program keeps its old file open; the new one starts with the next launch.
pub fn install(tarball: &[u8], target: &Target) -> Result<()> {
    let files = unpack(tarball)?;
    let take = |name: &str| files.get(name).ok_or_else(|| anyhow!("the archive has no {name}"));
    let binary = take("rhein")?;
    ensure!(binary.starts_with(b"\x7fELF"), "the archive's rhein is not a program");
    replace(&target.exe(), binary, 0o755)?;
    // The binary is what matters; an entry or icon that can't be written stays as it was.
    let exe = target.exe();
    let desktop = String::from_utf8_lossy(take(&format!("{APP_ID}.desktop"))?)
        .lines()
        .map(|line| match line.strip_prefix("Exec=rhein") {
            Some(args) => format!("Exec={}{args}", exe.display()),
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    if let Err(err) = replace(&target.desktop(), desktop.as_bytes(), 0o644) {
        log::warn!("update: desktop entry not replaced: {err:#}");
    }
    if let Err(err) = take(&format!("{APP_ID}.png")).and_then(|icon| replace(&target.icon(), icon, 0o644)) {
        log::warn!("update: icon not replaced: {err:#}");
    }
    Ok(())
}

/// The regular files of a tarball by file name (it holds one directory of them).
fn unpack(tarball: &[u8]) -> Result<HashMap<String, Vec<u8>>> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    let mut files = HashMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let Some(name) = entry.path()?.file_name().and_then(|n| n.to_str()).map(str::to_owned) else { continue };
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        files.insert(name, bytes);
    }
    Ok(files)
}

/// Writes `bytes` beside `dest` and renames it over, so `dest` is always either whole old or
/// whole new.
fn replace(dest: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = dest.parent().ok_or_else(|| anyhow!("{} has no directory", dest.display()))?;
    fs::create_dir_all(dir)?;
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("rhein");
    let tmp = dir.join(format!(".{name}.new"));
    let written = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
        file.write_all(bytes)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        fs::rename(&tmp, dest)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.with_context(|| format!("couldn't write {}", dest.display()))
}

/// Starts the installed copy, for a restart into the update; the caller quits afterwards.
pub fn launch(target: &Target) -> Result<()> {
    std::process::Command::new(target.exe()).spawn().context("couldn't start the new version")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    fn answer(tag: &str, signed: bool) -> Value {
        let tarball = format!("rhein-{tag}-linux-{}.tar.gz", std::env::consts::ARCH);
        let mut assets = vec![
            json!({ "name": tarball, "browser_download_url": format!("https://dl/{tarball}") }),
            json!({ "name": format!("{tarball}.sha256"), "browser_download_url": "https://dl/sum" }),
        ];
        if signed {
            assets.push(json!({ "name": format!("{tarball}.sig"), "browser_download_url": format!("https://dl/{tarball}.sig") }));
        }
        json!({ "tag_name": tag, "html_url": format!("https://gh/{tag}"), "draft": false, "prerelease": false, "assets": assets })
    }

    #[test]
    fn newer_releases_only() {
        let current = Version::new(0, 2, 0);
        assert_eq!(parse(&answer("v0.1.9", true), &current), None);
        assert_eq!(parse(&answer("v0.2.0", true), &current), None);
        let release = parse(&answer("v0.10.0", true), &current).unwrap();
        assert_eq!(release.version, Version::new(0, 10, 0));
        assert_eq!(release.page_url, "https://gh/v0.10.0");
        let (tarball, sig) = release.download.unwrap();
        assert!(tarball.ends_with(".tar.gz") && sig.ends_with(".tar.gz.sig"));
        let mut pre = answer("v0.3.0", true);
        pre["prerelease"] = json!(true);
        assert_eq!(parse(&pre, &current), None);
    }

    #[test]
    fn unsigned_release_is_shown_but_not_downloaded() {
        let release = parse(&answer("v1.0.0", false), &Version::new(0, 1, 0)).unwrap();
        assert_eq!(release.download, None);
    }

    #[test]
    fn signatures() {
        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        let public = key.verifying_key().to_bytes();
        let data = b"rhein tarball".to_vec();
        let sig = key.sign(&data).to_bytes();
        verify(&data, &sig, &public).unwrap();
        let mut tampered = data.clone();
        tampered[0] ^= 1;
        assert!(verify(&tampered, &sig, &public).is_err());
        let other = SigningKey::generate(&mut rand::rngs::OsRng).verifying_key().to_bytes();
        assert!(verify(&data, &sig, &other).is_err());
        assert!(verify(&data, &sig[..10], &public).is_err());
        VerifyingKey::from_bytes(&PUBLIC_KEY).unwrap();
    }

    #[test]
    fn targets() {
        assert_eq!(Target::for_exe(Path::new("/usr/bin/rhein")), None);
        assert_eq!(Target::for_exe(Path::new("/usr/local/bin/rhein")), None);
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        fs::create_dir_all(dir.path().join("target/production")).unwrap();
        let exe = dir.path().join("bin/rhein");
        assert_eq!(Target::for_exe(&exe), Some(Target { prefix: dir.path().to_owned() }));
        let deleted = PathBuf::from(format!("{} (deleted)", exe.display()));
        assert_eq!(Target::for_exe(&deleted), Some(Target { prefix: dir.path().to_owned() }));
        assert_eq!(Target::for_exe(&dir.path().join("target/production/rhein")), None);
        assert_eq!(Target::for_exe(&dir.path().join("bin/other")), None);
        fs::set_permissions(dir.path().join("bin"), fs::Permissions::from_mode(0o555)).unwrap();
        let read_only = Target::for_exe(&exe);
        fs::set_permissions(dir.path().join("bin"), fs::Permissions::from_mode(0o755)).unwrap();
        // Root writes anywhere, so this only holds for a normal user.
        if writable(Path::new("/")) {
            return;
        }
        assert_eq!(read_only, None);
    }

    fn tarball(binary: &[u8]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let files: [(&str, &[u8]); 4] = [
            ("rhein", binary),
            ("cc.rheinium.Rhein.desktop", b"[Desktop Entry]\nName=Rhein\nExec=rhein %u\n"),
            ("cc.rheinium.Rhein.png", b"png"),
            ("install.sh", b"#!/bin/sh\n"),
        ];
        for (name, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("rhein-v9.9.9-linux-x86_64/{name}"), bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn installs_over_the_old_copy() {
        let dir = tempfile::tempdir().unwrap();
        let target = Target { prefix: dir.path().to_owned() };
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        fs::write(target.exe(), b"\x7fELF old").unwrap();
        install(&tarball(b"\x7fELF new"), &target).unwrap();
        assert_eq!(fs::read(target.exe()).unwrap(), b"\x7fELF new");
        assert_eq!(fs::metadata(target.exe()).unwrap().permissions().mode() & 0o777, 0o755);
        let desktop = fs::read_to_string(target.desktop()).unwrap();
        assert!(desktop.contains(&format!("Exec={} %u\n", target.exe().display())), "{desktop}");
        assert_eq!(fs::read(target.icon()).unwrap(), b"png");
        assert!(!dir.path().join("bin/install.sh").exists());
        assert_eq!(fs::read_dir(dir.path().join("bin")).unwrap().count(), 1, "no temporary file left");
    }

    #[test]
    fn refuses_an_archive_without_a_program() {
        let dir = tempfile::tempdir().unwrap();
        let target = Target { prefix: dir.path().to_owned() };
        assert!(install(&tarball(b"#!/bin/sh\necho hi"), &target).is_err());
        assert!(!target.exe().exists());
    }
}
