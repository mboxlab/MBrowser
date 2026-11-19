//! Browser extensions: unpacked Chrome extensions, one subfolder each in the
//! `extensions` folder, installed into the engine's profile; plus what the
//! "Расширения" panel needs (list, on/off, settings page, removal).

use std::{
    cell::RefCell,
    collections::HashSet,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    rc::Rc,
};

use serde_json::{Value, json};
use webview2_com::{
    BrowserExtensionEnableCompletedHandler, BrowserExtensionRemoveCompletedHandler,
    Microsoft::Web::WebView2::Win32::*, ProfileAddBrowserExtensionCompletedHandler,
    ProfileGetBrowserExtensionsCompletedHandler, take_pwstr,
};
use windows::{
    Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW, SHFileOperationW,
    },
    core::{BOOL, HSTRING, Interface, PCWSTR, PWSTR},
};
use wry::{WebView, WebViewExtWindows};

use crate::{
    iconcache, store,
    tab::{Proxy, UserEvent},
};

/// Installed extensions as last reported by the engine (UI thread only).
pub type List = Rc<RefCell<Vec<ICoreWebView2BrowserExtension>>>;

pub fn dir() -> PathBuf {
    store::data_dir().join("extensions")
}

fn profile(view: &WebView) -> Option<ICoreWebView2Profile7> {
    unsafe {
        view.webview()
            .cast::<ICoreWebView2_13>()
            .and_then(|c| c.Profile())
            .and_then(|p| p.cast::<ICoreWebView2Profile7>())
            .ok()
    }
}

/// Installs the folders that are not installed yet, then fills `list`.
///
/// The engine keeps installed extensions (and their data) in the profile, so a
/// folder is added only once: adding it again counts as an *update* for the
/// extension — Tampermonkey, for one, then opens its "what's new" tab.
pub fn load(view: &WebView, list: &List, proxy: &Proxy) {
    let Some(profile) = profile(view) else { return };
    let folders = folders();
    let (p, l, px) = (profile.clone(), list.clone(), proxy.clone());
    query(&profile, move |installed| {
        let names: HashSet<String> = installed.iter().map(|e| name(e).to_lowercase()).collect();
        for f in &folders {
            if f.names.iter().any(|n| names.contains(&n.to_lowercase())) {
                continue;
            }
            let (p2, l2, px2) = (p.clone(), l.clone(), px.clone());
            let done = ProfileAddBrowserExtensionCompletedHandler::create(Box::new(move |_, _| {
                refresh_with(&p2, &l2, &px2);
                Ok(())
            }));
            unsafe {
                let _ = p.AddBrowserExtension(&HSTRING::from(f.path.as_path()), &done);
            }
        }
        *l.borrow_mut() = installed;
        let _ = px.send_event(UserEvent::Extensions);
    });
}

/// Installs one folder, or updates the extension if it is already installed
/// (default extensions after a new release); `UserEvent::Extensions` follows.
pub fn add(view: &WebView, path: &Path, list: &List, proxy: &Proxy) {
    let Some(p) = profile(view) else { return };
    let (p2, l, px) = (p.clone(), list.clone(), proxy.clone());
    let done = ProfileAddBrowserExtensionCompletedHandler::create(Box::new(move |_, _| {
        refresh_with(&p2, &l, &px);
        Ok(())
    }));
    unsafe {
        let _ = p.AddBrowserExtension(&HSTRING::from(path), &done);
    }
}

/// Re-reads the installed list; `UserEvent::Extensions` follows.
pub fn refresh(view: &WebView, list: &List, proxy: &Proxy) {
    if let Some(p) = profile(view) {
        refresh_with(&p, list, proxy);
    }
}

fn refresh_with(profile: &ICoreWebView2Profile7, list: &List, proxy: &Proxy) {
    let (l, px) = (list.clone(), proxy.clone());
    query(profile, move |v| {
        *l.borrow_mut() = v;
        let _ = px.send_event(UserEvent::Extensions);
    });
}

