//! Updates from GitHub Releases.
//!
//! A background thread asks the GitHub API for the newest release on the
//! configured channel ("stable": `releases/latest`; "beta": pre-releases
//! too) at startup and every six hours. A newer release shows up as a button
//! in the window's sidebar. Installing downloads the release zip into
//! `%LOCALAPPDATA%\chatty\update`, checks its SHA-256, unpacks it and swaps
//! the files next to chatty.exe: files in use are renamed to `*.old`, which
//! Windows allows for a running exe and its DLLs. Then the new exe starts and
//! this process exits; the new one waits for the single-instance mutex and
//! deletes the `*.old` files. Config and data live elsewhere and stay as
//! they are.

use std::cmp::Ordering;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};

const RELEASES: &str = "https://api.github.com/repos/warflash/chatty/releases";
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
/// Set on the process an update starts (to the old version); it waits for
/// the old process to exit.
pub const RESTART_ENV: &str = "CHATTY_UPDATED_FROM";
/// Replaces the releases API URL (testing against a local server).
const FEED_ENV: &str = "CHATTY_UPDATE_FEED";

pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------- version --

/// `MAJOR.MINOR.PATCH[-PRE]`, ordered as SemVer orders them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    nums: [u64; 3],
    pre: Option<String>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) if !p.is_empty() => (c, Some(p.to_string())),
            Some(_) => return None,
            None => (s, None),
        };
        let mut parts = core.split('.');
        let mut nums = [0u64; 3];
        for n in &mut nums {
            *n = parts.next()?.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(Version { nums, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.nums.cmp(&other.nums).then_with(|| match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => {
                let (mut a, mut b) = (a.split('.'), b.split('.'));
                loop {
                    match (a.next(), b.next()) {
                        (None, None) => return Ordering::Equal,
                        (None, Some(_)) => return Ordering::Less,
                        (Some(_), None) => return Ordering::Greater,
                        (Some(x), Some(y)) => {
                            let o = match (x.parse::<u64>(), y.parse::<u64>()) {
                                (Ok(x), Ok(y)) => x.cmp(&y),
                                (Ok(_), Err(_)) => Ordering::Less,
                                (Err(_), Ok(_)) => Ordering::Greater,
                                (Err(_), Err(_)) => x.cmp(y),
                            };
                            if o != Ordering::Equal {
                                return o;
                            }
                        }
                    }
                }
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c] = self.nums;
        write!(f, "{a}.{b}.{c}")?;
        if let Some(p) = &self.pre {
            write!(f, "-{p}")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- release --

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    /// The release page on GitHub (notes).
    pub page: String,
    zip_name: String,
    zip_url: String,
    size: u64,
    sha256: Checksum,
}

#[derive(Debug, Clone, PartialEq)]
enum Checksum {
    /// From the API's asset `digest`.
    Hex(String),
    /// A `<zip>.sha256` asset uploaded by CI.
    Url(String),
}

/// A release object from the GitHub API, if it carries a Windows zip with a
/// checksum. Drafts are skipped; pre-releases only when `pre` is set.
fn parse_release(v: &serde_json::Value, pre: bool) -> Option<Release> {
    if v["draft"].as_bool() == Some(true) || (!pre && v["prerelease"].as_bool() == Some(true)) {
        return None;
    }
    let tag = v["tag_name"].as_str()?;
    let version = Version::parse(tag)?;
    let assets = v["assets"].as_array()?;
    let name_of = |a: &serde_json::Value| a["name"].as_str().unwrap_or("").to_string();
    let zip = assets.iter().find(|a| name_of(a).ends_with("-windows-x64.zip"))?;
    let zip_name = name_of(zip);
    let digest = zip["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).filter(|h| h.len() == 64);
    let sha256 = match digest {
        Some(h) => Checksum::Hex(h.to_ascii_lowercase()),
        None => {
            let file = assets.iter().find(|a| name_of(a) == format!("{zip_name}.sha256"))?;
            Checksum::Url(file["browser_download_url"].as_str()?.to_string())
        }
    };
    Some(Release {
        tag: tag.to_string(),
        version,
        page: v["html_url"].as_str().unwrap_or("").to_string(),
        zip_url: zip["browser_download_url"].as_str()?.to_string(),
        size: zip["size"].as_u64()?,
        zip_name,
        sha256,
    })
}

fn get_text(url: &str, limit: u64) -> Result<Option<String>, String> {
    let response = crate::models::agent()
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call();
    match response {
        Ok(r) => r.into_body().into_with_config().limit(limit).read_to_string().map(Some).map_err(|e| e.to_string()),
        Err(ureq::Error::StatusCode(404)) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// The newest release on `channel`, if any.
fn newest(channel: &str) -> Result<Option<Release>, String> {
    let base = std::env::var(FEED_ENV).unwrap_or_else(|_| RELEASES.to_string());
    let beta = channel == "beta";
    let url = if beta { format!("{base}?per_page=20") } else { format!("{base}/latest") };
    let Some(text) = get_text(&url, 4 << 20)? else {
        return Ok(None); // no release yet
    };
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let list = if beta { json.as_array().cloned().unwrap_or_default() } else { vec![json] };
    Ok(list.iter().filter_map(|r| parse_release(r, beta)).max_by(|a, b| a.version.cmp(&b.version)))
}

// ------------------------------------------------------------------ state --

#[derive(Debug, Clone, Default)]
pub enum State {
    /// Not a build that can update itself (reason).
    Disabled(&'static str),
    /// `update_channel = "off"`.
    Off,
    #[default]
    Checking,
    UpToDate,
    CheckFailed(String),
    Available(Release),
    /// Download fraction.
    Installing(Release, f32),
    Failed(Release, String),
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static WAKE: OnceLock<Sender<()>> = OnceLock::new();

pub fn state() -> State {
    STATE.lock().unwrap().clone().unwrap_or_default()
}

fn set(s: State) {
    *STATE.lock().unwrap() = Some(s);
    crate::ui::notify();
}

/// Why this build doesn't update itself: debug builds and builds run from
/// cargo's target dir.
fn disabled_reason() -> Option<&'static str> {
    if cfg!(debug_assertions) {
        return Some("Development build, updates are off");
    }
    let exe = std::env::current_exe().unwrap_or_default();
    if exe.components().any(|c| c.as_os_str().eq_ignore_ascii_case("target")) {
        return Some("Running from the build folder, updates are off");
    }
    None
}

/// Starts the checker thread: cleans up after an update, then checks at
/// startup, every six hours and on `check_now`.
pub fn start(cfg: crate::config::Shared) {
    if let Some(why) = disabled_reason() {
        set(State::Disabled(why));
        return;
    }
    let (tx, rx) = mpsc::channel();
    let _ = WAKE.set(tx);
    std::thread::Builder::new()
        .name("update".into())
        .spawn(move || {
            remove_leftovers(&crate::paths::exe_dir());
            let _ = std::fs::remove_dir_all(update_dir());
            loop {
                let channel = cfg.get().update_channel.clone();
                if channel == "off" {
                    if !matches!(state(), State::Installing(..)) {
                        set(State::Off);
                    }
                } else {
                    check(&channel);
                }
                match rx.recv_timeout(CHECK_EVERY) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .expect("spawn update");
}

/// Checks again now (the channel changed, or "Check now").
pub fn check_now() {
    if let Some(tx) = WAKE.get() {
        let _ = tx.send(());
    }
}

fn check(channel: &str) {
    let before = state();
    match before {
        State::Installing(..) => return,
        State::Available(_) | State::Failed(..) => {}
        _ => set(State::Checking),
    }
    let me = current();
    match newest(channel) {
        Ok(Some(r)) if r.version > me => {
            log::info!("update available: {} (running {me})", r.tag);
            set(State::Available(r));
        }
        Ok(_) => set(State::UpToDate),
        Err(e) => {
            log::warn!("update check: {e}");
            if !matches!(before, State::Available(_) | State::Failed(..)) {
                set(State::CheckFailed(e));
            }
        }
    }
}

// ---------------------------------------------------------------- install --

fn update_dir() -> PathBuf {
    crate::paths::local_dir().join("update")
}

/// Downloads and installs the available release, then restarts into it.
pub fn install() {
    let release = match state() {
        State::Available(r) | State::Failed(r, _) => r,
        _ => return,
    };
    set(State::Installing(release.clone(), 0.0));
    std::thread::Builder::new()
        .name("update-install".into())
        .spawn(move || {
            log::info!("installing {}", release.tag);
            let progress = |f: f32| set(State::Installing(release.clone(), f));
            match apply(&release, &crate::paths::exe_dir(), progress).and_then(|()| restart()) {
                Ok(()) => {}
                Err(e) => {
                    log::error!("update to {}: {e}", release.tag);
                    crate::overlay::notice("Update failed");
                    set(State::Failed(release, e));
                }
            }
        })
        .expect("spawn update-install");
}

/// Downloads, verifies and unpacks `r`, then swaps it into `install_dir`.
fn apply(r: &Release, install_dir: &Path, progress: impl Fn(f32)) -> Result<(), String> {
    let dir = update_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let expected = match &r.sha256 {
        Checksum::Hex(h) => h.clone(),
        Checksum::Url(u) => {
            let text = get_text(u, 4096)?.ok_or("checksum file missing")?;
            text.split_whitespace().next().unwrap_or("").to_ascii_lowercase()
        }
    };
    let zip = dir.join(&r.zip_name);
    let digest = download(&r.zip_url, &zip, r.size, progress)?;
    if digest != expected {
        return Err(format!("{}: sha256 {digest}, the release says {expected}", r.zip_name));
    }
    let unpacked = dir.join("new");
    unzip(&zip, &unpacked)?;
    if !unpacked.join("chatty.exe").is_file() {
        return Err("the release zip has no chatty.exe".into());
    }
    swap(&unpacked, install_dir)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Streams `url` to `path`; returns the hex SHA-256.
fn download(url: &str, path: &Path, size: u64, progress: impl Fn(f32)) -> Result<String, String> {
    let response = crate::models::agent().get(url).call().map_err(|e| format!("download: {e}"))?;
    let mut reader = response.into_body().into_with_config().limit(size + 1).reader();
    let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let (mut done, mut last) = (0u64, 0.0f32);
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("download: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        let f = (done as f32 / size.max(1) as f32).min(1.0);
        if f - last >= 0.01 {
            last = f;
            progress(f);
        }
    }
    if done != size {
        return Err(format!("download: got {done} bytes, expected {size}"));
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Unpacks with the tar.exe that ships with Windows (reads zip).
fn unzip(zip: &Path, to: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    let tar = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from("tar.exe"), |r| PathBuf::from(r).join("System32").join("tar.exe"));
    let out = std::process::Command::new(tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(to)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("tar: {e}"))?;
    if !out.status.success() {
        return Err(format!("tar: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(())
}

/// Files under `dir`, relative to it.
fn files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for e in std::fs::read_dir(dir.join(&rel))? {
            let e = e?;
            let rel = rel.join(e.file_name());
            if e.file_type()?.is_dir() {
                stack.push(rel);
            } else {
                out.push(rel);
            }
        }
    }
    Ok(out)
}

/// `<file>.old`, or `<file>.old2`, ... when an earlier one can't be removed.
fn old_name(p: &Path) -> PathBuf {
    let mut n = 1;
    loop {
        let suffix = if n == 1 { ".old".to_string() } else { format!(".old{n}") };
        let mut s = p.as_os_str().to_owned();
        s.push(&suffix);
        let o = PathBuf::from(s);
        if !o.exists() || std::fs::remove_file(&o).is_ok() {
            return o;
        }
        n += 1;
    }
}

/// Copies every file of `from` over `to`, moving replaced files aside as
/// `*.old`. On an error, puts everything back.
fn swap(from: &Path, to: &Path) -> Result<(), String> {
    let list = files(from).map_err(|e| e.to_string())?;
    // (target, where the replaced file went)
    let mut done: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    for rel in list {
        let (src, dst) = (from.join(&rel), to.join(&rel));
        let step = (|| -> std::io::Result<()> {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let old = if dst.exists() {
                let o = old_name(&dst);
                std::fs::rename(&dst, &o)?;
                Some(o)
            } else {
                None
            };
            done.push((dst.clone(), old));
            std::fs::copy(&src, &dst)?;
            Ok(())
        })();
        if let Err(e) = step {
            for (dst, old) in done.iter().rev() {
                let _ = std::fs::remove_file(dst);
                if let Some(o) = old {
                    let _ = std::fs::rename(o, dst);
                }
            }
            return Err(format!("{}: {e}", dst.display()));
        }
    }
    Ok(())
}

/// Deletes `*.old` / `*.oldN` files left by an update, where the file they
/// were replaced by exists.
fn remove_leftovers(dir: &Path) {
    let Ok(list) = files(dir) else { return };
    for rel in list {
        let name = rel.to_string_lossy();
        let Some(i) = name.rfind(".old") else { continue };
        if !name[i + 4..].chars().all(|c| c.is_ascii_digit()) || i == 0 {
            continue;
        }
        if dir.join(&name[..i]).exists() {
            match std::fs::remove_file(dir.join(&rel)) {
                Ok(()) => log::info!("removed {name}"),
                Err(e) => log::warn!("remove {name}: {e}"),
            }
        }
    }
}

/// Starts the freshly installed chatty.exe and exits this process.
fn restart() -> Result<(), String> {
    let dir = crate::paths::exe_dir();
    std::process::Command::new(dir.join("chatty.exe"))
        .current_dir(&dir)
        .env(RESTART_ENV, env!("CARGO_PKG_VERSION"))
        .spawn()
        .map_err(|e| format!("start the new version: {e}"))?;
    log::info!("restarting into the new version");
    crate::tray::remove_icon();
    log::logger().flush();
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn versions_order_like_semver() {
        assert!(v("v0.2.0") > v("0.1.9"));
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("1.0.0") > v("1.0.0-beta.2"));
        assert!(v("1.0.0-beta.10") > v("1.0.0-beta.2"));
        assert!(v("1.0.0-beta") > v("1.0.0-alpha.1"));
        assert!(v("1.0.0-beta.1") > v("1.0.0-beta"));
        assert_eq!(v("v1.2.3"), v("1.2.3+build.5"));
        assert_eq!(v("1.2.3-rc.1").to_string(), "1.2.3-rc.1");
        for bad in ["", "1.2", "1.2.3.4", "1.x.3", "1.2.3-", "latest"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    fn release_json(tag: &str, pre: bool, digest: bool) -> serde_json::Value {
        let zip = format!("chatty-{}-windows-x64.zip", tag.trim_start_matches('v'));
        let mut asset = serde_json::json!({
            "name": zip, "size": 123,
            "browser_download_url": format!("https://example.test/{zip}"),
        });
        if digest {
            asset["digest"] = format!("sha256:{}", "AB".repeat(32)).into();
        }
        serde_json::json!({
            "tag_name": tag, "draft": false, "prerelease": pre,
            "html_url": format!("https://example.test/releases/{tag}"),
            "assets": [asset, {
                "name": format!("{zip}.sha256"), "size": 90,
                "browser_download_url": format!("https://example.test/{zip}.sha256"),
            }],
        })
    }

    #[test]
    fn releases_parse() {
        let r = parse_release(&release_json("v0.2.0", false, true), false).unwrap();
        assert_eq!(r.version, v("0.2.0"));
        assert_eq!(r.size, 123);
        assert_eq!(r.sha256, Checksum::Hex("ab".repeat(32)));
        let r = parse_release(&release_json("v0.2.0", false, false), false).unwrap();
        assert_eq!(r.sha256, Checksum::Url("https://example.test/chatty-0.2.0-windows-x64.zip.sha256".into()));
        assert!(parse_release(&release_json("v0.3.0-beta.1", true, true), false).is_none());
        assert!(parse_release(&release_json("v0.3.0-beta.1", true, true), true).is_some());
        let mut no_zip = release_json("v0.2.0", false, true);
        no_zip["assets"] = serde_json::json!([]);
        assert!(parse_release(&no_zip, false).is_none());
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("chatty-update-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn swap_moves_old_files_aside_and_cleans_up() {
        let (new, inst) = (temp("swap-new"), temp("swap-inst"));
        write(&new.join("chatty.exe"), "new exe");
        write(&new.join("llama/llama.dll"), "new dll");
        write(&new.join("added.txt"), "added");
        write(&inst.join("chatty.exe"), "old exe");
        write(&inst.join("llama/llama.dll"), "old dll");
        write(&inst.join("config.toml"), "kept");
        swap(&new, &inst).unwrap();
        assert_eq!(std::fs::read_to_string(inst.join("chatty.exe")).unwrap(), "new exe");
        assert_eq!(std::fs::read_to_string(inst.join("chatty.exe.old")).unwrap(), "old exe");
        assert_eq!(std::fs::read_to_string(inst.join("llama/llama.dll")).unwrap(), "new dll");
        assert_eq!(std::fs::read_to_string(inst.join("added.txt")).unwrap(), "added");
        assert_eq!(std::fs::read_to_string(inst.join("config.toml")).unwrap(), "kept");
        write(&inst.join("notes.old"), "not ours");
        remove_leftovers(&inst);
        assert!(!inst.join("chatty.exe.old").exists());
        assert!(!inst.join("llama/llama.dll.old").exists());
        assert!(inst.join("notes.old").exists());
        let _ = std::fs::remove_dir_all(&new);
        let _ = std::fs::remove_dir_all(&inst);
    }

    #[test]
    fn swap_rolls_back_on_error() {
        let (new, inst) = (temp("rb-new"), temp("rb-inst"));
        write(&new.join("a.txt"), "new a");
        write(&new.join("b/c.txt"), "new c");
        write(&inst.join("a.txt"), "old a");
        // A file where the new tree needs a directory: create_dir_all fails.
        write(&inst.join("b"), "a file");
        assert!(swap(&new, &inst).is_err());
        assert_eq!(std::fs::read_to_string(inst.join("a.txt")).unwrap(), "old a");
        assert!(!inst.join("a.txt.old").exists());
        assert_eq!(std::fs::read_to_string(inst.join("b")).unwrap(), "a file");
        let _ = std::fs::remove_dir_all(&new);
        let _ = std::fs::remove_dir_all(&inst);
    }
}
