//! Persistent state: settings, session, history, bookmarks.
//! Everything lives as small JSON files in %APPDATA%\MBrowser.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// `MBROWSER_HOME=<dir>` keeps everything (settings + engine profile) in one
/// folder: a portable install or a throwaway test profile.
fn home_override() -> Option<PathBuf> {
    std::env::var_os("MBROWSER_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn data_dir() -> PathBuf {
    let dir = home_override().unwrap_or_else(|| {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        base.join("MBrowser")
    });
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Browser engine profile (cookies, cache) — big, so it goes to LOCALAPPDATA.
pub fn engine_dir() -> PathBuf {
    let dir = match home_override() {
        Some(home) => home.join("Engine"),
        None => {
            let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(data_dir);
            base.join("MBrowser").join("Engine")
        }
    };
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Disposable caches (icons) live next to the engine profile.
pub fn cache_dir() -> PathBuf {
    let engine = engine_dir();
    engine.parent().map(PathBuf::from).unwrap_or(engine)
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load<T: DeserializeOwned + Default>(name: &str) -> T {
    fs::read(data_dir().join(name))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Atomic write: tmp file + rename, so a crash never leaves a half-written file.
fn save<T: Serialize>(name: &str, value: &T) {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = data_dir();
    let tmp = dir.join(format!("{name}.{}.tmp", SEQ.fetch_add(1, Ordering::Relaxed)));
    if let Ok(bytes) = serde_json::to_vec(value) {
        if fs::write(&tmp, bytes).is_ok() {
            let _ = fs::rename(&tmp, dir.join(name));
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }
}

// ---------------------------------------------------------------- settings

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    /// "google" | "yandex" | "duckduckgo" | "bing" | custom template with {q}
    pub search: String,
    pub adblock: bool,
    /// Also hide cookie consent banners (EasyList Cookie + uBO cookie notices).
    pub adblock_cookies: bool,
    /// Also hide annoyances: newsletter popups, social widgets, "open in app"…
    pub adblock_annoyances: bool,
    /// Extra filter list URLs (uBO / AdBlock Plus / hosts format).
    pub adblock_lists: Vec<String>,
    /// Skip sponsor segments on YouTube (sponsor.ajay.app).
    pub sponsorblock: bool,
    /// SponsorBlock categories to skip: sponsor, selfpromo, interaction, intro,
    /// outro, preview, music_offtopic, filler.
    pub sponsorblock_categories: Vec<String>,
    /// Page translation target language.
    pub translate_to: String,
    /// Edge tracking prevention: "none" | "basic" | "balanced" | "strict"
    pub tracking_prevention: String,
    /// Put background tabs to sleep after N minutes (0 = never).
    pub sleep_minutes: u64,
    pub restore_session: bool,
    /// Sites where the blocker is switched off by the user.
    pub shield_off: Vec<String>,
    /// File extensions opened automatically once downloaded ("png", "pdf").
    pub auto_open: Vec<String>,
    /// Tablo tiles: "small" | "medium" | "large".
    pub tablo_size: String,
    /// Tablo width: "auto" (fits the window) | "small" | "medium" | "large".
    pub tablo_width: String,
    /// Experimental: live blur under the Tablo (Windows acrylic window). Off:
    /// it misbehaves (blurs the desktop after some open/close cycles).
    pub live_blur: bool,
    /// New versions come from the GitHub releases by themselves (update.rs).
    pub auto_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            search: "google".into(),
            adblock: true,
            adblock_cookies: true,
            adblock_annoyances: false,
            adblock_lists: Vec::new(),
            sponsorblock: true,
            sponsorblock_categories: ["sponsor", "selfpromo", "interaction"].map(String::from).to_vec(),
            translate_to: "ru".into(),
            tracking_prevention: "balanced".into(),
            sleep_minutes: 15,
            restore_session: true,
            shield_off: Vec::new(),
            auto_open: Vec::new(),
            tablo_size: "large".into(),
            tablo_width: "auto".into(),
            live_blur: false,
            auto_update: true,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let s: Self = load("settings.json");
        s.save(); // materialize defaults so the user can find and edit the file
        s
    }
    pub fn save(&self) {
        save("settings.json", self);
    }
    pub fn search_template(&self) -> &str {
        match self.search.as_str() {
            "yandex" => "https://yandex.ru/search/?text={q}",
            "duckduckgo" | "ddg" => "https://duckduckgo.com/?q={q}",
            "bing" => "https://www.bing.com/search?q={q}",
            custom if custom.contains("{q}") => custom,
            _ => "https://www.google.com/search?q={q}",
        }
    }
    pub fn search_url(&self, q: &str) -> String {
        self.search_template().replace("{q}", &encode_query(q))
    }
}

pub fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------- session

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct SessionTab {
    pub url: String,
    pub title: String,
    pub pinned: bool,
    /// Favicon URL, so restored (sleeping) tabs show their icon right away.
    pub icon: String,
    /// Back / forward history around `url` (oldest first), rebuilt when the tab
    /// gets a webview again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub back: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fwd: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct WinState {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub maximized: bool,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct SessionSpace {
    pub name: String,
    pub color: String,
    /// Emoji shown instead of the tab count ("" = show the count).
    pub icon: String,
    pub tabs: Vec<SessionTab>,
    pub active: usize,
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Session {
    /// Pre-spaces format (one flat tab list); only read, to migrate old sessions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<SessionTab>,
    pub active: usize,
    pub window: Option<WinState>,
    pub spaces: Vec<SessionSpace>,
    /// Index of the current space.
    pub space: usize,
}

impl Session {
    pub fn load() -> Self {
        load("session.json")
    }
    pub fn save(&self) {
        save("session.json", self);
    }
}

// ---------------------------------------------------------------- history

#[derive(Serialize, Deserialize, Clone)]
pub struct HistEntry {
    pub url: String,
    pub title: String,
    pub visits: u32,
    pub last: u64,
}

pub struct History {
    pub entries: Vec<HistEntry>,
    index: HashMap<String, usize>,
    dirty: bool,
}

const HISTORY_MAX: usize = 20_000;
const HISTORY_KEEP: usize = 15_000;

impl History {
    pub fn load() -> Self {
        let entries: Vec<HistEntry> = load("history.json");
        let mut h = Self { entries, index: HashMap::new(), dirty: false };
        h.reindex();
        h
    }

    /// A history that never touches the disk (tests).
    #[cfg(test)]
    pub fn from_entries(entries: Vec<HistEntry>) -> Self {
        let mut h = Self { entries, index: HashMap::new(), dirty: false };
        h.reindex();
        h
    }

    fn reindex(&mut self) {
        self.index = self.entries.iter().enumerate().map(|(i, e)| (e.url.clone(), i)).collect();
    }

    pub fn visit(&mut self, url: &str, title: &str) {
        let t = now();
        if let Some(&i) = self.index.get(url) {
            let e = &mut self.entries[i];
            // Same URL reported twice within a few seconds (redirect, SPA echo) is one visit.
            if t.saturating_sub(e.last) > 3 {
                e.visits += 1;
            }
            e.last = t;
            if !title.is_empty() {
                e.title = title.to_string();
            }
        } else {
            self.index.insert(url.to_string(), self.entries.len());
            self.entries.push(HistEntry { url: url.into(), title: title.into(), visits: 1, last: t });
            if self.entries.len() > HISTORY_MAX {
                self.entries.sort_unstable_by(|a, b| b.last.cmp(&a.last));
                self.entries.truncate(HISTORY_KEEP);
                self.reindex();
            }
        }
        self.dirty = true;
    }

    pub fn set_title(&mut self, url: &str, title: &str) {
        if let Some(&i) = self.index.get(url) {
            if self.entries[i].title != title {
                self.entries[i].title = title.to_string();
                self.dirty = true;
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.index.clear();
        self.dirty = true;
    }

    /// Serialization and disk IO happen off the UI thread.
    pub fn flush(&mut self) {
        if self.dirty {
            self.dirty = false;
            let data = self.entries.clone();
            std::thread::spawn(move || save("history.json", &data));
        }
    }

    pub fn flush_sync(&mut self) {
        if self.dirty {
            self.dirty = false;
            save("history.json", &self.entries);
        }
    }
}

// ---------------------------------------------------------------- tablo (speed dial)

#[derive(Serialize, Deserialize, Clone)]
pub struct TabloItem {
    pub url: String,
    pub title: String,
}

/// The user's own tiles on the start screen, in their order.
pub struct Tablo {
    pub items: Vec<TabloItem>,
}

impl Tablo {
    pub fn load() -> Self {
        let path = data_dir().join("tablo.json");
        let items = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(Self::defaults);
        let tablo = Self { items };
        if !path.exists() {
            tablo.save();
        }
        tablo
    }

    fn defaults() -> Vec<TabloItem> {
        [
            ("https://www.youtube.com/", "YouTube"),
            ("https://mail.google.com/", "Gmail"),
            ("https://vk.com/", "ВКонтакте"),
            ("https://mail.ru/", "Главная Mail"),
        ]
        .iter()
        .map(|(url, title)| TabloItem { url: url.to_string(), title: title.to_string() })
        .collect()
    }

    pub fn save(&self) {
        save("tablo.json", &self.items);
    }
}

/// Best (large) icon URL per site, learned from pages the user visits.
/// "" means "looked, nothing good" so the page is not probed again.
pub struct Icons {
    pub map: HashMap<String, String>,
    dirty: bool,
}

impl Icons {
    pub fn load() -> Self {
        let mut map: HashMap<String, String> = load("icons.json");
        // Crisp official icons for the default tiles whose own pages don't expose one.
        for (site, url) in [
            ("youtube.com", "https://www.gstatic.com/youtube/img/branding/favicon/favicon_144x144.png"),
            ("mail.google.com", "https://www.gstatic.com/images/branding/product/2x/gmail_2020q4_48dp.png"),
        ] {
            let slot = map.entry(site.to_string()).or_default();
            if slot.is_empty() {
                *slot = url.to_string();
            }
        }
        Self { map, dirty: false }
    }
    pub fn set(&mut self, site: String, url: String) {
        if self.map.get(&site) != Some(&url) {
            self.map.insert(site, url);
            self.dirty = true;
        }
    }
    pub fn flush(&mut self) {
        if self.dirty {
            self.dirty = false;
            save("icons.json", &self.map);
        }
    }
}

// ---------------------------------------------------------------- downloads

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct DownloadItem {
    #[serde(skip)]
    pub id: u32,
    pub url: String,
    /// Page the download started from.
    pub page: String,
    pub path: String,
    pub total: i64,
    pub received: i64,
    /// 0 running, 1 done, 2 failed, 3 cancelled.
    pub state: u8,
    pub time: u64,
    #[serde(skip)]
    pub private: bool,
    #[serde(skip)]
    pub paused: bool,
    /// Tab (webview) the download runs in; 0 for entries loaded from disk.
    #[serde(skip)]
    pub tab: u32,
    /// The file is on disk: checked when the list opens and when the download
    /// ends, not on every progress update.
    #[serde(skip)]
    pub exists: bool,
}

pub fn load_downloads() -> Vec<DownloadItem> {
    load("downloads.json")
}

/// Finished, non-private downloads only; newest 200.
pub fn save_downloads(items: &[DownloadItem]) {
    let keep: Vec<&DownloadItem> = items.iter().filter(|d| !d.private && d.state != 0).take(200).collect();
    save("downloads.json", &keep);
}

// ---------------------------------------------------------------- bookmarks

#[derive(Serialize, Deserialize, Clone)]
pub struct Bookmark {
    pub url: String,
    pub title: String,
    pub added: u64,
}

pub struct Bookmarks {
    pub items: Vec<Bookmark>,
}

impl Bookmarks {
    pub fn load() -> Self {
        Self { items: load("bookmarks.json") }
    }
    pub fn contains(&self, url: &str) -> bool {
        self.items.iter().any(|b| b.url == url)
    }
    pub fn save(&self) {
        save("bookmarks.json", &self.items);
    }
    /// Returns true if the page is bookmarked after the call.
    pub fn toggle(&mut self, url: &str, title: &str) -> bool {
        let added = if let Some(i) = self.items.iter().position(|b| b.url == url) {
            self.items.remove(i);
            false
        } else {
            self.items.push(Bookmark { url: url.into(), title: title.into(), added: now() });
            true
        };
        save("bookmarks.json", &self.items);
        added
    }
}