fn query(profile: &ICoreWebView2Profile7, then: impl FnOnce(Vec<ICoreWebView2BrowserExtension>) + 'static) {
    let handler = ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |result, list| {
        let mut out = Vec::new();
        if let (Ok(()), Some(list)) = (result, list) {
            let mut n = 0u32;
            unsafe {
                list.Count(&mut n)?;
                for i in 0..n {
                    out.push(list.GetValueAtIndex(i)?);
                }
            }
        }
        then(out);
        Ok(())
    }));
    unsafe {
        let _ = profile.GetBrowserExtensions(&handler);
    }
}

fn text(f: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> String {
    let mut s = PWSTR::null();
    if f(&mut s).is_ok() { take_pwstr(s) } else { String::new() }
}

fn id(e: &ICoreWebView2BrowserExtension) -> String {
    text(|p| unsafe { e.Id(p) })
}

fn name(e: &ICoreWebView2BrowserExtension) -> String {
    text(|p| unsafe { e.Name(p) })
}

fn enabled(e: &ICoreWebView2BrowserExtension) -> bool {
    let mut b = BOOL::default();
    unsafe { e.IsEnabled(&mut b) }.is_ok() && b.as_bool()
}

fn find(list: &List, ext_id: &str) -> Option<ICoreWebView2BrowserExtension> {
    list.borrow().iter().find(|e| id(e) == ext_id).cloned()
}

// ---------------------------------------------------------------- folders

/// What an extension's folder tells us (the engine only knows id and name).
pub struct Folder {
    path: PathBuf,
    /// Every name it may be installed under (see `names`).
    names: Vec<String>,
    version: String,
    /// Settings page, relative to the extension root.
    options: Option<String>,
    /// The toolbar button's popup page (manifest "action"), relative to the root.
    popup: Option<String>,
    icon: Option<PathBuf>,
}

fn folders() -> Vec<Folder> {
    let Ok(entries) = std::fs::read_dir(dir()) else { return Vec::new() };
    entries.flatten().filter_map(|e| folder(e.path())).collect()
}

fn read_json(p: PathBuf) -> Option<Value> {
    let text = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

fn folder(path: PathBuf) -> Option<Folder> {
    let m = read_json(path.join("manifest.json"))?;
    let names = names(&path, &m);
    let version = m["version"].as_str().unwrap_or_default().to_string();
    let options = m["options_ui"]["page"].as_str().or(m["options_page"].as_str()).map(String::from);
    let icon = best_icon(&m).map(|rel| path.join(rel.trim_start_matches('/')));
    let popup = m["action"]["default_popup"]
        .as_str()
        .or(m["browser_action"]["default_popup"].as_str())
        .filter(|p| !p.is_empty())
        .map(String::from);
    Some(Folder { path, names, version, options, popup, icon })
}

/// The manifest's name, or for "__MSG_key__" its translation in each of the
/// extension's locales (the engine reports it in its own UI language).
fn names(dir: &Path, m: &Value) -> Vec<String> {
    let Some(name) = m["name"].as_str() else { return Vec::new() };
    let Some(key) = name.strip_prefix("__MSG_").and_then(|k| k.strip_suffix("__")) else {
        return vec![name.to_string()];
    };
    let key = key.to_lowercase();
    let Ok(locales) = std::fs::read_dir(dir.join("_locales")) else { return Vec::new() };
    locales
        .flatten()
        .filter_map(|l| read_json(l.path().join("messages.json")))
        .filter_map(|msgs| {
            let (_, v) = msgs.as_object()?.iter().find(|(k, _)| k.to_lowercase() == key)?;
            v["message"].as_str().map(String::from)
        })
        .collect()
}

/// A 32–64 px icon if there is one, else the closest size.
fn best_icon(m: &Value) -> Option<String> {
    let mut sizes: Vec<(u32, String)> = Vec::new();
    for set in [&m["icons"], &m["action"]["default_icon"], &m["browser_action"]["default_icon"]] {
        match set {
            Value::String(s) => sizes.push((32, s.clone())),
            Value::Object(o) => {
                sizes.extend(o.iter().filter_map(|(k, v)| Some((k.parse().ok()?, v.as_str()?.to_string()))));
            }
            _ => {}
        }
    }
    sizes.sort_by_key(|(s, _)| if *s >= 32 { s - 32 } else { 1000 + 32 - s });
    sizes.into_iter().next().map(|(_, p)| p)
}

fn folder_of<'a>(folders: &'a [Folder], ext_name: &str) -> Option<&'a Folder> {
    let n = ext_name.to_lowercase();
    folders.iter().find(|f| f.names.iter().any(|x| x.to_lowercase() == n))
}

