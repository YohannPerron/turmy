use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, BufRead, Cursor, Read, Write};
use std::path::Path;
use tar::Archive;

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/YohannPerron/turmy/releases/latest";
const USER_AGENT: &str = concat!("turmy/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    println!("Checking GitHub for the latest turmy release...");

    let release = fetch_latest_release()?;
    let latest = parse_release_version(&release.tag_name)?;

    if latest <= current {
        if latest == current {
            println!("turmy {current} is already up to date.");
        } else {
            println!(
                "This turmy build ({current}) is newer than the latest GitHub release ({latest})."
            );
        }
        return Ok(());
    }

    println!("A new version is available: {current} -> {latest}");

    let archive_name = format!("turmy-{}.tar.gz", env!("TURMY_TARGET"));
    let checksum_name = format!("{archive_name}.sha256");
    let archive_url = asset_url(&release, &archive_name)?;
    let checksum_url = asset_url(&release, &checksum_name)?;

    print!("Upgrade now? [y/N] ");
    io::stdout().flush()?;

    if !read_confirmation(io::stdin().lock())? {
        println!("Upgrade cancelled.");
        return Ok(());
    }

    println!("Downloading {archive_name}...");
    let archive = download(archive_url)?;
    let checksum = download(checksum_url)?;
    verify_sha256(&archive, &checksum)?;
    println!("SHA-256 checksum verified.");

    install_archive(&archive)?;
    println!("Successfully upgraded turmy to {latest}.");
    Ok(())
}

fn fetch_latest_release() -> Result<Release, Box<dyn Error>> {
    let mut response = ureq::get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT)
        .call()?;
    Ok(response.body_mut().read_json()?)
}

fn download(url: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut response = ureq::get(url).header("User-Agent", USER_AGENT).call()?;
    let mut bytes = Vec::new();
    response.body_mut().as_reader().read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn parse_release_version(tag: &str) -> Result<Version, Box<dyn Error>> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    Ok(Version::parse(version)?)
}

fn asset_url<'a>(release: &'a Release, name: &str) -> Result<&'a str, Box<dyn Error>> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.as_str())
        .ok_or_else(|| {
            io::Error::other(format!(
                "release {} has no asset named {name}; this platform may not be supported",
                release.tag_name
            ))
            .into()
        })
}

fn read_confirmation(mut input: impl BufRead) -> io::Result<bool> {
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn verify_sha256(archive: &[u8], checksum_file: &[u8]) -> Result<(), Box<dyn Error>> {
    let checksum_text = std::str::from_utf8(checksum_file)?;
    let expected = checksum_text
        .split_whitespace()
        .next()
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| io::Error::other("release checksum file is not a valid SHA-256 checksum"))?;
    let actual = sha256_hex(archive);

    if !actual.eq_ignore_ascii_case(expected) {
        return Err(io::Error::other(format!(
            "SHA-256 mismatch: expected {expected}, downloaded archive is {actual}"
        ))
        .into());
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn install_archive(archive: &[u8]) -> Result<(), Box<dyn Error>> {
    if std::env::consts::OS != "linux" {
        return Err(io::Error::other("self-update is currently supported only on Linux").into());
    }

    let current_exe = std::env::current_exe()?;
    let install_dir = current_exe
        .parent()
        .ok_or_else(|| io::Error::other("could not determine the executable directory"))?;
    let temp_dir = tempfile::Builder::new()
        .prefix(".turmy-update-")
        .tempdir_in(install_dir)?;
    let new_exe = temp_dir.path().join("turmy");

    extract_binary(archive, &new_exe)?;
    File::open(&new_exe)?.sync_all()?;
    fs::rename(&new_exe, &current_exe).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not replace {}: {error}. Check that you own the installed binary",
                current_exe.display()
            ),
        )
    })?;

    Ok(())
}

fn extract_binary(archive: &[u8], destination: &Path) -> Result<(), Box<dyn Error>> {
    let decoder = GzDecoder::new(Cursor::new(archive));
    let mut archive = Archive::new(decoder);
    let mut found = false;

    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_ref() != Path::new("turmy") || !entry.header().entry_type().is_file() {
            continue;
        }
        if found {
            return Err(
                io::Error::other("release archive contains more than one turmy binary").into(),
            );
        }

        let mut output = File::create(destination)?;
        io::copy(&mut entry, &mut output)?;
        output.flush()?;
        set_executable(destination)?;
        found = true;
    }

    if !found {
        return Err(io::Error::other("release archive does not contain a turmy binary").into());
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_tags_with_or_without_v() {
        assert_eq!(
            parse_release_version("v1.2.3").unwrap(),
            Version::new(1, 2, 3)
        );
        assert_eq!(
            parse_release_version("1.2.3").unwrap(),
            Version::new(1, 2, 3)
        );
    }

    #[test]
    fn confirmation_accepts_only_yes() {
        assert!(read_confirmation(Cursor::new("y\n")).unwrap());
        assert!(read_confirmation(Cursor::new("YES\n")).unwrap());
        assert!(!read_confirmation(Cursor::new("\n")).unwrap());
        assert!(!read_confirmation(Cursor::new("no\n")).unwrap());
    }

    #[test]
    fn verifies_valid_checksum_and_rejects_mismatch() {
        let archive = b"downloaded archive";
        let checksum = format!("{}\n", sha256_hex(archive));
        assert!(verify_sha256(archive, checksum.as_bytes()).is_ok());
        assert!(verify_sha256(archive, &[b'0'; 64]).is_err());
    }

    #[test]
    fn extracts_only_the_expected_binary() {
        let archive = test_archive(&[("README", b"ignored"), ("turmy", b"new executable")]);
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("turmy");

        extract_binary(&archive, &destination).unwrap();

        assert_eq!(fs::read(destination).unwrap(), b"new executable");
    }

    #[test]
    fn rejects_archive_without_binary() {
        let archive = test_archive(&[("other", b"not the executable")]);
        let temp = tempfile::tempdir().unwrap();
        assert!(extract_binary(&archive, &temp.path().join("turmy")).is_err());
    }

    fn test_archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, *contents).unwrap();
        }
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap()
    }
}
