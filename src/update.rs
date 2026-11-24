//! Self-update from the project's GitHub releases.
//!
//! Every few hours a background thread asks GitHub for the latest release. A newer
//! version's `mbrowser.exe` is downloaded, checked against the SHA-256 GitHub lists
//! for it, and swapped in place of the running exe: Windows lets a running exe be
//! renamed (to `*.old`), so the new file takes its name and runs from the next
//! start. The menu offers to restart right away. Old copies are removed at startup.
//!
//! Off in debug builds (unless `MBROWSER_UPDATE_API` points to a test server), with
//! `MBROWSER_NO_UPDATE` set, or when the user switches it off in the settings.

use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crate::{
    defext::newer,
    net, store,
    tab::{Proxy, UserEvent},
};

#[cfg_attr(debug_assertions, allow(dead_code))]
const API: &str = "https://api.github.com/repos/mboxlab/MBrowser/releases/latest";
/// The release asset with the bare browser (the installer is `MBrowser-Setup.exe`).
const ASSET: &str = "mbrowser.exe";
const CHECK_EVERY_SECS: u64 = 6 * 3600;
/// A failed check (offline, GitHub's rate limit) is retried sooner.
const RETRY_SECS: u64 = 3600;
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Inno Setup's uninstall entry (installer/mbrowser.iss, AppId): its version is kept current.
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{17401014-DB4C-45D9-923E-64F2D04FAA62}_is1";

#[derive(Debug)]
pub enum Outcome {
    /// This version is in place and runs from the next start.
    Ready(String),
    UpToDate,
    Failed(String),
}

static BUSY: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicU64 = AtomicU64::new(0);
/// The version already swapped in (not downloaded again) and the exe it went to.
static STAGED: Mutex<Option<(String, PathBuf)>> = Mutex::new(None);

/// The releases API: the real one, or a test server in debug builds.
fn api() -> Option<String> {
    #[cfg(debug_assertions)]
    {
        std::env::var("MBROWSER_UPDATE_API").ok()
    }
    #[cfg(not(debug_assertions))]
    {
        Some(API.to_string())
    }
}

fn allowed() -> bool {
    std::env::var_os("MBROWSER_NO_UPDATE").is_none() && api().is_some()
}

/// The exe the update replaced and should be started now, if any.
pub fn staged() -> Option<(String, PathBuf)> {
    STAGED.lock().ok()?.clone()
}

/// Periodic (tick): a background check when one is due. The result arrives as
/// `UserEvent::Update(outcome, false)`.
pub fn maybe_check(proxy: &Proxy, on: bool) {
    if !on || !allowed() || store::now() < NEXT.load(Ordering::Relaxed) {
        return;
    }
    start(proxy, false);
}

/// "Проверить сейчас" in the settings: answers with `UserEvent::Update(outcome, true)`.
pub fn check_now(proxy: &Proxy) {
    if !allowed() {
        let _ = proxy.send_event(UserEvent::Update(Outcome::Failed("обновления выключены в этой сборке".into()), true));
        return;
    }
    start(proxy, true);
}

fn start(proxy: &Proxy, manual: bool) {
    if BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    if *crate::tab::DEBUG {
        eprintln!("update: checking {:?}", api());
    }
    let p = proxy.clone();
    std::thread::spawn(move || {
        let outcome = match check() {
            Ok(Some(v)) => Outcome::Ready(v),
            Ok(None) => Outcome::UpToDate,
            Err(e) => Outcome::Failed(e),
        };
        let wait = if matches!(outcome, Outcome::Failed(_)) { RETRY_SECS } else { CHECK_EVERY_SECS };
        NEXT.store(store::now() + wait, Ordering::Relaxed);
        BUSY.store(false, Ordering::Release);
        let _ = p.send_event(UserEvent::Update(outcome, manual));
    });
}

/// One check; `Some(version)` once a newer version is in place.
fn check() -> Result<Option<String>, String> {
    let api = api().ok_or("updates are off")?;
    let r = net::request("GET", &api, &[("Accept", "application/vnd.github+json")], None)?;
    if !r.ok() {
        return Err(format!("GitHub: HTTP {}", r.status));
    }
    let v: serde_json::Value = serde_json::from_slice(&r.body).map_err(|e| e.to_string())?;
    let latest = v["tag_name"].as_str().ok_or("no tag_name")?.trim_start_matches('v').to_string();
    if !newer(&latest, CURRENT) {
        return Ok(None);
    }
    if staged().is_some_and(|(s, _)| s == latest) {
        return Ok(Some(latest));
    }
    let asset = v["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(ASSET)))
        .ok_or("the release has no mbrowser.exe")?;
    let url = asset["browser_download_url"].as_str().ok_or("no download url")?;
    let size = asset["size"].as_u64().unwrap_or(0);
    let digest = asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase);

    let r = net::get(url)?;
    if !r.ok() {
        return Err(format!("download: HTTP {}", r.status));
    }
    verify(&r.body, size, digest.as_deref())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    swap_in(&exe, &r.body)?;
    set_uninstall_version(&exe, &latest);
    if let Ok(mut s) = STAGED.lock() {
        *s = Some((latest.clone(), exe));
    }
    Ok(Some(latest))
}

