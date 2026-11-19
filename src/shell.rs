//! Windows shell integration: single instance per profile (a named pipe forwards the
//! command line of a second launch to the running browser), installing the exe into
//! %LOCALAPPDATA% and registering it as a browser for the current user (HKCU only).
//!
//! Test overrides: `MBROWSER_REG_ROOT=Software\MBrowserTest` moves every registry write
//! under that HKCU key, `MBROWSER_INSTALL_DIR=<dir>` replaces the install folder. Either
//! one also switches off the Start-Menu / desktop shortcuts, the Settings page and message boxes.

use std::{
    ffi::OsString,
    io::Write,
    os::windows::{ffi::OsStringExt, process::CommandExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use tao::event_loop::EventLoopProxy;
use windows::{
    Win32::{
        Foundation::{HANDLE, HWND, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND, ReadFile},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                CoUninitialize, IPersistFile,
            },
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_TYPE_BYTE, PIPE_WAIT,
            },
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW,
                RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
            },
        },
        UI::{
            Shell::{FOLDERID_Desktop, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellExecuteW, ShellLink},
            WindowsAndMessaging::{
                AllowSetForegroundWindow, IsIconic, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW, SW_RESTORE,
                SW_SHOWNORMAL, SetForegroundWindow, ShowWindow,
            },
        },
    },
    core::{HSTRING, Interface, PCWSTR},
};

use crate::{store, tab::UserEvent};

/// Registry / Start-Menu name (also the value the Settings page is opened with).
const REG_NAME: &str = "MBrowser";
const PROG_URL: &str = "MBrowserURL";
const PROG_HTML: &str = "MBrowserHTML";
const PROG_PDF: &str = "MBrowserPDF";
const MAX_MSG: usize = 64 * 1024;
const ERROR_PIPE_BUSY: i32 = 231;
const ERROR_NO_DATA: i32 = 232;
const ERROR_PIPE_CONNECTED: i32 = 535;
/// How long a second launch waits for the running instance to take the request.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(15);

// ------------------------------------------------------------------ command line

/// First non-empty command-line argument, with file paths turned into `file://` URLs
/// (resolved against the current directory, which the running instance doesn't share).
pub fn cli_input() -> Option<String> {
    std::env::args().skip(1).find(|a| !a.trim().is_empty()).map(|a| normalize_arg(&a))
}