// ---------------------------------------------------------------- panel

/// Only the user's extensions (those with a folder): the engine also lists its
/// own components (clipboard, PDF viewer) that must not be switched off.
pub fn json(list: &List) -> Value {
    let folders = folders();
    let items: Vec<Value> = list
        .borrow()
        .iter()
        .filter_map(|e| {
            let name = name(e);
            let f = folder_of(&folders, &name)?;
            let icon = f
                .icon
                .as_ref()
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|b| iconcache::data_url(&b))
                .unwrap_or_default();
            Some(json!({
                "id": id(e), "name": name, "enabled": enabled(e), "icon": icon,
                "version": f.version, "options": f.options.is_some(),
            }))
        })
        .collect();
    json!({ "items": items })
}

/// The extension's own settings page (Tampermonkey: its dashboard).
pub fn options_url(list: &List, ext_id: &str) -> Option<String> {
    let e = find(list, ext_id)?;
    let folders = folders();
    let page = folder_of(&folders, &name(&e))?.options.clone()?;
    Some(format!("chrome-extension://{ext_id}/{}", page.trim_start_matches('/')))
}

pub fn name_of(list: &List, ext_id: &str) -> Option<String> {
    find(list, ext_id).map(|e| name(&e))
}

/// On/off; the engine remembers it in the profile.
pub fn set_enabled(list: &List, ext_id: &str, on: bool, proxy: &Proxy) {
    let Some(e) = find(list, ext_id) else { return };
    let px = proxy.clone();
    let done = BrowserExtensionEnableCompletedHandler::create(Box::new(move |_| {
        let _ = px.send_event(UserEvent::Extensions);
        Ok(())
    }));
    unsafe {
        let _ = e.Enable(on, &done);
    }
}

/// Uninstalls it and moves its folder to the Recycle Bin (so it is not installed
/// again on the next start, and can still be restored). False if the folder
/// could not be moved.
pub fn remove(list: &List, ext_id: &str, proxy: &Proxy) -> bool {
    let Some(e) = find(list, ext_id) else { return false };
    let folders = folders();
    let recycled = folder_of(&folders, &name(&e)).is_none_or(|f| recycle(&f.path));
    let (l, px, gone) = (list.clone(), proxy.clone(), ext_id.to_string());
    let done = BrowserExtensionRemoveCompletedHandler::create(Box::new(move |_| {
        l.borrow_mut().retain(|x| id(x) != gone);
        let _ = px.send_event(UserEvent::Extensions);
        Ok(())
    }));
    unsafe {
        let _ = e.Remove(&done);
    }
    recycled
}

