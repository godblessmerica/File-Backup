use anyhow::{Context, Result, ensure};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::os::windows::process::CommandExt;
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
};

pub const CURRENT: &str = concat!(env!("CARGO_PKG_VERSION"), "-alpha");
pub const RELEASE_PAGE: &str = "https://github.com/godblessmerica/File-Backup/releases/latest";
const API: &str = "https://api.github.com/repos/godblessmerica/File-Backup/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/godblessmerica/File-Backup/releases/download/";
const LIMIT: u64 = 300 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
    pub digest: Option<String>,
}
#[derive(Clone)]
pub struct Prepared {
    pub target: PathBuf,
    pub folder: PathBuf,
    pub digest: String,
    pub size: u64,
}
fn version(tag: &str) -> Result<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).context("Invalid release version")
}
pub fn newer(release: &Release) -> Result<bool> {
    Ok(!release.draft && version(&release.tag_name)? > version(CURRENT)?)
}
impl Release {
    fn asset(&self) -> Result<&Asset> {
        let release_version = version(&self.tag_name)?;
        let asset = self
            .assets
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case("filebackup.exe"))
            .or_else(|| self.assets.iter().find(|a| {
                let name = a.name.to_ascii_lowercase();
                ["filebackup-", "fileback-"].iter().any(|prefix| {
                    name.strip_prefix(prefix)
                        .and_then(|s| s.strip_suffix(".exe"))
                        .is_some_and(|tag| version(tag).is_ok_and(|v| v == release_version))
                })
            }))
            .context(
                "This release has no matching FileBackup EXE. Open the release page to download manually.",
            )?;
        ensure!(
            asset.size >= 64 && asset.size <= LIMIT,
            "Update file size is invalid"
        );
        ensure!(
            asset.browser_download_url.starts_with(DOWNLOAD_PREFIX),
            "Update download is outside the FileBackup repository"
        );
        digest(asset)?;
        Ok(asset)
    }
}
fn digest(asset: &Asset) -> Result<&str> {
    let hash = asset
        .digest
        .as_deref()
        .and_then(|s| s.strip_prefix("sha256:"))
        .context("GitHub has no SHA-256 for this EXE; automatic installation is unavailable.")?;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()),
        "Invalid update checksum"
    );
    Ok(hash)
}
fn curl() -> Result<Command> {
    let root = std::env::var_os("SystemRoot").context("Windows folder is unavailable")?;
    let mut command = Command::new(PathBuf::from(root).join("System32/curl.exe"));
    command.creation_flags(0x08000000);
    command.args([
        "--fail",
        "--location",
        "--silent",
        "--show-error",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "10",
        "--user-agent",
        concat!("FileBackup/", env!("CARGO_PKG_VERSION")),
    ]);
    Ok(command)
}
pub fn check() -> Result<Option<Release>> {
    let output = curl()?
        .args([
            "--max-time",
            "30",
            "--max-filesize",
            "1048576",
            "--header",
            "Accept: application/vnd.github+json",
            API,
        ])
        .output()?;
    // An empty repository has no latest release yet.
    if !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("404") {
        return Ok(None);
    }
    ensure!(
        output.status.success(),
        "GitHub update check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let release: Release = serde_json::from_slice(&output.stdout)?;
    Ok(if newer(&release)? {
        Some(release)
    } else {
        None
    })
}
fn verify(path: &Path, hash: &str, size: u64) -> Result<()> {
    ensure!(
        fs::metadata(path)?.len() == size && size <= LIMIT,
        "Update download is incomplete"
    );
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    ensure!(
        format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(hash),
        "Update checksum does not match GitHub"
    );
    file.rewind()?;
    let mut header = [0; 64];
    file.read_exact(&mut header)?;
    ensure!(&header[..2] == b"MZ", "Update is not a Windows EXE");
    let offset = u32::from_le_bytes(header[60..64].try_into().unwrap()) as u64;
    ensure!(offset >= 64 && offset + 6 <= size, "Invalid EXE header");
    file.seek(SeekFrom::Start(offset))?;
    let mut pe = [0; 6];
    file.read_exact(&mut pe)?;
    ensure!(
        &pe[..4] == b"PE\0\0" && pe[4..] == [0x64, 0x86],
        "Update must be a 64-bit Windows EXE"
    );
    Ok(())
}
pub fn prepare(release: &Release, target: PathBuf) -> Result<Prepared> {
    let asset = release.asset()?;
    let folder = target
        .parent()
        .context("EXE has no folder")?
        .join(".filebackup-update");
    super::backend::no_links(&folder)?;
    fs::create_dir_all(&folder)?;
    let download = folder.join("download.exe");
    let output = curl()?
        .args([
            "--max-time",
            "300",
            "--max-filesize",
            &LIMIT.to_string(),
            "--output",
        ])
        .arg(&download)
        .arg(&asset.browser_download_url)
        .output()?;
    ensure!(
        output.status.success(),
        "Update download failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let hash = digest(asset)?.to_owned();
    verify(&download, &hash, asset.size)?;
    fs::copy(&target, folder.join("helper.exe"))?;
    Ok(Prepared {
        target,
        folder,
        digest: hash,
        size: asset.size,
    })
}
pub fn launch_helper(prepared: &Prepared) -> Result<()> {
    Command::new(prepared.folder.join("helper.exe"))
        .arg("--apply-update")
        .arg(std::process::id().to_string())
        .arg(&prepared.target)
        .arg(&prepared.digest)
        .arg(prepared.size.to_string())
        .creation_flags(0x08000000)
        .spawn()
        .context("Could not start update installer")?;
    Ok(())
}
fn replace(target: &Path, folder: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let previous = folder.join("previous.exe");
    ensure!(
        !previous.exists(),
        "A previous update still needs cleanup; restart FileBackup before updating"
    );
    let target_w = wide(target);
    let download_w = wide(&folder.join("download.exe"));
    let previous_w = wide(&previous);
    // Windows replaces the file atomically and keeps the previous EXE for rollback.
    ensure!(
        unsafe {
            ReplaceFileW(
                target_w.as_ptr(),
                download_w.as_ptr(),
                previous_w.as_ptr(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        } != 0,
        "Could not replace FileBackup: {}",
        std::io::Error::last_os_error()
    );
    if let Err(error) = Command::new(target).creation_flags(0x08000000).spawn() {
        ensure!(
            unsafe {
                ReplaceFileW(
                    target_w.as_ptr(),
                    previous_w.as_ptr(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            } != 0,
            "Could not restart or restore FileBackup: {}",
            std::io::Error::last_os_error()
        );
        return Err(error).context("Could not restart update; old EXE restored");
    }
    Ok(())
}
pub fn apply(args: &[std::ffi::OsString]) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    ensure!(args.len() == 6, "Invalid update installer arguments");
    let pid: u32 = args[2].to_str().context("Invalid process ID")?.parse()?;
    let target = PathBuf::from(&args[3]);
    let helper = std::env::current_exe()?;
    let folder = helper.parent().context("Installer has no folder")?;
    ensure!(
        target.is_absolute()
            && target
                .parent()
                .context("EXE has no folder")?
                .join(".filebackup-update")
                == folder
            && helper.file_name().is_some_and(|name| name == "helper.exe"),
        "Invalid update destination"
    );
    super::backend::no_links(&target)?;
    super::backend::no_links(folder)?;
    let hash = args[4].to_str().context("Invalid checksum")?;
    let size = args[5].to_str().context("Invalid file size")?.parse()?;
    verify(&folder.join("download.exe"), hash, size)?;
    // Wait for the owning process, not a guessed sleep, before replacing its EXE.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        ensure!(
            std::io::Error::last_os_error().raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32),
            "Could not wait for FileBackup to exit"
        );
    } else {
        let result = unsafe { WaitForSingleObject(process, 60_000) };
        unsafe { CloseHandle(process) };
        ensure!(
            result == WAIT_OBJECT_0,
            "FileBackup did not exit; update was not installed"
        );
    }
    replace(&target, folder)
}
pub fn cleanup(root: &Path) {
    let folder = root.join(".filebackup-update");
    if super::backend::no_links(&folder).is_err() {
        return;
    }
    for file in ["download.exe", "helper.exe", "previous.exe"] {
        let _ = fs::remove_file(folder.join(file));
    }
    let _ = fs::remove_dir(folder);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "contacts the public GitHub API"]
    fn latest_github_release_can_be_checked() {
        let release = check().unwrap();
        println!("Newer release: {:?}", release.map(|r| r.tag_name));
    }
    #[test]
    fn versioned_release_asset_names_match_the_tag() {
        for tag in ["v0.3.2-alpha", "v0.3.2-beta", "v0.3.2"] {
            for prefix in ["filebackup", "fileback"] {
                for named_version in [tag, tag.trim_start_matches('v')] {
                    let mut r = release(tag);
                    r.assets[0].name = format!("{prefix}-{named_version}.exe");
                    assert!(r.asset().is_ok(), "{}", r.assets[0].name);
                }
            }
        }
        let mut r = release("v0.3.2-alpha");
        for name in [
            "fileback-v0.3.1-alpha.exe",
            "filebackup-v0.3.2-beta.exe",
            "unrelated-v0.3.2-alpha.exe",
            "filebackup-v0.3.2-alpha.zip",
        ] {
            r.assets[0].name = name.into();
            assert!(r.asset().is_err(), "{name}");
        }
        r.assets[0].name = "FILEBACK-V0.3.2-ALPHA.EXE".into();
        assert!(r.asset().is_ok());
        r.assets[0].digest = None;
        assert!(r.asset().is_err());
        let mut preferred = release("v0.3.2-alpha");
        let mut named = preferred.assets[0].clone();
        named.name = "fileback-v0.3.2-alpha.exe".into();
        preferred.assets.insert(0, named);
        assert_eq!(preferred.asset().unwrap().name, "filebackup.exe");
    }
    fn release(tag: &str) -> Release {
        serde_json::from_value(serde_json::json!({"tag_name":tag,"assets":[{
            "name":"filebackup.exe","size":1024,"digest":format!("sha256:{}", "a".repeat(64)),
            "browser_download_url":format!("{DOWNLOAD_PREFIX}{tag}/filebackup.exe")
        }]}))
        .unwrap()
    }
    #[test]
    fn update_versions_assets_and_checksum_are_validated() {
        assert!(!newer(&release("v0.2.1-alpha")).unwrap());
        assert!(!newer(&release("v0.3.1-alpha")).unwrap());
        assert!(!newer(&release("v0.3.1")).unwrap());
        assert!(!newer(&release("v0.3.2-alpha")).unwrap());
        assert!(newer(&release("v0.3.2")).unwrap());
        assert!(newer(&release("v3.10.0")).unwrap());
        assert!(newer(&release("random")).is_err());
        let mut r = release("v3.1.0");
        r.draft = true;
        assert!(!newer(&r).unwrap());
        r.draft = false;
        assert!(r.asset().is_ok());
        r.assets[0].browser_download_url = "https://example.com/filebackup.exe".into();
        assert!(r.asset().is_err());
        r.assets[0].browser_download_url = format!("{DOWNLOAD_PREFIX}v3.1.0/filebackup.exe");
        r.assets[0].digest = None;
        assert!(r.asset().is_err());
        let folder = std::env::temp_dir().join(format!(
            "filebackup-update-test-{}",
            super::super::backend::id()
        ));
        fs::create_dir_all(&folder).unwrap();
        let mut bytes = vec![0u8; 128];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60] = 64;
        bytes[64..70].copy_from_slice(b"PE\0\0\x64\x86");
        let path = folder.join("test.exe");
        fs::write(&path, &bytes).unwrap();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        verify(&path, &hash, 128).unwrap();
        assert!(verify(&path, &"0".repeat(64), 128).is_err());
        assert!(verify(&path, &hash, 129).is_err());
        bytes[68] = 0x4c;
        bytes[69] = 0x01;
        fs::write(&path, &bytes).unwrap();
        assert!(verify(&path, &format!("{:x}", Sha256::digest(&bytes)), 128).is_err());
        fs::remove_file(&path).unwrap();
        let target = folder.join("original.exe");
        fs::write(&target, b"original EXE").unwrap();
        fs::write(folder.join("download.exe"), b"invalid EXE").unwrap();
        assert!(replace(&target, &folder).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original EXE");
        fs::remove_file(&target).unwrap();
        fs::remove_dir(&folder).unwrap();
    }
}