/// Windows hands files over as plain paths (`C:\x\a b.html`); the address bar logic
/// would take them for a search, so they become `file://` URLs here.
pub fn normalize_arg(arg: &str) -> String {
    let t = arg.trim().trim_matches('"').trim();
    let b = t.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    let unc = t.starts_with("\\\\") && !t.starts_with("\\\\?\\");
    let relative = !t.contains("://") && Path::new(t).is_file();
    if !(drive || unc || relative) {
        return t.to_string();
    }
    let full = if drive || unc { PathBuf::from(t) } else { std::path::absolute(t).unwrap_or_else(|_| PathBuf::from(t)) };
    let s = full.to_string_lossy().replace('\\', "/");
    let (host, path) = match s.strip_prefix("//") {
        Some(rest) => {
            let (h, p) = rest.split_once('/').unwrap_or((rest, ""));
            (h.to_string(), format!("/{p}"))
        }
        None => (String::new(), format!("/{s}")),
    };
    let mut out = format!("file://{host}");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// ------------------------------------------------------------------ single instance

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

/// One pipe per profile: a test profile (`MBROWSER_HOME`) never meets the real one.
fn pipe_name() -> String {
    let dir = store::data_dir().to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    format!(r"\\.\pipe\MBrowser-{:016x}", fnv(&dir))
}

/// Creates the server end; `FIRST_PIPE_INSTANCE` makes this an atomic "am I the only
/// instance" check that disappears with the process, so a crash leaves nothing stale.
fn create_server(name: &str) -> Result<HANDLE, i32> {
    let h = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(name),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0,
            MAX_MSG as u32,
            0,
            None,
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_invalid() {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    Ok(h)
}

/// Accepts one client at a time and passes what it wrote to `on_msg` (false = stop).
fn serve(pipe: isize, on_msg: impl Fn(String) -> bool) {
    let h = HANDLE(pipe as _);
    loop {
        let connected = match unsafe { ConnectNamedPipe(h, None) } {
            Ok(()) => true,
            // The client came in (and maybe already left) before Connect: its data is still buffered.
            Err(e) => matches!(e.code().0 & 0xFFFF, ERROR_PIPE_CONNECTED | ERROR_NO_DATA),
        };
        if connected {
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            while data.len() < MAX_MSG {
                let mut n = 0u32;
                // EOF (client closed) and errors both end the message.
                if unsafe { ReadFile(h, Some(&mut buf), Some(&mut n), None) }.is_err() || n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n as usize]);
            }
            let keep = on_msg(String::from_utf8_lossy(&data).trim().to_string());
            unsafe {
                let _ = DisconnectNamedPipe(h);
            }
            if !keep {
                return;
            }
        } else {
            unsafe {
                let _ = DisconnectNamedPipe(h);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Sends the request to the running instance; Err = it isn't (yet) reachable.
fn forward(name: &str, payload: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).open(name)?;
    f.write_all(payload.as_bytes())
}

/// Call first thing in `main`. True: this process is the browser for the profile.
/// False: the request was handed to the running instance (or could not be), exit now.
pub fn single_instance(proxy: EventLoopProxy<UserEvent>) -> bool {
    let name = pipe_name();
    let payload = cli_input().unwrap_or_default();
    let deadline = Instant::now() + FORWARD_TIMEOUT;
    loop {
        match create_server(&name) {
            Ok(h) => {
                let pipe = h.0 as isize;
                let spawned = std::thread::Builder::new().name("single-instance".into()).spawn(move || {
                    serve(pipe, |msg| proxy.send_event(UserEvent::External(msg)).is_ok());
                });
                if spawned.is_err() {
                    eprintln!("single instance: cannot start the pipe thread");
                }
                return true;
            }
            // Someone else owns the pipe (access denied / busy): hand over the request.
            Err(5) | Err(ERROR_PIPE_BUSY) => {
                // The user's click gives this process the right to focus the running window.
                unsafe {
                    let _ = AllowSetForegroundWindow(u32::MAX);
                }
                match forward(&name, &payload) {
                    Ok(()) => return false,
                    Err(e) if Instant::now() < deadline => {
                        let _ = e; // busy / still starting: retry
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        eprintln!("single instance: cannot reach the running browser: {e}");
                        return false;
                    }
                }
            }
            // Anything else: run without forwarding rather than refuse to start.
            Err(code) => {
                eprintln!("single instance: pipe error {code}, continuing without it");
                return true;
            }
        }
    }
}

/// Brings the window up (restores it if minimized). Not in automated UI checks.
pub fn raise(hwnd: HWND) {
    if cfg!(debug_assertions) && std::env::var_os("MBROWSER_BACKGROUND").is_some() {
        return;
    }
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

// ------------------------------------------------------------------ install / register

struct Cfg {
    /// HKCU subkey that plays the role of `Software`.
    reg_root: String,
    install_dir: PathBuf,
    /// Test overrides active: no shortcuts, no Settings page, no message boxes.
    test: bool,
}

impl Cfg {
    fn from_env() -> Self {
        let env = |k| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let (root, dir) = (env("MBROWSER_REG_ROOT"), env("MBROWSER_INSTALL_DIR"));
        Self {
            test: root.is_some() || dir.is_some(),
            reg_root: root.unwrap_or_else(|| "Software".into()).trim_matches('\\').to_string(),
            install_dir: dir.map(PathBuf::from).unwrap_or_else(|| {
                let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
                base.join("Programs").join("MBrowser")
            }),
        }
    }

    fn exe(&self) -> PathBuf {
        self.install_dir.join("mbrowser.exe")
    }

    fn client_key(&self) -> String {
        format!(r"{}\Clients\StartMenuInternet\{REG_NAME}", self.reg_root)
    }

    fn class_key(&self, prog: &str) -> String {
        format!(r"{}\Classes\{prog}", self.reg_root)
    }
}

/// Sets a REG_SZ value (`name` empty = the key's default value), creating the key.
fn reg_set(key: &str, name: &str, value: &str) -> Result<(), String> {
    unsafe {
        let mut h = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut h,
            None,
        )
        .ok()
        .map_err(|e| format!("{key}: {e}"))?;
        let wide: Vec<u8> = value.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
        let res = RegSetValueExW(h, &HSTRING::from(name), None, REG_SZ, Some(&wide));
        let _ = RegCloseKey(h);
        res.ok().map_err(|e| format!("{key}: {e}"))
    }
}

/// Registers the browser under HKCU: Clients\StartMenuInternet + Capabilities, the
/// RegisteredApplications entry and the ProgIDs. Windows still makes the user choose.
fn register(cfg: &Cfg, exe: &Path) -> Result<(), String> {
    let exe_s = exe.to_string_lossy().to_string();
    let icon = format!("\"{exe_s}\",0");
    let open_url = format!("\"{exe_s}\" \"%1\"");
    let name = crate::APP_NAME;
    let desc = "Быстрый лёгкий браузер на движке WebView2";
    let client = cfg.client_key();
    let caps = format!(r"{client}\Capabilities");

    reg_set(&client, "", name)?;
    reg_set(&format!(r"{client}\DefaultIcon"), "", &icon)?;
    reg_set(&format!(r"{client}\shell\open\command"), "", &format!("\"{exe_s}\""))?;
    reg_set(&caps, "ApplicationName", name)?;
    reg_set(&caps, "ApplicationDescription", desc)?;
    reg_set(&caps, "ApplicationIcon", &format!("{exe_s},0"))?;
    reg_set(&format!(r"{caps}\Startmenu"), "StartMenuInternet", REG_NAME)?;
    for scheme in ["http", "https"] {
        reg_set(&format!(r"{caps}\URLAssociations"), scheme, PROG_URL)?;
    }
    for (ext, prog) in [(".htm", PROG_HTML), (".html", PROG_HTML), (".xhtml", PROG_HTML), (".pdf", PROG_PDF)] {
        reg_set(&format!(r"{caps}\FileAssociations"), ext, prog)?;
    }
    reg_set(&format!(r"{}\RegisteredApplications", cfg.reg_root), REG_NAME, &caps)?;

    for (prog, title, is_url) in [
        (PROG_URL, "Meteor Browser URL", true),
        (PROG_HTML, "Meteor Browser HTML Document", false),
        (PROG_PDF, "Meteor Browser PDF Document", false),
    ] {
        let key = cfg.class_key(prog);
        reg_set(&key, "", title)?;
        if is_url {
            reg_set(&key, "URL Protocol", "")?;
        }
        reg_set(&format!(r"{key}\DefaultIcon"), "", &icon)?;
        reg_set(&format!(r"{key}\shell\open\command"), "", &open_url)?;
    }
    Ok(())
}

/// Removes everything `register` wrote (a missing key is fine). The user's own
/// "default app" choice is kept by Windows; it simply stops pointing at a live app.
fn unregister(cfg: &Cfg) {
    if cfg.reg_root.is_empty() {
        return;
    }
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(cfg.client_key()));
        for prog in [PROG_URL, PROG_HTML, PROG_PDF] {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(cfg.class_key(prog)));
        }
        let mut h = HKEY::default();
        let apps = HSTRING::from(format!(r"{}\RegisteredApplications", cfg.reg_root));
        if RegOpenKeyExW(HKEY_CURRENT_USER, &apps, None, KEY_WRITE, &mut h).is_ok() {
            let _ = RegDeleteValueW(h, &HSTRING::from(REG_NAME));
            let _ = RegCloseKey(h);
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()),
        _ => false,
    }
}

