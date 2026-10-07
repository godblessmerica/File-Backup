use anyhow::{Context, Result, bail, ensure};
use chrono::Local;
use flate2::{Compression, write::GzEncoder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::os::windows::{fs::MetadataExt, process::CommandExt};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Config {
    pub compression_level: u32,
    pub date_format: String,
    pub backup_root: String,
    pub log_directory: String,
    pub log_compression_level: u32,
    pub exclude_files: Vec<String>,
    pub exclude_folders: Vec<String>,
    pub overwrite_existing_files: bool,
    pub copy_retries: u32,
    pub retry_delay_seconds: u32,
    pub close_to_tray: bool,
    pub tray_notifications_enabled: bool,
    pub completed_status_dismiss_seconds: u32,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            compression_level: 6,
            date_format: "{year}-{month}-{day}_{hour}-{minute}-{second}".into(),
            backup_root: ".backups".into(),
            log_directory: ".logs".into(),
            log_compression_level: 6,
            exclude_files: vec![],
            exclude_folders: vec![],
            overwrite_existing_files: true,
            copy_retries: 3,
            retry_delay_seconds: 5,
            close_to_tray: true,
            tray_notifications_enabled: true,
            completed_status_dismiss_seconds: 5,
        }
    }
}
impl Config {
    pub fn changes(&self, next: &Self) -> Result<String> {
        let old = serde_json::to_value(self)?;
        let new = serde_json::to_value(next)?;
        let changes: Vec<_> = new
            .as_object()
            .context("Settings must be an object")?
            .iter()
            .filter(|(key, value)| old[*key] != **value)
            .map(|(key, value)| format!("User changed setting {key}: {} -> {value}", old[key]))
            .collect();
        Ok(if changes.is_empty() {
            "User saved settings (no changes)".into()
        } else {
            changes.join("\n")
        })
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            [0, 1, 3, 5, 6, 7, 9].contains(&self.compression_level),
            "Choose a supported ZIP level."
        );
        ensure!(
            self.log_compression_level <= 9,
            "Log compression must be 0–9."
        );
        ensure!(
            self.copy_retries <= 100 && self.retry_delay_seconds <= 300,
            "Retry values exceed allowed limits."
        );
        ensure!(
            !self.backup_root.trim().is_empty() && !self.log_directory.trim().is_empty(),
            "Folder locations cannot be empty."
        );
        ensure!(
            self.completed_status_dismiss_seconds <= 3600,
            "Completed status dismissal must be 0–3600 seconds."
        );
        for p in self.exclude_files.iter().chain(&self.exclude_folders) {
            glob::Pattern::new(p).context("Invalid exclusion pattern")?;
        }
        valid_name(&date_name(&self.date_format)?)?;
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub source: String,
    pub zip: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Request {
    pub name: String,
    pub source: String,
    pub zip: bool,
    #[serde(default)]
    pub overwrite: bool,
}

pub fn id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
pub fn root_path(root: &Path, value: &str) -> PathBuf {
    let p = Path::new(value);
    if p.is_absolute() {
        p.into()
    } else {
        root.join(p)
    }
}
pub fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty() && name == name.trim() && !name.ends_with(['.', ' ']),
        "Enter a valid backup name."
    );
    ensure!(
        !name.chars().any(|c| c < ' ' || "<>:\"/\\|?*".contains(c)),
        "Name contains characters Windows cannot use."
    );
    let stem = name.split('.').next().unwrap_or("").to_uppercase();
    ensure!(
        ![".", "..", ".logs", ".backups"].contains(&name.to_lowercase().as_str()),
        "Reserved backup name."
    );
    ensure!(
        !["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&stem.as_str())
            && !(stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit()),
        "Reserved Windows name."
    );
    ensure!(name.len() <= 180, "Name is too long.");
    Ok(())
}
pub fn date_name(template: &str) -> Result<String> {
    let now = Local::now();
    let mut text = template.to_owned();
    for (key, format) in [
        ("year", "%Y"),
        ("month", "%m"),
        ("day", "%d"),
        ("hour", "%H"),
        ("minute", "%M"),
        ("second", "%S"),
        ("time", "%H%M%S"),
    ] {
        text = text.replace(&format!("{{{key}}}"), &now.format(format).to_string());
    }
    ensure!(!text.contains(['{', '}']), "Unknown date placeholder.");
    Ok(text)
}
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", id()));
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    let result = replace_file(&temporary, path);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    // Preserve the previous file until publishing succeeds, including on Windows.
    let previous = to.with_extension(format!("{}.previous", id()));
    let exists = to.exists();
    if exists {
        fs::rename(to, &previous)?;
    }
    if let Err(error) = fs::rename(from, to) {
        if exists {
            let _ = fs::rename(&previous, to);
        }
        return Err(error.into());
    }
    if exists {
        fs::remove_file(previous)?;
    }
    Ok(())
}
pub(crate) fn no_links(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        if ancestor.exists() {
            ensure!(
                fs::symlink_metadata(ancestor)?.file_attributes() & 0x400 == 0,
                "Linked paths are not supported: {}",
                ancestor.display()
            );
        }
    }
    Ok(())
}
fn within(child: &Path, parent: &Path) -> bool {
    let child = child.to_string_lossy().to_lowercase();
    let parent = parent
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_lowercase();
    child == parent || child.starts_with(&(parent + "\\"))
}
fn excluded(path: &Path, relative: &Path, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        glob::Pattern::new(pattern).is_ok_and(|p| {
            let options = glob::MatchOptions {
                case_sensitive: false,
                require_literal_separator: false,
                require_literal_leading_dot: false,
            };
            p.matches_with(
                &path.file_name().unwrap_or_default().to_string_lossy(),
                options,
            ) || p.matches_with(&relative.to_string_lossy(), options)
                || p.matches_with(&path.to_string_lossy(), options)
        })
    })
}
fn collect(
    path: &Path,
    source: &Path,
    config: &Config,
    files: &mut Vec<(PathBuf, u64)>,
    exclusions: &mut Vec<PathBuf>,
) -> Result<()> {
    let info = fs::symlink_metadata(path)?;
    ensure!(
        info.file_attributes() & 0x400 == 0,
        "Source contains a linked path: {}",
        path.display()
    );
    let relative = path.strip_prefix(source).unwrap_or(path);
    if path != source
        && excluded(
            path,
            relative,
            if info.is_dir() {
                &config.exclude_folders
            } else {
                &config.exclude_files
            },
        )
    {
        exclusions.push(path.to_owned());
        return Ok(());
    }
    if info.is_dir() {
        for item in fs::read_dir(path)? {
            collect(&item?.path(), source, config, files, exclusions)?;
        }
    } else if !excluded(path, relative, &config.exclude_files) {
        files.push((path.to_owned(), info.len()));
    } else {
        exclusions.push(path.to_owned());
    }
    Ok(())
}
pub struct Prepared {
    pub request: Request,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub files: Vec<(PathBuf, u64)>,
    pub exclusions: Vec<PathBuf>,
}
pub fn prepare(root: &Path, config: &Config, mut request: Request) -> Result<Prepared> {
    config.validate()?;
    request.name = date_name(&request.name)?;
    valid_name(&request.name)?;
    no_links(Path::new(&request.source))?;
    let source = fs::canonicalize(&request.source).context("Source not found")?;
    // Canonical paths may have a Win32 extended prefix; ordinary paths work with native tools and display.
    let source = PathBuf::from(source.to_string_lossy().trim_start_matches(r"\\?\"));
    no_links(&source)?;
    let backup_root = root_path(root, &config.backup_root);
    fs::create_dir_all(&backup_root)?;
    no_links(&backup_root)?;
    let backup_root = fs::canonicalize(&backup_root)?;
    let backup_root = PathBuf::from(backup_root.to_string_lossy().trim_start_matches(r"\\?\"));
    let name = if request.zip {
        format!("{}.zip", request.name.trim_end_matches(".zip"))
    } else {
        request.name.clone()
    };
    let destination = backup_root.join(name);
    no_links(&destination)?;
    ensure!(
        !within(&destination, &source) && !within(&source, &destination),
        "Source and destination must be separate."
    );
    if destination.exists() {
        ensure!(
            request.overwrite,
            "Destination already exists; confirm updating or replacing it."
        );
        ensure!(
            destination.is_dir() != request.zip,
            "Existing backup is a different type."
        );
    }
    let mut files = vec![];
    let mut exclusions = vec![];
    collect(&source, &source, config, &mut files, &mut exclusions)?;
    Ok(Prepared {
        request,
        source,
        destination,
        files,
        exclusions,
    })
}
pub fn list_backups(root: &Path, config: &Config) -> Result<Vec<Value>> {
    let location = root_path(root, &config.backup_root);
    fs::create_dir_all(&location)?;
    let mut entries = vec![];
    for item in fs::read_dir(location)? {
        let item = item?;
        let metadata = fs::symlink_metadata(item.path())?;
        if metadata.file_attributes() & 0x400 == 0
            && (metadata.is_dir()
                || item
                    .path()
                    .extension()
                    .is_some_and(|v| v.eq_ignore_ascii_case("zip")))
        {
            entries.push(
                json!({"name": item.file_name().to_string_lossy(), "zip": metadata.is_file()}),
            );
        }
    }
    entries.sort_by_key(|a| a["name"].as_str().unwrap_or("").to_lowercase());
    Ok(entries)
}
pub fn backup_path(root: &Path, config: &Config, name: &str) -> Result<PathBuf> {
    valid_name(name)?;
    let location = root_path(root, &config.backup_root).join(name);
    no_links(&location)?;
    ensure!(location.exists(), "Backup no longer exists.");
    ensure!(
        location.is_dir()
            || location
                .extension()
                .is_some_and(|v| v.eq_ignore_ascii_case("zip")),
        "Not a backup."
    );
    Ok(location)
}
pub fn seven_zip() -> Result<PathBuf> {
    for env in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(env) {
            let path = PathBuf::from(root).join("7-Zip\\7z.exe");
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    bail!("Install 7-Zip to create ZIP backups.")
}
pub fn audit(root: &Path, config: &Config, message: &str) -> Result<()> {
    let location = root_path(root, &config.log_directory);
    fs::create_dir_all(&location)?;
    no_links(&location)?;
    static SESSION_LOGS: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();
    let mut logs = SESSION_LOGS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    let file = if let Some(path) = logs.get(&location) {
        no_links(path)?;
        OpenOptions::new().append(true).create(true).open(path)?
    } else {
        let stamp = date_name(&config.date_format)?;
        let mut path = location.join(format!("{stamp}.log.gz"));
        let mut result = OpenOptions::new().write(true).create_new(true).open(&path);
        if result
            .as_ref()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
        {
            path = location.join(format!(
                "{stamp}-{:09}.log.gz",
                Local::now().timestamp_subsec_nanos()
            ));
            result = OpenOptions::new().write(true).create_new(true).open(&path);
        }
        let file = result?;
        logs.insert(location, path);
        file
    };
    let mut gzip = GzEncoder::new(file, Compression::new(config.log_compression_level));
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    for line in message.lines().filter(|line| !line.trim().is_empty()) {
        writeln!(gzip, "[{timestamp}] {}", line.replace('\r', ""))?;
    }
    gzip.finish()?;
    Ok(())
}
struct Lock {
    path: PathBuf,
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
pub fn lock_destination(path: &Path) -> Result<impl Drop> {
    let lock = path.with_file_name(format!(
        ".{}.filebackup-lock",
        path.file_name().unwrap().to_string_lossy()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .context("This backup is already in use. A stale lock may remain after a crash.")?;
    writeln!(file, "{}", std::process::id())?;
    Ok(Lock { path: lock })
}
fn run(command: &mut Command, cancel: &AtomicBool, output: &mut impl FnMut(&str)) -> Result<i32> {
    command
        .creation_flags(0x08000000)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    // Bound queued tool output so large backups cannot accumulate unlimited lines.
    let (sender, receiver) = std::sync::mpsc::sync_channel(64);
    for stream in [
        Box::new(stdout) as Box<dyn Read + Send>,
        Box::new(stderr) as Box<dyn Read + Send>,
    ] {
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut stream = stream;
            let mut line = vec![];
            let mut bytes = [0u8; 4096];
            loop {
                match stream.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        for b in &bytes[..count] {
                            if *b == b'\r' || *b == b'\n' {
                                if !line.is_empty() {
                                    if sender
                                        .send(String::from_utf8_lossy(&line).into_owned())
                                        .is_err()
                                    {
                                        return;
                                    }
                                    line.clear();
                                }
                            } else {
                                line.push(*b);
                            }
                        }
                    }
                }
            }
            if !line.is_empty() {
                let _ = sender.send(String::from_utf8_lossy(&line).into_owned());
            }
        });
    }
    drop(sender);
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Cancelled. Files already copied remain in the backup.");
        }
        match receiver.recv_timeout(Duration::from_millis(80)) {
            Ok(line) => output(&line),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) => {}
        }
    }
    Ok(child.wait()?.code().unwrap_or(-1))
}
pub fn execute(
    mut prepared: Prepared,
    root: &Path,
    config: &Config,
    cancel: Arc<AtomicBool>,
    mut emit: impl FnMut(Value),
) -> Result<()> {
    let _lock = lock_destination(&prepared.destination)?;
    let source = &prepared.source;
    let destination = &prepared.destination;
    let source_is_dir = source.is_dir();
    let total: u64 = prepared.files.iter().map(|(_, size)| size).sum();
    let count = prepared.files.len();
    let existed = destination.exists();
    let backup_name = destination.file_name().unwrap().to_string_lossy();
    audit(
        root,
        config,
        &format!(
            "User started {}backup \"{}\". Source: \"{}\". Destination: \"{}\".",
            if prepared.request.zip { "ZIP " } else { "" },
            backup_name,
            source.display(),
            destination.display(),
        ),
    )?;
    // Keep only recent tool output for failure diagnostics; routine logs stay readable.
    let mut tool_log = VecDeque::with_capacity(20);
    let sizes: HashMap<String, u64> = std::mem::take(&mut prepared.files)
        .into_iter()
        .map(|(p, s)| (p.to_string_lossy().to_lowercase(), s))
        .collect();
    let mut done = HashSet::new();
    let mut current = String::new();
    let mut completed_bytes = 0u64;
    let mut tool_percent = 0u32;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut output = |line: &str| {
        if !line.trim().is_empty() {
            if tool_log.len() == 20 {
                tool_log.pop_front();
            }
            tool_log.push_back(line.trim().to_owned());
        }
        let trimmed = line.trim();
        if let Some(percent) = trimmed
            .split('%')
            .next()
            .and_then(|v| v.trim().parse::<f64>().ok())
        {
            tool_percent = (percent as u32).min(99);
        }
        let file = if prepared.request.zip {
            let detail = trimmed
                .split_once("+ ")
                .or_else(|| trimmed.split_once("T "))
                .map(|(_, name)| name);
            detail
                .map(|name| {
                    if source_is_dir {
                        source.join(name)
                    } else {
                        source.parent().unwrap().join(name)
                    }
                })
                .map(|p| p.to_string_lossy().to_lowercase())
        } else {
            line.rsplit('\t').next().map(|v| v.trim().to_lowercase())
        };
        if let Some(file) = file.filter(|p| sizes.contains_key(p)) {
            if current != file {
                if !current.is_empty() && done.insert(current.clone()) {
                    completed_bytes += sizes.get(&current).copied().unwrap_or(0);
                }
                current = file;
                if !prepared.request.zip {
                    tool_percent = 0;
                }
            }
        }
        if last_draw.elapsed() < Duration::from_millis(100) {
            return;
        }
        last_draw = Instant::now();
        let bytes = if prepared.request.zip {
            total.saturating_mul(tool_percent as u64) / 100
        } else {
            let size = sizes.get(&current).copied().unwrap_or(0);
            completed_bytes + size.saturating_mul(tool_percent as u64) / 100
        };
        let percent = if prepared.request.zip {
            tool_percent
        } else if total == 0 {
            0
        } else {
            (bytes.saturating_mul(100) / total).min(99) as u32
        };
        emit(
            json!({"percent":percent,"files":done.len(),"totalFiles":count,"bytes":bytes,"totalBytes":total,"source":source,"destination":destination,"detail":trimmed}),
        );
    };
    output("Preparing backup");
    let result = (|| -> Result<()> {
        ensure!(!cancel.load(Ordering::Relaxed), "Cancelled.");
        // Recheck after acquiring the cross-process destination lock.
        if destination.exists() {
            ensure!(prepared.request.overwrite, "Destination already exists.");
        }
        if prepared.request.zip {
            let partial = destination.with_extension(format!("{}.partial", id()));
            let archive_result = (|| -> Result<()> {
                let (working, input) = if source_is_dir {
                    (source.as_path(), PathBuf::from("."))
                } else {
                    (
                        source.parent().unwrap(),
                        PathBuf::from(source.file_name().unwrap()),
                    )
                };
                let mut create = Command::new(seven_zip()?);
                create
                    .current_dir(working)
                    .args(["a", "-tzip", "-spd", "-bb1", "-bsp1"])
                    .arg(if config.compression_level == 0 {
                        "-mm=Copy"
                    } else {
                        "-mm=Deflate"
                    })
                    .arg(format!("-mx={}", config.compression_level))
                    .arg(&partial);
                for excluded in &prepared.exclusions {
                    create.arg(format!("-x!{}", excluded.strip_prefix(working)?.display()));
                }
                create.arg("--").arg(input);
                ensure!(
                    run(&mut create, &cancel, &mut output)? == 0,
                    "ZIP creation failed."
                );
                output("Verifying ZIP");
                ensure!(
                    run(
                        Command::new(seven_zip()?)
                            .args(["t", "-tzip", "-bsp1"])
                            .arg(&partial),
                        &cancel,
                        &mut output
                    )? == 0,
                    "ZIP verification failed."
                );
                ensure!(!cancel.load(Ordering::Relaxed), "Cancelled.");
                replace_file(&partial, destination)?;
                Ok(())
            })();
            if partial.exists() {
                let _ = fs::remove_file(&partial);
            }
            archive_result?;
        } else {
            fs::create_dir_all(destination)?;
            // Validate existing contents too; Robocopy must never write through a junction.
            let mut existing = vec![];
            collect(
                destination,
                destination,
                &Config::default(),
                &mut existing,
                &mut vec![],
            )?;
            let mut copy = Command::new("robocopy.exe");
            if source_is_dir {
                copy.arg(source).arg(destination).arg("/E");
            } else {
                copy.arg(source.parent().unwrap())
                    .arg(destination)
                    .arg(source.file_name().unwrap());
            }
            copy.args(["/Z", "/XJ", "/SL", "/COPY:DAT", "/DCOPY:T", "/BYTES", "/FP"])
                .arg(format!("/R:{}", config.copy_retries))
                .arg(format!("/W:{}", config.retry_delay_seconds));
            if !config.overwrite_existing_files {
                copy.args(["/XC", "/XN", "/XO"]);
            }
            if !config.exclude_files.is_empty() {
                copy.arg("/XF").args(&config.exclude_files);
            }
            if !config.exclude_folders.is_empty() {
                copy.arg("/XD").args(&config.exclude_folders);
            }
            let code = run(&mut copy, &cancel, &mut output)?;
            ensure!(
                (0..8).contains(&code),
                "Copy failed (Robocopy exit code {code})."
            );
        }
        Ok(())
    })();
    drop(output);
    let message = match &result {
        Ok(_) => format!(
            "User {} \"{}\". Backup complete; {count} files processed.",
            if !existed {
                "created backup"
            } else if prepared.request.zip {
                "overwrote ZIP backup"
            } else {
                "added files to backup"
            },
            backup_name
        ),
        Err(_) if cancel.load(Ordering::Relaxed) => {
            format!("User cancelled backup \"{}\".", backup_name)
        }
        Err(error) => format!("Backup \"{}\" failed: {error:#}", backup_name),
    };
    let log_result = audit(root, config, &message);
    if result.is_err() && !cancel.load(Ordering::Relaxed) {
        for line in &tool_log {
            let _ = audit(root, config, &format!("Backup tool: {line}"));
        }
    }
    result?;
    log_result.context("Backup completed, but saving its log failed")?;
    emit(
        json!({"percent":100,"files":count,"totalFiles":count,"bytes":total,"totalBytes":total,"source":source,"destination":destination,"detail":"Backup complete"}),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_output_is_drained_without_loss() {
        let mut count = 0;
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/C", "for /L %i in (1,1,2048) do @echo output-%i"]);
        assert_eq!(
            run(&mut command, &AtomicBool::new(false), &mut |line| {
                assert!(line.starts_with("output-"));
                count += 1;
            })
            .unwrap(),
            0
        );
        assert_eq!(count, 2048);
    }
    #[test]
    fn names_templates_and_defaults() {
        for name in ["", "../escape", "CON", "thing:", "bad.", ".logs"] {
            assert!(valid_name(name).is_err(), "{name}");
        }
        assert!(valid_name("School backup").is_ok());
        assert!(!date_name("backup-{year}-{time}").unwrap().contains('{'));
        assert!(date_name("{unknown}").is_err());
        assert_eq!(Config::default().compression_level, 6);
        assert_eq!(Config::default().completed_status_dismiss_seconds, 5);
        let mut config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.completed_status_dismiss_seconds, 5);
        config.completed_status_dismiss_seconds = 3601;
        assert!(config.validate().is_err());
    }
    #[test]
    fn copy_zip_update_cancel_and_locks() {
        let root = std::env::temp_dir().join(format!("filebackup-v3-test-{}", id()));
        fs::create_dir_all(root.join("source/cache")).unwrap();
        fs::write(root.join("source/a.txt"), "hello").unwrap();
        fs::write(root.join("source/cache/skip.txt"), "skip").unwrap();
        let config = Config {
            exclude_folders: vec!["cache".into()],
            copy_retries: 0,
            retry_delay_seconds: 0,
            ..Config::default()
        };
        let request = Request {
            name: "School".into(),
            source: root.join("source").to_string_lossy().into_owned(),
            zip: false,
            overwrite: false,
        };
        let p = prepare(&root, &config, request.clone()).unwrap();
        assert_eq!(p.files.len(), 1);
        let lock = lock_destination(&p.destination).unwrap();
        assert!(lock_destination(&p.destination).is_err());
        drop(lock);
        execute(p, &root, &config, Arc::new(AtomicBool::new(false)), |_| {}).unwrap();
        assert_eq!(
            fs::read(root.join(".backups/School/a.txt")).unwrap(),
            b"hello"
        );
        assert!(!root.join(".backups/School/cache").exists());
        assert!(prepare(&root, &config, request.clone()).is_err());
        let mut request = request;
        request.overwrite = true;
        fs::write(root.join("source/a.txt"), "hello again").unwrap();
        execute(
            prepare(&root, &config, request.clone()).unwrap(),
            &root,
            &config,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        assert_eq!(
            fs::read(root.join(".backups/School/a.txt")).unwrap(),
            b"hello again"
        );
        request.zip = true;
        execute(
            prepare(&root, &config, request.clone()).unwrap(),
            &root,
            &config,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        assert!(root.join(".backups/School.zip").exists());
        assert!(
            execute(
                prepare(&root, &config, request).unwrap(),
                &root,
                &config,
                Arc::new(AtomicBool::new(true)),
                |_| {}
            )
            .is_err()
        );
        assert!(
            fs::read_dir(root.join(".logs")).unwrap().all(|p| p
                .unwrap()
                .path()
                .extension()
                .unwrap()
                == "gz")
        );
        let log_file = fs::read_dir(root.join(".logs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut log = String::new();
        flate2::read::MultiGzDecoder::new(fs::File::open(&log_file).unwrap())
            .read_to_string(&mut log)
            .unwrap();
        assert!(
            log.lines()
                .all(|line| line.starts_with('[') && line.contains("] "))
        );
        assert!(log.contains("User created backup \"School\""));
        assert!(log.contains("User added files to backup \"School\""));
        assert!(log.contains("User cancelled backup \"School.zip\""));
        assert!(!log.contains("BACKUP_STARTED"));
        fs::remove_file(log_file).unwrap();
        audit(&root, &config, "User reopened FileBackup").unwrap();
        assert_eq!(fs::read_dir(root.join(".logs")).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
