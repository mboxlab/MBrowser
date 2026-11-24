//! Extensions the browser installs by itself and keeps up to date.
//!
//! VOT (Voice Over Translation: Yandex's voice-over for foreign videos) comes
//! from its GitHub releases. Yandex changes its side every few weeks and old
//! versions stop working, so the check runs every few hours in the background.
//! An extension the user removed in the "Расширения" panel is not brought back.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{
    ext, net, store,
    tab::{Proxy, UserEvent},
};

struct Def {
    /// Our key in the state file.
    key: &'static str,
    /// Folder created in `extensions` (an existing one is found by manifest name).
    folder: &'static str,
    /// `name` in its manifest.
    manifest_name: &'static str,
    /// GitHub "latest release" API and the Chromium build's download URL.
    api: &'static str,
    zip: &'static str,
}

const DEFAULTS: &[Def] = &[Def {
    key: "vot",
    folder: "VOT - Voice Over Translation",
    manifest_name: "[VOT] - Voice Over Translation",
    api: "https://api.github.com/repos/ilyhalight/voice-over-translation/releases/latest",
    zip: "https://github.com/ilyhalight/voice-over-translation/releases/download/{tag}/vot-extension-chrome.zip",
}];

const CHECK_EVERY_SECS: u64 = 6 * 3600;

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct State {
    /// Installed once: a missing folder now means the user removed it.
    offered: Vec<String>,
    /// Last successful check (unix seconds).
    checked: u64,
}

impl Default for State {
    fn default() -> Self {
        Self { offered: Vec::new(), checked: 0 }
    }
}

fn state_path() -> PathBuf {
    store::data_dir().join("default-extensions.json")
}

fn read_state() -> State {
    std::fs::read(state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

static BUSY: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Called from the periodic tick: starts a background check when one is due.
/// Each installed or updated extension arrives as `UserEvent::ExtInstalled`.
pub fn maybe_check(proxy: &Proxy) {
    let now = store::now();
    if now < NEXT.load(Ordering::Relaxed) || BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let p = proxy.clone();
    std::thread::spawn(move || {
        let mut st = read_state();
        let next = if now.saturating_sub(st.checked) < CHECK_EVERY_SECS {
            st.checked + CHECK_EVERY_SECS
        } else {
            let ok = DEFAULTS.iter().all(|d| match check_one(d, &mut st) {
                Ok(Some(path)) => {
                    let _ = p.send_event(UserEvent::ExtInstalled(path));
                    true
                }
                Ok(None) => true,
                Err(e) => {
                    eprintln!("default extension {}: {e}", d.key);
                    false
                }
            });
            if ok {
                st.checked = now;
            }
            if let Ok(b) = serde_json::to_vec(&st) {
                let _ = std::fs::write(state_path(), b);
            }
            // A failed check (offline) is retried in an hour.
            if ok { now + CHECK_EVERY_SECS } else { now + 3600 }
        };
        NEXT.store(next, Ordering::Relaxed);
        BUSY.store(false, Ordering::Release);
    });
}

/// Installs or updates one extension; returns its folder if it changed.
fn check_one(d: &Def, st: &mut State) -> Result<Option<PathBuf>, String> {
    let installed = find_folder(d.manifest_name);
    let offered = st.offered.iter().any(|k| k == d.key);
    if installed.is_none() && offered {
        return Ok(None); // removed by the user
    }
    if !offered {
        st.offered.push(d.key.to_string());
    }
    let r = net::request("GET", d.api, &[("Accept", "application/vnd.github+json")], None)?;
    if !r.ok() {
        return Err(format!("GitHub API: HTTP {}", r.status));
    }
    let v: serde_json::Value = serde_json::from_slice(&r.body).map_err(|e| e.to_string())?;
    let tag = v["tag_name"].as_str().ok_or("no tag_name")?;
    if let Some((_, have)) = &installed {
        if !newer(tag.trim_start_matches('v'), have) {
            return Ok(None);
        }
    }
    let target = installed.map(|(p, _)| p).unwrap_or_else(|| ext::dir().join(d.folder));
    install(&d.zip.replace("{tag}", tag), &target, d.manifest_name)?;
    Ok(Some(target))
}

/// The folder in `extensions` holding this extension, and its version.
fn find_folder(manifest_name: &str) -> Option<(PathBuf, String)> {
    std::fs::read_dir(ext::dir()).ok()?.flatten().find_map(|e| {
        let text = std::fs::read_to_string(e.path().join("manifest.json")).ok()?;
        let m: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
        (m["name"].as_str() == Some(manifest_name)).then(|| (e.path(), m["version"].as_str().unwrap_or("0").to_string()))
    })
}

/// "1.12.10" > "1.12.9"
pub(crate) fn newer(latest: &str, have: &str) -> bool {
    let parts = |s: &str| s.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(latest) > parts(have)
}

/// Downloads the zip and swaps it in place of `target` (same path: the engine
/// treats it as an update and keeps the extension's data).
fn install(url: &str, target: &Path, manifest_name: &str) -> Result<(), String> {
    let r = net::get(url)?;
    if !r.ok() || !r.body.starts_with(b"PK") {
        return Err(format!("download: HTTP {}", r.status));
    }
    // Staging lives inside `extensions` (same volume for the final rename) but one
    // level deeper, where it is never mistaken for an extension.
    let staging = ext::dir().join(".staging");
    let fresh = staging.join("new");
    let old = staging.join("old");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&fresh).map_err(|e| e.to_string())?;
    let zip = staging.join("ext.zip");
    std::fs::write(&zip, &r.body).map_err(|e| e.to_string())?;
    unzip(&zip, &fresh)?;
    let ok = std::fs::read_to_string(fresh.join("manifest.json")).is_ok_and(|t| t.contains(manifest_name));
    if !ok {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("unexpected archive contents".into());
    }
    if target.exists() {
        std::fs::rename(target, &old).map_err(|e| format!("replace: {e}"))?;
    }
    if let Err(e) = std::fs::rename(&fresh, target) {
        let _ = std::fs::rename(&old, target); // put the working version back
        return Err(format!("install: {e}"));
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
}

/// Windows 10/11 ship bsdtar, which reads zip archives.
fn unzip(zip: &Path, to: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let status = std::process::Command::new(root.join("System32").join("tar.exe"))
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(to)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("tar: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("tar: {status}")) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn compares_versions() {
        assert!(super::newer("1.12.10", "1.12.9"));
        assert!(super::newer("2.0", "1.99.99"));
        assert!(!super::newer("1.12.8", "1.12.8"));
        assert!(!super::newer("1.12.7", "1.12.8"));
    }
}