/// Copies `src` to `<dir>\mbrowser.exe`. A running target can't be overwritten but can
/// be renamed, so it is moved aside (`*.old`, removed now or on the next install).
fn install_exe(src: &Path, dir: &Path) -> Result<PathBuf, String> {
    let dst = dir.join("mbrowser.exe");
    if same_file(src, &dst) {
        return Ok(dst);
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.path().extension().is_some_and(|x| x == "old") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let tmp = dir.join("mbrowser.exe.new");
    std::fs::copy(src, &tmp).map_err(|e| format!("копирование: {e}"))?;
    if std::fs::rename(&tmp, &dst).is_err() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let old = dir.join(format!("mbrowser.exe.{stamp}.old"));
        let moved = std::fs::rename(&dst, &old).and_then(|()| std::fs::rename(&tmp, &dst));
        if let Err(e) = moved {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("{}: {e}", dst.display()));
        }
        let _ = std::fs::remove_file(&old);
    }
    Ok(dst)
}

/// The user's desktop folder. Asked from the shell because it may be redirected
/// (OneDrive, another drive), so `%USERPROFILE%\Desktop` is not reliable.
fn desktop_dir() -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None).ok()?;
        let dir = PathBuf::from(OsString::from_wide(p.as_wide()));
        CoTaskMemFree(Some(p.0 as *const _));
        Some(dir)
    }
}