/// The download is the whole file GitHub describes, and a Windows program.
fn verify(body: &[u8], size: u64, sha256: Option<&str>) -> Result<(), String> {
    if size != 0 && body.len() as u64 != size {
        return Err(format!("download: {} of {size} bytes", body.len()));
    }
    if !body.starts_with(b"MZ") {
        return Err("download: not an exe".into());
    }
    if let Some(want) = sha256 {
        let got = sha256_hex(body).ok_or("SHA-256 failed")?;
        if got != want {
            return Err("download: SHA-256 does not match the release".into());
        }
    }
    Ok(())
}

pub fn sha256_hex(data: &[u8]) -> Option<String> {
    use windows::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    status.is_ok().then(|| out.iter().map(|b| format!("{b:02x}")).collect())
}

/// Puts `bytes` in place of `exe`, which may be running: it is renamed aside first.
fn swap_in(exe: &Path, bytes: &[u8]) -> Result<(), String> {
    let name = exe.file_name().ok_or("no exe name")?.to_string_lossy().into_owned();
    let fresh = exe.with_file_name(format!("{name}.update"));
    std::fs::write(&fresh, bytes).map_err(|e| format!("{}: {e}", fresh.display()))?;
    let old = exe.with_file_name(format!("{name}.{}.old", store::now()));
    if let Err(e) = std::fs::rename(exe, &old) {
        let _ = std::fs::remove_file(&fresh);
        return Err(format!("{}: {e}", exe.display()));
    }
    if let Err(e) = std::fs::rename(&fresh, exe) {
        let _ = std::fs::rename(&old, exe); // keep the working version
        let _ = std::fs::remove_file(&fresh);
        return Err(format!("{}: {e}", exe.display()));
    }
    Ok(())
}

/// Startup: copies replaced by earlier updates (or installs) are no longer running.
pub fn cleanup() {
    let Ok(exe) = std::env::current_exe() else { return };
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else { return };
    let name = name.to_string_lossy().to_ascii_lowercase();
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let f = e.file_name().to_string_lossy().to_ascii_lowercase();
        let ours = f.strip_prefix(&name).is_some_and(|rest| rest == ".update" || (rest.starts_with('.') && rest.ends_with(".old")));
        if ours {
            let _ = std::fs::remove_file(e.path()); // fails harmlessly while still in use
        }
    }
}

/// "Установка и удаление программ" shows the installed version: keep it current
/// when the exe is the one the installer put there.
fn set_uninstall_version(exe: &Path, version: &str) {
    use windows::{
        Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, REG_VALUE_TYPE, RegCloseKey,
            RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
        },
        core::HSTRING,
    };
    unsafe {
        let mut h = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY), None, KEY_QUERY_VALUE | KEY_SET_VALUE, &mut h)
            .is_err()
        {
            return;
        }
        // Only the installer's copy: InstallLocation must be this exe's folder.
        let mut buf = vec![0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let mut kind = REG_VALUE_TYPE::default();
        let read = RegQueryValueExW(
            h,
            &HSTRING::from("InstallLocation"),
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        );
        let location = String::from_utf16_lossy(&buf[..(len as usize / 2)]).trim_end_matches('\0').to_string();
        let same = read.is_ok()
            && exe.parent().is_some_and(|d| {
                d.to_string_lossy().trim_end_matches('\\').eq_ignore_ascii_case(location.trim_end_matches('\\'))
            });
        if same {
            let wide: Vec<u8> = version.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
            let _ = RegSetValueExW(h, &HSTRING::from("DisplayVersion"), None, REG_SZ, Some(&wide));
        }
        let _ = RegCloseKey(h);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(super::sha256_hex(b"").unwrap(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(super::sha256_hex(b"abc").unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn verify_rejects_bad_downloads() {
        let exe = b"MZ\x90\0rest of the program".to_vec();
        let sum = super::sha256_hex(&exe).unwrap();
        assert!(super::verify(&exe, exe.len() as u64, Some(&sum)).is_ok());
        assert!(super::verify(&exe, exe.len() as u64 + 1, Some(&sum)).is_err()); // truncated
        assert!(super::verify(&exe, 0, Some("00")).is_err()); // wrong hash
        assert!(super::verify(b"<html>rate limited</html>", 0, None).is_err()); // not an exe
    }

    #[test]
    fn swaps_a_file_in_place_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("mbrowser-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("mbrowser.exe");
        std::fs::write(&exe, b"MZ old").unwrap();
        super::swap_in(&exe, b"MZ new").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ new");
        let olds: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(olds.iter().any(|n| n.starts_with("mbrowser.exe.") && n.ends_with(".old")), "{olds:?}");
        assert!(!dir.join("mbrowser.exe.update").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
