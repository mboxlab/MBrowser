//! Tablo icons cached on disk, so tiles show their icons instantly after a
//! restart instead of a letter while the network catches up.
//!
//! Icons are fetched once, in a background thread, with Windows' own downloader
//! (urlmon) and stored as files; the UI gets them as `data:` URLs.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use windows::{
    Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, IBindStatusCallback, Urlmon::URLDownloadToFileW},
    core::{HSTRING, IUnknown},
};

use crate::{
    store,
    tab::{self, Proxy, UserEvent},
};

pub struct IconCache {
    dir: PathBuf,
    data: HashMap<String, String>,
    fetching: HashSet<String>,
}

impl IconCache {
    pub fn new() -> Self {
        let dir = store::cache_dir().join("Icons");
        let _ = fs::create_dir_all(&dir);
        Self { dir, data: HashMap::new(), fetching: HashSet::new() }
    }

    fn file(&self, site: &str) -> PathBuf {
        let safe: String = site.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
        self.dir.join(format!("{safe}.img"))
    }

    /// The cached icon as a `data:` URL, without fetching anything.
    pub fn peek(&mut self, site: &str) -> Option<String> {
        if let Some(d) = self.data.get(site) {
            return Some(d.clone());
        }
        let url = data_url(&fs::read(self.file(site)).ok()?)?;
        self.data.insert(site.to_string(), url.clone());
        Some(url)
    }

    /// Like `peek`, but starts a one-time background download when missing;
    /// `UserEvent::IconCached(site)` arrives once it is on disk.
    pub fn get(&mut self, site: &str, candidates: Vec<String>, proxy: &Proxy) -> Option<String> {
        if let Some(url) = self.peek(site) {
            return Some(url);
        }
        if self.fetching.insert(site.to_string()) {
            let (file, proxy) = (self.file(site), proxy.clone());
            std::thread::spawn(move || {
                if fetch(&candidates, &file) {
                    let _ = proxy.send_event(UserEvent::IconCached);
                }
            });
        }
        None
    }

    /// A better icon became known (e.g. learned from the site): fetch again.
    pub fn invalidate(&mut self, site: &str) {
        self.data.remove(site);
        self.fetching.remove(site);
        let _ = fs::remove_file(self.file(site));
    }
}

fn fetch(candidates: &[String], file: &Path) -> bool {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let tmp = file.with_extension("part");
    for url in candidates {
        let ok = unsafe {
            URLDownloadToFileW(
                None::<&IUnknown>,
                &HSTRING::from(url.as_str()),
                &HSTRING::from(tmp.as_os_str()),
                0,
                None::<&IBindStatusCallback>,
            )
        }
        .is_ok();
        // Tiny files are "no icon" placeholders (Yandex answers 1x1 for unknown sites).
        if ok && fs::read(&tmp).map(|b| b.len() > 100 && mime(&b).is_some()).unwrap_or(false) {
            return fs::rename(&tmp, file).is_ok();
        }
    }
    let _ = fs::remove_file(&tmp);
    false
}

fn mime(b: &[u8]) -> Option<&'static str> {
    Some(match b {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', ..] => "image/gif",
        [0, 0, 1, 0, ..] => "image/x-icon",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        [b'B', b'M', ..] => "image/bmp",
        _ if String::from_utf8_lossy(&b[..b.len().min(512)]).contains("<svg") => "image/svg+xml",
        _ => return None,
    })
}

pub fn data_url(bytes: &[u8]) -> Option<String> {
    let m = mime(bytes)?;
    Some(format!("data:{m};base64,{}", tab::base64(bytes)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn sniffs_images_and_rejects_html() {
        assert_eq!(super::mime(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(super::mime(b"\0\0\x01\0\x01\0"), Some("image/x-icon"));
        assert_eq!(super::mime(b"<?xml version='1.0'?><svg xmlns=...>"), Some("image/svg+xml"));
        assert_eq!(super::mime(b"<!doctype html><html>404</html>"), None);
    }

    /// Network: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn downloads_a_real_icon_and_skips_placeholders() {
        let dir = std::env::temp_dir().join("mbrowser-icon-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("vk.img");
        let _ = std::fs::remove_file(&file);
        let tries = vec![
            "https://favicon.yandex.net/favicon/v2/nonexistent-site-xyz123.com?size=120".to_string(),
            "https://favicon.yandex.net/favicon/v2/vk.com?size=120".to_string(),
        ];
        assert!(super::fetch(&tries, &file));
        let bytes = std::fs::read(&file).unwrap();
        assert!(bytes.len() > 1000, "got {} bytes", bytes.len());
        assert!(super::data_url(&bytes).unwrap().starts_with("data:image/"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