/// `MBrowser.lnk` in the Start Menu and on the desktop.
fn shortcut_paths() -> Vec<PathBuf> {
    let start_menu =
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs"));
    let name = format!("{REG_NAME}.lnk");
    [start_menu, desktop_dir()].into_iter().flatten().map(|dir| dir.join(&name)).collect()
}

fn create_shortcuts(exe: &Path) -> Result<(), String> {
    let paths = shortcut_paths();
    if paths.is_empty() {
        return Err("нет папок меню «Пуск» и рабочего стола".into());
    }
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let res = (|| -> windows::core::Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(exe.as_os_str()))?;
            link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0)?;
            link.SetDescription(&HSTRING::from(crate::APP_NAME))?;
            if let Some(dir) = exe.parent() {
                link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
            }
            let file = link.cast::<IPersistFile>()?;
            for lnk in &paths {
                file.Save(&HSTRING::from(lnk.as_os_str()), true)?;
            }
            Ok(())
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        res.map_err(|e| format!("ярлык: {e}"))
    }
}

/// Install + register. Returns the installed exe.
fn install(cfg: &Cfg) -> Result<PathBuf, String> {
    let src = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = install_exe(&src, &cfg.install_dir)?;
    register(cfg, &exe)?;
    if !cfg.test {
        create_shortcuts(&exe)?;
    }
    Ok(exe)
}

/// Unregister, drop the shortcuts and the installed exe (user data stays). If the exe is
/// the running one it is deleted by a detached helper after this process exits.
fn uninstall(cfg: &Cfg) {
    unregister(cfg);
    if !cfg.test {
        for lnk in shortcut_paths() {
            let _ = std::fs::remove_file(lnk);
        }
    }
    let exe = cfg.exe();
    if !exe.exists() {
        return;
    }
    if std::env::current_exe().is_ok_and(|me| same_file(&me, &exe)) {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = format!("ping -n 4 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"", exe.display(), cfg.install_dir.display());
        let _ = std::process::Command::new("cmd").args(["/c", &script]).creation_flags(CREATE_NO_WINDOW).spawn();
    } else {
        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::remove_dir(&cfg.install_dir);
    }
}

fn message(cfg: &Cfg, text: &str, error: bool) {
    if cfg.test {
        eprintln!("{text}");
        return;
    }
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(crate::APP_NAME), MB_OK | icon);
    }
}

/// Menu action: install, register, then open the Windows "Default apps" page.
pub fn install_async() {
    let _ = std::thread::Builder::new().name("shell-install".into()).spawn(|| {
        let cfg = Cfg::from_env();
        match install(&cfg) {
            Ok(exe) => {
                message(
                    &cfg,
                    &format!(
                        "Браузер установлен:\n{}\n\nСейчас откроются настройки Windows: выберите там {} \
                         для HTTP/HTTPS (Windows не разрешает сделать это автоматически).",
                        exe.display(),
                        crate::APP_NAME
                    ),
                    false,
                );
                if !cfg.test {
                    open_default_apps();
                }
            }
            Err(e) => message(&cfg, &format!("Не удалось установить браузер: {e}"), true),
        }
    });
}

/// Menu action: undo `install_async` (registration, shortcuts, installed copy).
pub fn uninstall_async() {
    let _ = std::thread::Builder::new().name("shell-uninstall".into()).spawn(|| {
        let cfg = Cfg::from_env();
        uninstall(&cfg);
        message(&cfg, "Регистрация и установленная копия браузера удалены. Ваши данные не тронуты.", false);
    });
}