fn recycle(path: &Path) -> bool {
    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.extend([0, 0]); // the list ends with an empty string
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0) as u16,
        ..Default::default()
    };
    unsafe { SHFileOperationW(&mut op) == 0 && !op.fAnyOperationsAborted.as_bool() }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn picks_a_mid_size_icon() {
        let m = json!({"icons": {"16": "i16.png", "48": "i48.png", "128": "i128.png"}});
        assert_eq!(super::best_icon(&m).as_deref(), Some("i48.png"));
        let m = json!({"icons": {"16": "i16.png"}});
        assert_eq!(super::best_icon(&m).as_deref(), Some("i16.png"));
        let m = json!({"action": {"default_icon": "a.png"}});
        assert_eq!(super::best_icon(&m).as_deref(), Some("a.png"));
    }

    #[test]
    fn resolves_localized_names() {
        let dir = std::env::temp_dir().join("mbrowser-ext-test");
        let _ = std::fs::remove_dir_all(&dir);
        for (loc, msg) in [("en", "Probe"), ("ru", "Проба")] {
            let d = dir.join("_locales").join(loc);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("messages.json"), format!(r#"{{"extName":{{"message":"{msg}"}}}}"#)).unwrap();
        }
        let mut names = super::names(&dir, &json!({"name": "__MSG_extname__"}));
        names.sort();
        assert_eq!(names, vec!["Probe", "Проба"]);
        assert_eq!(super::names(&dir, &json!({"name": "Plain"})), vec!["Plain"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------------- toolbar popups

/// Toolbar buttons: enabled extensions that have a popup page.
pub fn actions(list: &List) -> Value {
    let folders = folders();
    let items: Vec<Value> = list
        .borrow()
        .iter()
        .filter(|e| enabled(e))
        .filter_map(|e| {
            let name = name(e);
            let f = folder_of(&folders, &name)?;
            f.popup.as_ref()?;
            let icon = f
                .icon
                .as_ref()
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|b| iconcache::data_url(&b))
                .unwrap_or_default();
            Some(json!({ "id": id(e), "name": name, "icon": icon }))
        })
        .collect();
    Value::Array(items)
}

pub fn popup_url(list: &List, ext_id: &str) -> Option<String> {
    let e = find(list, ext_id)?;
    let folders = folders();
    let page = folder_of(&folders, &name(&e))?.popup.clone()?;
    Some(format!("chrome-extension://{ext_id}/{}", page.trim_start_matches('/')))
}

/// Runs first in an extension's popup page.
///
/// Hosted in its own webview, the popup *is* the engine's active tab, so an
/// extension asking chrome.tabs for "the active tab" (SponsorBlock: which video
/// is open; Tampermonkey: which site) would find its own popup. Answer with the
/// page the user is looking at instead. Esc closes the popup.
pub fn popup_script(page_url: &str) -> String {
    format!(
        r#"(() => {{
  const PAGE = {page};
  const tabs = window.chrome && chrome.tabs;
  if (tabs && tabs.query && PAGE) {{
    const query = tabs.query.bind(tabs);
    const base = u => (u || '').split('#')[0].replace(/\/$/, '');
    let page = null;
    const find = () => page || (page = query({{}}).then(all =>
      all.find(t => t.url === PAGE) || all.find(t => base(t.url) === base(PAGE)) || null).catch(() => null));
    const patched = function (info, cb) {{
      const active = info && (info.active || info.currentWindow || info.lastFocusedWindow || info.highlighted);
      const p = (active ? find() : Promise.resolve(null)).then(t => t ? [t] : query(info || {{}}));
      if (typeof cb === 'function') {{ p.then(r => cb(r)); return; }}
      return p;
    }};
    try {{ Object.defineProperty(tabs, 'query', {{value: patched, configurable: true, writable: true}}); }}
    catch {{ tabs.query = patched; }}
  }}
  addEventListener('keydown', e => {{ if (e.key === 'Escape') window.__mbClose = true; }});
}})();"#,
        page = serde_json::to_string(page_url).unwrap_or_else(|_| "\"\"".into())
    )
}

/// Measures the popup's content (like Chrome, the popup is as big as its page,
/// up to 800×600); the reply is a JSON string `{w, h, close}`.
pub const POPUP_MEASURE: &str = r#"(() => {
  const r = document.documentElement, b = document.body;
  if (!r || !b) return JSON.stringify({w: 0, h: 0, close: !!window.__mbClose});
  if (r.style.width !== 'max-content') r.style.width = 'max-content';
  // Height of the content itself: the page's own box is at least the window's
  // height, which would never let the popup shrink.
  const px = v => parseFloat(v) || 0, cs = getComputedStyle(b);
  let bottom = 0;
  for (const el of b.children) {
    const box = el.getBoundingClientRect();
    if (box.width || box.height) bottom = Math.max(bottom, box.bottom + px(getComputedStyle(el).marginBottom));
  }
  const h = bottom + px(cs.paddingBottom) + px(cs.borderBottomWidth) + px(cs.marginBottom) + scrollY;
  return JSON.stringify({w: Math.ceil(r.getBoundingClientRect().width), h: Math.ceil(h), close: !!window.__mbClose});
})()"#;