fn open_default_apps() {
    let uri = format!("ms-settings:defaultapps?registeredAppUser={REG_NAME}");
    unsafe {
        ShellExecuteW(None, windows::core::w!("open"), &HSTRING::from(uri), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_SZ};

    fn get(key: &str, name: &str) -> Option<String> {
        let mut buf = [0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(key),
                &HSTRING::from(name),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        };
        r.is_ok().then(|| String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
    }

    fn key_exists(key: &str) -> bool {
        let mut h = HKEY::default();
        let ok = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(key), None, KEY_WRITE, &mut h) }.is_ok();
        if ok {
            unsafe {
                let _ = RegCloseKey(h);
            }
        }
        ok
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mbrowser-shell-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn file_paths_become_urls() {
        assert_eq!(normalize_arg(r"C:\x\a.html"), "file:///C:/x/a.html");
        assert_eq!(normalize_arg(r#""C:\my dir\a b#1.html""#), "file:///C:/my%20dir/a%20b%231.html");
        assert_eq!(normalize_arg(r"\\srv\share\f.pdf"), "file://srv/share/f.pdf");
        assert_eq!(normalize_arg("C:/Users/x/Привет.html"), "file:///C:/Users/x/%D0%9F%D1%80%D0%B8%D0%B2%D0%B5%D1%82.html");
        assert_eq!(normalize_arg("file:///C:/x/a.html"), "file:///C:/x/a.html");
        assert_eq!(normalize_arg("https://example.com/a?b=c"), "https://example.com/a?b=c");
        assert_eq!(normalize_arg("localhost:8080"), "localhost:8080");
        assert_eq!(normalize_arg("  rust lang  "), "rust lang");
        assert_eq!(normalize_arg(""), "");
    }

    #[test]
    fn relative_existing_file() {
        let d = scratch("rel");
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("page one.html");
        std::fs::write(&f, "x").unwrap();
        let url = normalize_arg(f.to_str().unwrap());
        assert!(url.starts_with("file:///") && url.ends_with("page%20one.html"), "{url}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pipe_name_depends_on_profile() {
        assert_ne!(fnv("c:\\a"), fnv("c:\\b"));
        assert!(pipe_name().starts_with(r"\\.\pipe\MBrowser-"));
    }

    #[test]
    fn pipe_forwarding_and_recovery() {
        let name = format!(r"\\.\pipe\MBrowser-test-{}", std::process::id());
        let h = create_server(&name).expect("first instance");
        assert!(matches!(create_server(&name), Err(5) | Err(ERROR_PIPE_BUSY)), "second instance must be refused");
        let (tx, rx) = std::sync::mpsc::channel();
        let pipe = h.0 as isize;
        std::thread::spawn(move || serve(pipe, |m| tx.send(m).is_ok()));
        let payloads = ["", "https://example.com/?a=1&b=\"2\"", r"file:///C:/x/a%20b.html", "кириллица query"];
        for p in payloads {
            let deadline = Instant::now() + Duration::from_secs(5);
            while forward(&name, p).is_err() {
                assert!(Instant::now() < deadline, "cannot forward");
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), p.trim());
        }
        // Many clients in a row, some while the server is busy.
        let writers: Vec<_> = (0..8)
            .map(|i| {
                let name = name.clone();
                std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while forward(&name, &format!("m{i}")).is_err() {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        let mut got: Vec<String> = (0..8).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
        got.sort();
        assert_eq!(got, (0..8).map(|i| format!("m{i}")).collect::<Vec<_>>());
    }

    #[test]
    fn register_unregister_roundtrip() {
        let root = format!(r"Software\MBrowserTest\reg-{}", std::process::id());
        let dir = scratch("reg");
        let cfg = Cfg { reg_root: root.clone(), install_dir: dir.clone(), test: true };
        let exe = Path::new(r"C:\Program Files\M B\mbrowser.exe");
        register(&cfg, exe).unwrap();
        register(&cfg, exe).unwrap(); // idempotent

        let client = cfg.client_key();
        let caps = format!(r"{client}\Capabilities");
        assert_eq!(get(&caps, "ApplicationName").as_deref(), Some(crate::APP_NAME));
        assert!(get(&caps, "ApplicationDescription").is_some());
        assert_eq!(get(&format!(r"{caps}\URLAssociations"), "http").as_deref(), Some(PROG_URL));
        assert_eq!(get(&format!(r"{caps}\URLAssociations"), "https").as_deref(), Some(PROG_URL));
        for ext in [".htm", ".html", ".xhtml"] {
            assert_eq!(get(&format!(r"{caps}\FileAssociations"), ext).as_deref(), Some(PROG_HTML));
        }
        assert_eq!(get(&format!(r"{caps}\FileAssociations"), ".pdf").as_deref(), Some(PROG_PDF));
        assert_eq!(get(&format!(r"{caps}\Startmenu"), "StartMenuInternet").as_deref(), Some(REG_NAME));
        assert_eq!(get(&format!(r"{client}\DefaultIcon"), "").as_deref(), Some(r#""C:\Program Files\M B\mbrowser.exe",0"#));
        assert_eq!(
            get(&format!(r"{client}\shell\open\command"), "").as_deref(),
            Some(r#""C:\Program Files\M B\mbrowser.exe""#)
        );
        assert_eq!(get(&format!(r"{root}\RegisteredApplications"), REG_NAME).as_deref(), Some(caps.as_str()));
        let cmd = r#""C:\Program Files\M B\mbrowser.exe" "%1""#;
        for prog in [PROG_URL, PROG_HTML, PROG_PDF] {
            assert_eq!(get(&format!(r"{}\shell\open\command", cfg.class_key(prog)), "").as_deref(), Some(cmd));
        }
        assert_eq!(get(&cfg.class_key(PROG_URL), "URL Protocol").as_deref(), Some(""));
        assert!(get(&cfg.class_key(PROG_HTML), "URL Protocol").is_none());

        unregister(&cfg);
        assert!(!key_exists(&client));
        for prog in [PROG_URL, PROG_HTML, PROG_PDF] {
            assert!(!key_exists(&cfg.class_key(prog)));
        }
        assert!(get(&format!(r"{root}\RegisteredApplications"), REG_NAME).is_none());
        unregister(&cfg); // nothing left: still fine

        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(root.as_str()));
        }
        assert!(!key_exists(&root));
    }

    #[test]
    fn install_copy_and_in_use_target() {
        let dir = scratch("inst");
        let src = std::env::current_exe().unwrap();
        // Fresh install.
        let exe = install_exe(&src, &dir).unwrap();
        assert!(exe.is_file());
        assert_eq!(std::fs::metadata(&exe).unwrap().len(), std::fs::metadata(&src).unwrap().len());
        // Running from the target: skipped.
        assert_eq!(install_exe(&exe, &dir).unwrap(), exe);
        assert!(!dir.join("mbrowser.exe.new").exists());
        // Target in use: replace the exe by a running ping.exe, then install over it.
        let ping = Path::new(r"C:\Windows\System32\PING.EXE");
        std::fs::remove_file(&exe).unwrap();
        std::fs::copy(ping, &exe).unwrap();
        let mut child = std::process::Command::new(&exe).args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let res = install_exe(&src, &dir);
        let _ = child.kill();
        let _ = child.wait();
        let exe = res.expect("install over a running exe");
        assert_eq!(std::fs::metadata(&exe).unwrap().len(), std::fs::metadata(&src).unwrap().len());
        // The stale .old is cleaned on the next install.
        install_exe(&src, &dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_removes_copy() {
        let root = format!(r"Software\MBrowserTest\un-{}", std::process::id());
        let dir = scratch("un");
        let cfg = Cfg { reg_root: root.clone(), install_dir: dir.clone(), test: true };
        let exe = install_exe(&std::env::current_exe().unwrap(), &dir).unwrap();
        register(&cfg, &exe).unwrap();
        uninstall(&cfg);
        assert!(!exe.exists() && !dir.exists());
        assert!(!key_exists(&cfg.client_key()));
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(root.as_str()));
        }
    }
}
