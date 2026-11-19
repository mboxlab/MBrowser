//! Browser core: spaces, tabs, layout, panels, commands, UI sync.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::Path,
    rc::Rc,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};
use tao::{
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event_loop::EventLoop,
    platform::windows::{IconExtWindows, WindowBuilderExtWindows, WindowExtWindows},
    window::{Fullscreen, Icon, ResizeDirection, Theme, Window, WindowBuilder},
};
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, POINT, WPARAM},
        Graphics::Gdi::{CombineRgn, CreateRectRgn, DeleteObject, RGN_OR, SetWindowRgn},
        UI::WindowsAndMessaging::*,
    },
    core::HSTRING,
};
use wry::{
    MemoryUsageLevel, Rect, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows,
    WebViewExtWindows,
};

use crate::{
    blocker::{self, BlockCtx, host_of, site_key},
    net,
    ext,
    glass::{self, Glass},
    iconcache::IconCache,
    internal::{self, BOOKMARKS_URL, SETTINGS_URL},
    omni, pages,
    store::{
        self, Bookmarks, DownloadItem, History, Icons, Session, SessionSpace, SessionTab, Settings, Tablo, TabloItem,
        WinState,
    },
    tab::{self, Cmd, Proxy, Source, Spec, TabShared, UserEvent},
};

#[path = "recover.rs"]
pub mod recover;

/// Height of the browser chrome (tab strip + toolbar), logical px.
const CHROME_H: f64 = 82.0;
const UI_HTML: &str = include_str!("ui.html");
const HISTORY_URL: &str = "mb:history";
/// Runs in a loaded page: the largest icon it declares (≥64px or SVG), or "".
const ICON_PROBE_JS: &str = r#"(() => {
  let best = '', size = 0;
  for (const l of document.querySelectorAll('link[rel~="icon"],link[rel~="apple-touch-icon"],link[rel~="apple-touch-icon-precomposed"]')) {
    const href = l.href;
    if (!href || !/^https?:/.test(href)) continue;
    const sizes = (l.getAttribute('sizes') || '').split(/\s+/).map(s => parseInt(s, 10)).filter(n => n > 0);
    let n = sizes.length ? Math.max(...sizes) : (/apple-touch/.test(l.rel) ? 180 : 0);
    if (/\.svg(\?|$)/i.test(href) || /svg/.test(l.type || '')) n = Math.max(n, 256);
    if (n > size) { size = n; best = href; }
  }
  return size >= 64 ? best : '';
})()"#;
const SPACE_COLORS: [&str; 7] = ["#58b4f0", "#5ec8a0", "#f0a35e", "#ef6f8f", "#8a96ff", "#c48cff", "#e8d25a"];

#[derive(Clone, Copy, PartialEq)]
enum Internal {
    NewTab,
    History,
    Bookmarks,
    Settings,
}

impl Internal {
    /// The `mb:` address of a built-in page ("" for the new tab).
    fn url(self) -> &'static str {
        match self {
            Internal::NewTab => "",
            Internal::History => HISTORY_URL,
            Internal::Bookmarks => BOOKMARKS_URL,
            Internal::Settings => SETTINGS_URL,
        }
    }
    fn from_url(url: &str) -> Option<Self> {
        [Internal::History, Internal::Bookmarks, Internal::Settings].into_iter().find(|k| k.url() == url)
    }
    fn title(self) -> &'static str {
        match self {
            Internal::NewTab => "Новая вкладка",
            Internal::History => "История",
            Internal::Bookmarks => "Закладки",
            Internal::Settings => "Настройки",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Fs {
    Off,
    Window,
    Content,
}

/// Full-window panels drawn by the chrome over a blurred snapshot of the page.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Panel {
    Launch,
    Spaces,
    Downloads,
    Extensions,
}

impl Panel {
    fn name(self) -> &'static str {
        match self {
            Panel::Launch => "launch",
            Panel::Spaces => "spaces",
            Panel::Downloads => "downloads",
            Panel::Extensions => "extensions",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "spaces" => Panel::Spaces,
            "downloads" => Panel::Downloads,
            "extensions" => Panel::Extensions,
            _ => Panel::Launch,
        }
    }
}

/// A named set of tabs with its own accent color.
struct Space {
    id: u32,
    name: String,
    color: String,
    /// Emoji shown instead of the tab count ("" = show the count).
    icon: String,
    /// Tab that was active when the user left this space.
    active: u32,
}

struct Tab {
    id: u32,
    space: u32,
    url: String,
    title: String,
    icon: String,
    loading: bool,
    audio: bool,
    muted: bool,
    private: bool,
    pinned: bool,
    internal: Option<Internal>,
    last_active: Instant,
    view: Option<WebView>,
    /// Webview just created with a dark base color; switch to white after the
    /// first load so pages without their own background stay readable.
    fresh: bool,
    /// The page's text is mostly not Russian: offer to translate it.
    foreign: bool,
    shared: Rc<TabShared>,
}

impl Tab {
    fn new(id: u32, space: u32, url: &str, private: bool) -> Self {
        let internal = match url {
            "" | "about:blank" => Some(Internal::NewTab),
            _ => Internal::from_url(url),
        };
        Self {
            id,
            space,
            url: if internal.is_some() { String::new() } else { url.to_string() },
            title: String::new(),
            icon: String::new(),
            loading: false,
            audio: false,
            muted: false,
            private,
            pinned: false,
            internal,
            last_active: Instant::now(),
            view: None,
            fresh: false,
            foreign: false,
            shared: Rc::new(TabShared::default()),
        }
    }

    fn display_title(&self) -> String {
        if !self.title.is_empty() && self.title != "about:blank" {
            return self.title.clone();
        }
        match self.internal {
            Some(k) => k.title().into(),
            None => host_of(&self.url).map(|h| site_key(h).to_string()).unwrap_or_else(|| self.url.clone()),
        }
    }

    fn session_url(&self) -> String {
        match self.internal {
            Some(k) => k.url().into(),
            None => self.url.clone(),
        }
    }
}

/// The launcher's hidden webview: created when the Tablo opens, starts loading a
/// tile as soon as the pointer rests on it, and becomes the next new tab opened
/// from the launcher — the click then shows a page that is already loading.
struct Spare {
    tab: Tab,
    /// URL it was told to load ("" = still blank).
    target: String,
    /// A page has committed: switching to another URL would leave it in Back.
    committed: bool,
}

impl Spare {
    fn go(&mut self, url: &str) {
        if self.target == url {
            return;
        }
        if let Some(v) = &self.tab.view {
            let _ = v.load_url(url);
        }
        self.target = url.to_string();
        self.tab.url = url.to_string();
        self.tab.title.clear();
        self.tab.icon.clear();
    }
}

#[derive(Deserialize)]
struct Msg {
    cmd: String,
    #[serde(default)]
    id: u32,
    #[serde(default)]
    text: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    flag: bool,
    #[serde(default)]
    num: f64,
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
}

pub struct App {
    window: Window,
    proxy: Proxy,
    _ctx: WebContext,
    env: ICoreWebView2Environment,
    ui: WebView,
    spaces: Vec<Space>,
    space: u32,
    tabs: Vec<Tab>,
    active: u32,
    next_id: u32,
    closed: Vec<SessionTab>,
    settings: Settings,
    history: History,
    bookmarks: Bookmarks,
    tablo: Tablo,
    icons: Icons,
    /// Tablo icons on disk (data: URLs), filled in the background.
    icon_cache: RefCell<IconCache>,
    /// Download list, newest first (running ones included).
    downloads: Vec<DownloadItem>,
    dl_ops: tab::Downloads,
    /// Installed browser extensions (for the "Расширения" panel).
    exts: ext::List,
    /// A download finished since the panel was last opened.
    dl_unseen: bool,
    block: Rc<BlockCtx>,
    /// Ad-blocker updater: a background update is running / its last error /
    /// no automatic retry before this moment.
    ab_busy: bool,
    ab_error: String,
    ab_next_try: Instant,
    /// Hidden page that builds uBO's scriptlet resources (alive while it works).
    ab_res_view: Option<(WebView, Instant)>,
    /// SponsorBlock answers by hash prefix (this session only).
    sb_cache: HashMap<String, String>,
    /// Address bar dropdown: waiting for the page snapshot / raised over the page.
    drop_wait: bool,
    drop_up: bool,
    fs: Fs,
    ui_ready: bool,
    ui_dirty: bool,
    session_dirty: bool,
    ui_focus: bool,
    win: Option<WinState>,
    /// Panel that is open (or being prepared behind the page).
    panel: Option<Panel>,
    /// Waiting for the page snapshot before the panel can be prepared.
    panel_wait: Option<Panel>,
    /// The panel is raised above the page.
    panel_up: bool,
    /// A dropdown panel (downloads, extensions) is open: the chrome is clipped to
    /// these rectangles (CSS px), so the page stays live around it.
    ui_clip: Option<Vec<[f64; 4]>>,
    /// The open launcher is the "home" of an empty space (it is the page itself,
    /// so it cannot be dismissed).
    home: bool,
    /// Hidden prerender webview, alive while the launcher is open.
    spare: Option<Spare>,
    /// The downloads list is open as a layer over the Tablo.
    dl_watch: bool,
    /// Window over the page area that blurs it live under the Tablo.
    glass: Option<Glass>,
    /// The open Tablo is drawn in the glass window (not over a snapshot).
    glass_on: bool,
    /// Popup of an extension's toolbar button.
    xpop: Option<crate::xpopup::Popup>,
    /// Which popup closed when (a click on its own button closes it, not reopens).
    xpop_closed: Option<(String, Instant)>,
    /// The IPC message being handled came from the glass window.
    from_glass: bool,
    maximized: bool,
    /// Crash-recovery bookkeeping (recover.rs).
    recover: recover::State,
    /// Random per run; embedded in the management pages (bookmarks, settings) so
    /// that only they can change settings (see `internal_message`).
    int_token: String,
}

impl App {
    pub fn new(event_loop: &EventLoop<UserEvent>) -> wry::Result<Self> {
        recover::on_start();
        let proxy = event_loop.create_proxy();
        // The compiled ad-blocking engine is read while the window and the
        // browser environment are being created.
        let engine_job = std::thread::spawn(blocker::load_cached);
        let settings = Settings::load();
        let session = if settings.restore_session || recover::restarted() { Session::load() } else { Session::default() };

        let mut wb = WindowBuilder::new()
            .with_title(crate::APP_NAME)
            // Tabs live in the title bar: no system caption, but native resize
            // borders, shadow, snap and maximize are kept.
            .with_decorations(false)
            .with_undecorated_shadow(true)
            .with_theme(Some(Theme::Dark))
            .with_visible(false)
            .with_window_icon(app_icon(SM_CXSMICON))
            .with_taskbar_icon(app_icon(SM_CXICON))
            .with_min_inner_size(LogicalSize::new(520.0, 360.0));
        match session.window {
            Some(ws) if ws.w > 200 && ws.h > 200 => {
                wb = wb.with_inner_size(PhysicalSize::new(ws.w, ws.h)).with_maximized(ws.maximized);
                let on_screen = event_loop.available_monitors().any(|m| {
                    let (p, s) = (m.position(), m.size());
                    let (cx, cy) = (ws.x + 60, ws.y + 20);
                    cx >= p.x && cx < p.x + s.width as i32 && cy >= p.y && cy < p.y + s.height as i32
                });
                if on_screen {
                    wb = wb.with_position(PhysicalPosition::new(ws.x, ws.y));
                }
            }
            _ => wb = wb.with_inner_size(LogicalSize::new(1280.0, 820.0)),
        }
        // Automated UI checks (debug builds): show up without taking the user's focus.
        #[cfg(debug_assertions)]
        if std::env::var_os("MBROWSER_BACKGROUND").is_some() {
            wb = wb.with_focused(false).with_always_on_top(true);
        }
        let window = wb.build(event_loop).map_err(|e| wry::Error::Io(std::io::Error::other(e.to_string())))?;

        // The chrome UI. Creating it first also creates the shared browser environment.
        let mut ctx = WebContext::new(Some(store::engine_dir()));
        let ipc_proxy = proxy.clone();
        // Unpacked Chrome extensions (e.g. uBlock Origin Lite) — one folder each.
        let ext_dir = store::data_dir().join("extensions");
        let _ = std::fs::create_dir_all(&ext_dir);
        let ui = WebViewBuilder::new_with_web_context(&mut ctx)
            // wry defaults minus forced autoplay; SmartScreen stays on for safety.
            .with_additional_browser_args("--disable-features=msWebOOUI,msPdfOOUI")
            .with_browser_extensions_enabled(true)
            .with_html(UI_HTML)
            .with_navigation_handler({
                // The chrome must never navigate away (it has IPC access). A link or
                // file dropped onto the tab strip opens as a new tab instead.
                let proxy = proxy.clone();
                move |url| {
                    if url.starts_with("about:") || url.starts_with("data:") {
                        return true;
                    }
                    let _ = proxy.send_event(UserEvent::OpenTab { url, opener: 0 });
                    false
                }
            })
            .with_bounds(chrome_rect(&window, f64::from(u16::MAX)))
            .with_background_color((24, 25, 30, 255))
            .with_ipc_handler(move |req| {
                let _ = ipc_proxy.send_event(UserEvent::Ipc(req.into_body()));
            })
            .with_hotkeys_zoom(false)
            .with_browser_accelerator_keys(false)
            .with_devtools(cfg!(debug_assertions))
            .build_as_child(&window)?;
        unsafe {
            let _ = tab::hook_keys(&ui, &proxy);
            recover::hook(&ui, recover::UI, &proxy);
        }
        tab::clip_siblings(&ui);
        tab::dark_container_class(&ui);
        let exts = ext::List::default();
        ext::load(&ui, &exts, &proxy);
        let env = ui.environment();

        let list = blocker::user_list_path();
        if !list.exists() {
            let _ = std::fs::write(
                &list,
                "# Свои правила блокировки, по одному на строку:\n\
                 #   ads.example.com              — домен (и все поддомены)\n\
                 #   0.0.0.0 ads.example.com      — формат hosts\n\
                 #   example.com/promo/           — адреса, начинающиеся так\n\
                 #   ||example.com^$third-party   — любые правила uBlock Origin / AdBlock Plus\n\
                 #   example.com##.banner         — скрыть элементы на сайте\n\
                 # Можно вставить целый список, например https://github.com/StevenBlack/hosts\n\
                 # Изменения применяются сами через несколько секунд.\n",
            );
        }
        let engine = engine_job.join().unwrap_or_else(|_| blocker::load_cached());
        let block = Rc::new(BlockCtx::new(engine, settings.adblock, settings.shield_off.iter().cloned().collect()));
        block.sponsorblock.set(settings.sponsorblock);

        let mut app = Self {
            window,
            proxy,
            _ctx: ctx,
            env,
            ui,
            spaces: Vec::new(),
            space: 0,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            closed: Vec::new(),
            settings,
            history: History::load(),
            bookmarks: Bookmarks::load(),
            tablo: Tablo::load(),
            icons: Icons::load(),
            icon_cache: RefCell::new(IconCache::new()),
            downloads: {
                let mut list = store::load_downloads();
                for d in &mut list {
                    d.id = tab::NEXT_DL.fetch_add(1, Ordering::Relaxed);
                }
                list
            },
            dl_ops: Rc::new(RefCell::new(HashMap::new())),
            exts,
            dl_unseen: false,
            block,
            ab_busy: false,
            ab_error: String::new(),
            // First check a few seconds after startup, not during it.
            ab_next_try: Instant::now() + Duration::from_secs(5),
            ab_res_view: None,
            sb_cache: HashMap::new(),
            drop_wait: false,
            drop_up: false,
            fs: Fs::Off,
            ui_ready: false,
            ui_dirty: true,
            session_dirty: false,
            ui_focus: false,
            win: session.window,
            panel: None,
            panel_wait: None,
            panel_up: false,
            home: false,
            ui_clip: None,
            spare: None,
            dl_watch: false,
            glass: None,
            glass_on: false,
            xpop: None,
            xpop_closed: None,
            from_glass: false,
            maximized: false,
            int_token: internal::new_token(),
            recover: Default::default(),
        };

        // Restore spaces; restored tabs start asleep, only the active one gets a webview.
        let mut saved = session.spaces.clone();
        if saved.is_empty() {
            // Older sessions had one flat tab list.
            saved.push(SessionSpace {
                name: "Основное".into(),
                color: SPACE_COLORS[0].into(),
                icon: String::new(),
                tabs: session.tabs.clone(),
                active: session.active,
            });
        }
        for (n, ss) in saved.iter().enumerate() {
            let sid = app.alloc_id();
            let mut active = 0;
            for (i, st) in ss.tabs.iter().enumerate() {
                let id = app.alloc_id();
                let mut t = Tab::new(id, sid, &st.url, false);
                t.title = st.title.clone();
                t.icon = st.icon.clone();
                t.pinned = st.pinned;
                app.tabs.push(t);
                if i == ss.active.min(ss.tabs.len().saturating_sub(1)) {
                    active = id;
                }
            }
            let name = if ss.name.trim().is_empty() { format!("Пространство {}", n + 1) } else { ss.name.clone() };
            let color = if ss.color.is_empty() { SPACE_COLORS[n % SPACE_COLORS.len()].into() } else { ss.color.clone() };
            app.spaces.push(Space { id: sid, name, color, icon: ss.icon.clone(), active });
        }
        let current = &app.spaces[session.space.min(app.spaces.len() - 1)];
        let (sid, sactive) = (current.id, current.active);
        app.space = sid;

        let cli_url = crate::shell::cli_input().and_then(|a| omni::resolve(&a, &app.settings));
        if let Some(url) = cli_url {
            app.open(&url, None, false, true);
        } else if sactive != 0 {
            app.activate(sactive);
        } else {
            app.show_home(); // completes once the chrome is loaded ("ready")
        }
        app.window.set_visible(true);
        if app.settings.live_blur && glass::supported() {
            app.glass = Glass::new(event_loop, &app.window);
        }
        app.xpop = crate::xpopup::Popup::new(event_loop, &app.window);
        Ok(app)
    }

    fn alloc_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn idx(&self, id: u32) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn active_idx(&self) -> Option<usize> {
        self.idx(self.active)
    }

    fn active_view(&self) -> Option<&WebView> {
        self.active_idx().and_then(|i| self.tabs[i].view.as_ref())
    }

    /// Global indices of the current space's tabs, in display order.
    fn visible(&self) -> Vec<usize> {
        (0..self.tabs.len()).filter(|&i| self.tabs[i].space == self.space).collect()
    }

    fn space_mut(&mut self, id: u32) -> Option<&mut Space> {
        self.spaces.iter_mut().find(|s| s.id == id)
    }

    // ------------------------------------------------------------ layout

    fn chrome_h(&self) -> f64 {
        if self.fs == Fs::Off { CHROME_H } else { 0.0 }
    }

    fn content_rect(&self) -> Rect {
        let size = self.window.inner_size();
        let top = ((self.chrome_h() * self.window.scale_factor()).round() as u32).min(size.height);
        Rect {
            position: PhysicalPosition::new(0, top as i32).into(),
            size: PhysicalSize::new(size.width, size.height - top).into(),
        }
    }

    pub fn layout(&mut self) {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return; // minimized
        }
        // The chrome always spans the whole window and normally lies *under* the
        // page; overlays appear by raising it, never by resizing (no flicker).
        let _ = self.ui.set_bounds(chrome_rect(&self.window, f64::from(u16::MAX)));
        if self.glass_on {
            self.place_glass();
        }
        if let Some(v) = self.active_view() {
            let _ = v.set_bounds(self.content_rect());
        }
        let max = self.window.is_maximized();
        if max != self.maximized {
            self.maximized = max;
            self.ui_dirty = true;
        }
        self.remember_window();
    }

    fn remember_window(&mut self) {
        if self.fs != Fs::Off || self.window.is_maximized() || self.window.is_minimized() {
            return;
        }
        if let Ok(pos) = self.window.outer_position() {
            let s = self.window.inner_size();
            self.win = Some(WinState { x: pos.x, y: pos.y, w: s.width, h: s.height, maximized: false });
            self.session_dirty = true;
        }
    }

    pub fn moved(&mut self) {
        self.close_ext_popup(); // it would be left hanging where the button was
        for v in self.tabs.iter().filter_map(|t| t.view.as_ref()) {
            tab::notify_moved(v);
        }
        tab::notify_moved(&self.ui);
        if self.glass_on {
            self.place_glass();
        }
        self.remember_window();
    }

    /// Z-order of the chrome relative to the page. Normally it sits *below*: the
    /// page starts under the toolbar, so only the toolbar shows. It is raised only
    /// while the address dropdown or a panel must cover the page — by then it is
    /// already fully painted underneath, so the switch is a single clean frame.
    fn stack_ui(&self, on_top: bool) {
        let after = if on_top { HWND_TOP } else { HWND_BOTTOM };
        unsafe {
            let _ = SetWindowPos(self.ui.hwnd(), Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    fn ui_should_be_on_top(&self) -> bool {
        self.drop_up || self.panel_up
    }

    // ------------------------------------------------------------ address dropdown

    /// The dropdown covers the page, so it is drawn over a snapshot of it.
    fn open_drop(&mut self) {
        if self.drop_up || self.drop_wait || self.panel.is_some() {
            return;
        }
        self.drop_wait = true;
        match self.active_view() {
            Some(v) => tab::capture(v, &self.proxy),
            None => self.drop_ready(),
        }
    }

    fn drop_ready(&mut self) {
        self.drop_wait = false;
        self.drop_up = true;
        self.stack_ui(true);
        let _ = self.ui.evaluate_script("mb.dropShown()");
    }

    fn close_drop(&mut self) {
        self.drop_wait = false;
        if self.drop_up {
            self.drop_up = false;
            if !self.panel_up {
                self.stack_ui(false); // page back on top first...
            }
        }
        let _ = self.ui.evaluate_script("mb.dropHidden()"); // ...then the chrome tidies up unseen
    }

    pub fn restore_focus(&self) {
        if self.ui_focus {
            let _ = self.ui.focus();
        } else if let Some(v) = self.active_view() {
            let _ = v.focus();
        }
    }

    fn focus_url(&mut self) {
        if self.fs == Fs::Window {
            self.set_fs(Fs::Off);
        }
        self.ui_focus = true;
        self.flush_ui(); // the bar must show the new tab's URL before it gets focus
        let _ = self.ui.focus();
        let _ = self.ui.evaluate_script("mb.focusUrl()");
    }

    fn focus_content(&mut self) {
        self.ui_focus = false;
        if let Some(v) = self.active_view() {
            let _ = v.focus();
        }
    }

    fn set_fs(&mut self, mode: Fs) {
        if self.fs == mode {
            return;
        }
        if self.fs == Fs::Content && mode == Fs::Off {
            if let Some(v) = self.active_view() {
                let _ = v.evaluate_script("document.fullscreenElement&&document.exitFullscreen()");
            }
        }
        self.fs = mode;
        self.window.set_fullscreen((mode != Fs::Off).then_some(Fullscreen::Borderless(None)));
        let _ = self.ui.set_visible(mode == Fs::Off);
        self.layout();
    }

    // ------------------------------------------------------------ panels

    /// Opening is staged so nothing flickers:
    /// 1. snapshot the page;
    /// 2. the chrome (lying under the page) draws the panel on that snapshot;
    /// 3. once painted ("panelReady") it is raised in one step — that frame looks
    ///    exactly like the page — and then the blur animates in.
    fn open_panel(&mut self, kind: Panel) {
        if self.panel == Some(kind) || self.panel_wait == Some(kind) {
            return;
        }
        if kind == Panel::Downloads {
            self.dl_unseen = false;
            self.ui_dirty = true;
        }
        if kind == Panel::Extensions {
            ext::refresh(&self.ui, &self.exts, &self.proxy);
        }
        if self.panel.is_some() {
            self.close_panel(false);
        }
        if self.fs != Fs::Off {
            self.set_fs(Fs::Off);
        }
        self.close_drop();
        if kind == Panel::Launch && !self.home && self.active_view().is_some() && self.glass_usable() {
            self.open_glass();
            return;
        }
        self.panel_wait = Some(kind);
        match self.active_view() {
            Some(v) => tab::capture(v, &self.proxy),
            None => self.prepare_panel(String::new()),
        }
    }

    fn prepare_panel(&mut self, jpeg: String) {
        let Some(kind) = self.panel_wait.take() else { return };
        self.panel = Some(kind);
        self.ui_focus = true;
        self.flush_ui();
        let data = self.panel_data(kind);
        let bg = if jpeg.is_empty() { String::new() } else { format!("data:image/jpeg;base64,{jpeg}") };
        let home = self.home && kind == Panel::Launch;
        let _ = self.ui.evaluate_script(&format!("mb.panel('{}',{data},'{bg}',{home})", kind.name()));
    }

    /// An empty space has no tabs at all: its page is the launcher itself.
    fn show_home(&mut self) {
        if self.home && self.panel == Some(Panel::Launch) {
            return;
        }
        if self.panel.is_some() || self.panel_wait.is_some() {
            self.close_panel(false);
        }
        // Hide whatever page was showing (e.g. from the space we just left).
        if let Some(i) = self.active_idx() {
            if let Some(v) = &self.tabs[i].view {
                let _ = v.set_visible(false);
                let _ = v.set_memory_usage_level(MemoryUsageLevel::Low);
            }
            self.tabs[i].last_active = Instant::now();
        }
        self.active = 0;
        self.home = true;
        self.update_title();
        self.ui_dirty = true;
        self.session_dirty = true;
        if self.ui_ready {
            self.open_panel(Panel::Launch);
        }
    }

    fn panel_ready(&mut self) {
        if self.panel.is_none() || self.panel_up {
            return;
        }
        self.panel_up = true;
        if *tab::DEBUG {
            eprintln!("panel_ready {:?} clip={:?}", self.panel.map(Panel::name), self.ui_clip);
        }
        self.apply_ui_clip(); // before raising: the page must never be covered whole
        self.stack_ui(true);
        let _ = self.ui.focus();
        let _ = self.ui.evaluate_script("mb.panelShown()");
        if self.panel == Some(Panel::Launch) {
            self.warm_spare();
        }
    }

    // ------------------------------------------------------------ glass (live blur)

    fn glass_ui(&self) -> Option<&WebView> {
        self.glass.as_ref().and_then(|g| g.ui.as_ref())
    }

    /// The webview that draws the open panel.
    fn panel_ui(&self) -> &WebView {
        match self.glass_ui() {
            Some(ui) if self.glass_on => ui,
            _ => &self.ui,
        }
    }

    fn glass_usable(&self) -> bool {
        self.glass.as_ref().is_some_and(|g| g.ready && g.ui.is_some()) && glass::supported()
    }

    pub fn is_glass(&self, id: tao::window::WindowId) -> bool {
        self.glass.as_ref().is_some_and(|g| g.id() == id)
    }

    pub fn glass_event(&mut self, ev: &tao::event::WindowEvent) {
        use tao::event::WindowEvent;
        match ev {
            // It just lost activation (its form was in use): keep the backdrop drawn.
            WindowEvent::Focused(false) => {
                if let Some(g) = self.glass.as_ref().filter(|g| g.shown) {
                    g.keep_active_look();
                }
            }
            WindowEvent::CloseRequested => self.close_panel(true),
            _ => {}
        }
    }

    /// Lays the glass exactly over the page area.
    fn place_glass(&self) {
        let (Some(g), Ok(pos)) = (&self.glass, self.window.inner_position()) else { return };
        let size = self.window.inner_size();
        let top = ((self.chrome_h() * self.window.scale_factor()).round() as u32).min(size.height);
        g.place(pos.x, pos.y + top as i32, size.width, size.height - top);
    }

    /// Opens the Tablo in the glass: the page stays live under it. The glass
    /// page draws the Tablo while still hidden and says "panelReady".
    fn open_glass(&mut self) {
        self.panel_wait = None;
        self.panel = Some(Panel::Launch);
        self.glass_on = true;
        self.ui_focus = true;
        self.flush_ui();
        self.place_glass();
        let data = self.panel_data(Panel::Launch);
        if let Some(ui) = self.glass_ui() {
            let _ = ui.evaluate_script(&format!("mb.panel('launch',{data},'',false)"));
        }
        // The address bar keeps working as with the Tablo: same state, no panel of its own.
        let _ = self.ui.evaluate_script("mb.panel('launch',null,'',false,true)");
    }

    fn glass_ready(&mut self) {
        if self.panel != Some(Panel::Launch) || !self.glass_on {
            return;
        }
        if let Some(g) = self.glass.as_mut() {
            g.show();
        }
        let _ = self.ui.focus();
        let _ = self.ui.evaluate_script("mb.panelShown()");
        if let Some(ui) = self.glass_ui() {
            let _ = ui.evaluate_script("mb.panelShown()");
        }
        self.warm_spare();
    }

    // ------------------------------------------------------------ prerender

    /// Creates the launcher's hidden spare webview (no page yet): the next new tab
    /// skips webview creation, and a hovered tile can start loading in it.
    fn warm_spare(&mut self) {
        let private = self.active_idx().map(|i| self.tabs[i].private).unwrap_or(false);
        if self.spare.as_ref().is_some_and(|s| s.tab.private == private) {
            return;
        }
        self.spare = None;
        let id = self.alloc_id();
        let mut tab = Tab::new(id, self.space, "", private);
        tab.internal = None;
        let spec = Spec {
            id,
            private,
            source: Source::Blank,
            bounds: self.content_rect(),
            visible: false,
            env: &self.env,
            tracking: &self.settings.tracking_prevention,
        };
        match tab::build(&self.window, spec, &self.proxy, &self.block, &tab.shared, &self.dl_ops) {
            Ok(view) => {
                tab::set_muted(&view, true); // silent until it is actually shown
                tab.view = Some(view);
                tab.fresh = true;
                self.spare = Some(Spare { tab, target: String::new(), committed: false });
                self.stack_ui(self.ui_should_be_on_top());
            }
            Err(e) => eprintln!("spare webview failed: {e}"),
        }
    }

    /// The pointer rests on a Tablo tile (or presses it): start loading it.
    fn prerender(&mut self, url: &str) {
        if self.panel != Some(Panel::Launch) || !omni::is_navigable(url) {
            return;
        }
        // Another site already committed there would end up in Back: start over.
        if self.spare.as_ref().is_some_and(|s| s.committed && s.target != url) {
            self.spare = None;
        }
        self.warm_spare();
        if let Some(s) = self.spare.as_mut() {
            if *tab::DEBUG {
                eprintln!("prerender {url}");
            }
            s.go(url);
        }
    }

    /// The spare becomes a real tab at the end of the current space, showing `url`.
    fn adopt_spare(&mut self, mut s: Spare, url: &str) -> u32 {
        s.go(url);
        let mut t = s.tab;
        let id = t.id;
        t.space = self.space;
        if let Some(v) = &t.view {
            tab::set_muted(v, false);
        }
        let at = self.insert_at(None);
        self.tabs.insert(at, t);
        self.ui_focus = false;
        self.activate(id);
        self.sync_url(id);
        self.ui_dirty = true;
        self.session_dirty = true;
        id
    }

    fn spare_tab(&mut self, id: u32) -> Option<&mut Tab> {
        self.spare.as_mut().map(|s| &mut s.tab).filter(|t| t.id == id)
    }

    fn close_panel(&mut self, focus_page: bool) {
        if self.panel.is_none() && self.panel_wait.is_none() {
            return;
        }
        self.panel = None;
        self.panel_wait = None;
        self.panel_up = false;
        self.home = false;
        self.spare = None; // not adopted: the prerendered page is thrown away
        self.dl_watch = false;
        if self.glass_on {
            self.glass_on = false;
            if let Some(g) = self.glass.as_mut() {
                g.hide();
                if let Some(ui) = &g.ui {
                    let _ = ui.evaluate_script("mb.hidePanel()");
                }
            }
        }
        // Page goes on top first (instant), then the chrome hides the panel unseen.
        if !self.drop_up {
            self.stack_ui(false);
        }
        if self.ui_clip.take().is_some() {
            unsafe {
                SetWindowRgn(self.ui.hwnd(), None, true);
            }
        }
        let _ = self.ui.evaluate_script("mb.hidePanel()");
        if focus_page {
            self.focus_content();
        }
    }

    /// Dropdown panels cover only themselves (plus room for their shadow): the
    /// chrome webview is clipped to the toolbar and those rectangles, so the page
    /// keeps playing everywhere else instead of showing a frozen snapshot.
    fn apply_ui_clip(&self) {
        let Some(rects) = &self.ui_clip else { return };
        let sf = self.window.scale_factor();
        let width = self.window.inner_size().width as i32;
        let top = (self.chrome_h() * sf).round() as i32;
        unsafe {
            let rgn = CreateRectRgn(0, 0, width, top);
            for r in rects {
                let px = |v: f64| (v * sf).round() as i32;
                let part = CreateRectRgn(px(r[0]), px(r[1]), px(r[2]), px(r[3]));
                CombineRgn(Some(rgn), Some(rgn), Some(part), RGN_OR);
                let _ = DeleteObject(part.into());
            }
            SetWindowRgn(self.ui.hwnd(), Some(rgn), true); // the window owns the region now
        }
    }

    fn panel_data(&self, kind: Panel) -> Value {
        match kind {
            Panel::Launch => self.tablo_json(),
            Panel::Spaces => self.spaces_json(),
            Panel::Downloads => self.downloads_json(),
            Panel::Extensions => ext::json(&self.exts),
        }
    }

    // ------------------------------------------------------------ downloads

    fn downloads_json(&self) -> Value {
        let items: Vec<Value> = self
            .downloads
            .iter()
            .take(80)
            .map(|d| {
                let name = Path::new(&d.path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| d.url.rsplit('/').next().unwrap_or("").to_string());
                let ext = file_ext(&name);
                json!({
                    "id": d.id, "name": name, "url": d.url, "page": d.page,
                    "total": d.total, "received": d.received, "state": d.state, "paused": d.paused,
                    "exists": !d.path.is_empty() && Path::new(&d.path).exists(),
                    "auto": !ext.is_empty() && self.settings.auto_open.contains(&ext),
                    "ext": ext,
                })
            })
            .collect();
        json!({ "items": items })
    }

    // ------------------------------------------------------------ extension popups

    pub fn is_xpop(&self, id: tao::window::WindowId) -> bool {
        self.xpop.as_ref().is_some_and(|p| p.id() == id)
    }

    pub fn xpop_event(&mut self, ev: &tao::event::WindowEvent) {
        if let tao::event::WindowEvent::CloseRequested = ev {
            self.close_ext_popup();
        }
    }

    /// The toolbar buttons of extensions that have a popup.
    fn push_ext_actions(&self) {
        if self.ui_ready {
            let _ = self.ui.evaluate_script(&format!("mb.extActions({})", ext::actions(&self.exts)));
        }
    }

    fn open_ext_popup(&mut self, id: &str, x: f64, y: f64) {
        let url = ext::popup_url(&self.exts, id);
        if *tab::DEBUG {
            eprintln!("extension popup {id}: {url:?}, window {}", self.xpop.is_some());
        }
        let Some(url) = url else { return };
        // The page the extension should act on (see ext::popup_script).
        let page = self.active_idx().map(|i| self.tabs[i].url.clone()).filter(|u| u.starts_with("http")).unwrap_or_default();
        let sf = self.window.scale_factor();
        let origin = self.window.inner_position().unwrap_or_default();
        let anchor = (origin.x + (x * sf).round() as i32, origin.y + (y * sf).round() as i32);
        self.close_panel(false);
        if let Some(p) = self.xpop.as_mut() {
            p.open(&self.env, id, &url, &page, anchor, sf, &self.proxy);
        }
    }

    fn close_ext_popup(&mut self) {
        if let Some(p) = self.xpop.as_mut().filter(|p| p.is_open()) {
            self.xpop_closed = Some((p.ext.clone(), Instant::now()));
            p.close();
        }
    }

    fn extension_action(&mut self, action: &str, id: &str, flag: bool) {
        match action {
            "toggle" => ext::set_enabled(&self.exts, id, flag, &self.proxy),
            "options" => {
                if let Some(url) = ext::options_url(&self.exts, id) {
                    self.close_panel(false);
                    self.open(&url, None, false, true);
                }
            }
            "remove" => {
                let name = ext::name_of(&self.exts, id).unwrap_or_default();
                if self.confirm(&format!("Удалить расширение «{name}»?\nЕго папка будет перемещена в Корзину.")) {
                    if !ext::remove(&self.exts, id, &self.proxy) {
                        self.confirm("Не удалось переместить папку расширения в Корзину.\nУдалите её вручную, иначе оно установится снова.");
                    }
                }
            }
            "folder" => {
                let _ = std::process::Command::new("explorer").arg(ext::dir()).spawn();
            }
            _ => {}
        }
    }

    fn downloads_changed(&mut self, persist: bool) {
        if persist {
            store::save_downloads(&self.downloads);
        }
        self.ui_dirty = true;
        if self.panel == Some(Panel::Downloads) || self.dl_watch {
            let _ = self.panel_ui().evaluate_script(&format!("mb.downloadsData({})", self.downloads_json()));
        }
    }

    fn download_action(&mut self, action: &str, id: u32) {
        match action {
            "folder" => {
                let dir = self
                    .downloads
                    .iter()
                    .filter_map(|d| Path::new(&d.path).parent().filter(|p| p.is_dir()).map(Path::to_path_buf))
                    .next()
                    .or_else(|| std::env::var_os("USERPROFILE").map(|p| std::path::PathBuf::from(p).join("Downloads")));
                if let Some(d) = dir {
                    let _ = std::process::Command::new("explorer").arg(d).spawn();
                }
                return;
            }
            "clear" => {
                self.downloads.retain(|d| d.state == 0);
                self.downloads_changed(true);
                return;
            }
            _ => {}
        }
        let Some(i) = self.downloads.iter().position(|d| d.id == id) else { return };
        let (path, url, page) = {
            let d = &self.downloads[i];
            (d.path.clone(), d.url.clone(), d.page.clone())
        };
        let op = self.dl_ops.borrow().get(&id).cloned();
        match action {
            "open" => shell_open(&path),
            "show" => {
                let _ = std::process::Command::new("explorer").arg(format!("/select,{path}")).spawn();
            }
            "source" => {
                let target = if page.starts_with("http") { page } else { url };
                self.close_panel(false);
                self.open(&target, None, false, true);
            }
            "retry" => {
                self.close_panel(false);
                self.open(&url, None, false, true);
            }
            "cancel" => {
                if let Some(op) = op {
                    unsafe {
                        let _ = op.Cancel();
                    }
                }
            }
            "pause" | "resume" => {
                if let Some(op) = op {
                    let pause = action == "pause";
                    let ok = unsafe { if pause { op.Pause() } else { op.Resume() } };
                    if ok.is_ok() {
                        self.downloads[i].paused = pause;
                    }
                }
            }
            "remove" => {
                if let Some(op) = op {
                    unsafe {
                        let _ = op.Cancel();
                    }
                }
                self.downloads.remove(i);
            }
            "delete" => {
                let _ = std::fs::remove_file(&path);
            }
            "auto" => {
                let ext = file_ext(&path);
                if !ext.is_empty() {
                    if let Some(k) = self.settings.auto_open.iter().position(|e| *e == ext) {
                        self.settings.auto_open.remove(k);
                    } else {
                        self.settings.auto_open.push(ext);
                    }
                    self.settings.save();
                }
            }
            _ => return,
        }
        self.downloads_changed(true);
    }

    // ------------------------------------------------------------ tablo

    fn tablo_json(&self) -> Value {
        json!({
            "tablo": self.with_icons(pages::tablo_tiles(&self.tablo.items), true),
            "popular": self.with_icons(pages::history_sites(&self.history, 14, false), false),
            "recent": self.with_icons(pages::history_sites(&self.history, 14, true), false),
            "view": { "size": self.settings.tablo_size, "width": self.settings.tablo_width },
        })
    }

    /// After a page loads, ask it for its largest declared icon (once per site),
    /// so Tablo tiles get crisp icons straight from the site.
    fn probe_icon(&self, i: usize) {
        let t = &self.tabs[i];
        if t.private {
            return;
        }
        let Some(site) = host_of(&t.url).map(|h| site_key(h).to_ascii_lowercase()) else { return };
        if self.icons.map.contains_key(&site) {
            return;
        }
        let Some(v) = &t.view else { return };
        let proxy = self.proxy.clone();
        let _ = v.evaluate_script_with_callback(ICON_PROBE_JS, move |res| {
            let _ = proxy.send_event(UserEvent::Icon(site.clone(), res));
        });
    }

    fn refresh_tablo(&self) {
        if self.panel == Some(Panel::Launch) {
            let _ = self.panel_ui().evaluate_script(&format!("mb.tabloData({})", self.tablo_json()));
        }
    }

    /// "github.com", "https://x.y/z" or just "github" (matched against history).
    fn tablo_item(&self, input: &str, title: &str) -> Option<TabloItem> {
        let t = input.trim();
        if t.is_empty() {
            return None;
        }
        let url = omni::as_url(t)
            .or_else(|| omni::suggest(t, &[], &self.history, &self.bookmarks, &self.settings).1.map(|c| c.url))
            .or_else(|| {
                let word = t.to_lowercase();
                word.chars().all(|c| c.is_alphanumeric() || c == '-').then(|| format!("https://{word}.com/"))
            })?;
        let title = match title.trim() {
            "" => pages::site_label(host_of(&url).unwrap_or(&url)),
            s => s.chars().take(40).collect(),
        };
        Some(TabloItem { url, title })
    }

    fn tablo_changed(&mut self) {
        self.tablo.save();
        self.refresh_tablo();
    }

    fn spaces_list(&self) -> Vec<Value> {
        self.spaces
            .iter()
            .map(|s| {
                let count = self.tabs.iter().filter(|t| t.space == s.id).count();
                json!({ "id": s.id, "name": s.name, "color": s.color, "icon": s.icon, "count": count })
            })
            .collect()
    }

    fn spaces_json(&self) -> Value {
        json!({ "spaces": self.spaces_list(), "current": self.space })
    }

    fn refresh_spaces_panel(&self) {
        if self.panel == Some(Panel::Spaces) {
            let _ = self.ui.evaluate_script(&format!("mb.spacesData({})", self.spaces_json()));
        }
    }

    // ------------------------------------------------------------ spaces

    fn switch_space(&mut self, sid: u32) {
        if sid == self.space || !self.spaces.iter().any(|s| s.id == sid) {
            return;
        }
        let (cur, active) = (self.space, self.active);
        if let Some(s) = self.space_mut(cur) {
            s.active = active;
        }
        self.space = sid;
        let remembered = self.spaces.iter().find(|s| s.id == sid).map_or(0, |s| s.active);
        let vis = self.visible();
        let target = if vis.iter().any(|&i| self.tabs[i].id == remembered) {
            remembered
        } else {
            vis.first().map_or(0, |&i| self.tabs[i].id)
        };
        if target == 0 {
            self.show_home();
        } else {
            self.ui_focus = false;
            self.activate(target);
        }
        self.ui_dirty = true;
        self.session_dirty = true;
    }

    fn create_space(&mut self, name: &str, color: &str) -> u32 {
        let id = self.alloc_id();
        let n = self.spaces.len();
        let name = match name.trim() {
            "" => format!("Пространство {}", n + 1),
            s => s.chars().take(40).collect(),
        };
        let color = if color.starts_with('#') { color.to_string() } else { SPACE_COLORS[n % SPACE_COLORS.len()].into() };
        self.spaces.push(Space { id, name, color, icon: String::new(), active: 0 });
        self.session_dirty = true;
        id
    }

    fn delete_space(&mut self, sid: u32) {
        if self.spaces.len() < 2 {
            return;
        }
        let Some(pos) = self.spaces.iter().position(|s| s.id == sid) else { return };
        let count = self.tabs.iter().filter(|t| t.space == sid && t.internal != Some(Internal::NewTab)).count();
        if count > 0 {
            let text = format!("Удалить пространство «{}»?\nБудет закрыто вкладок: {count}.", self.spaces[pos].name);
            if !self.confirm(&text) {
                return;
            }
        }
        if self.space == sid {
            let other = self.spaces[if pos == 0 { 1 } else { pos - 1 }].id;
            self.switch_space(other);
        }
        self.tabs.retain(|t| t.space != sid);
        self.spaces.retain(|s| s.id != sid);
        self.ui_dirty = true;
        self.session_dirty = true;
    }

    fn move_to_space(&mut self, id: u32, sid: u32) {
        let Some(i) = self.idx(id) else { return };
        if self.tabs[i].space == sid {
            return;
        }
        let pos = self.visible().iter().position(|&g| g == i).unwrap_or(0);
        let mut t = self.tabs.remove(i);
        t.space = sid;
        t.pinned = false;
        self.tabs.push(t);
        if let Some(s) = self.space_mut(sid) {
            if s.active == 0 {
                s.active = id;
            }
        }
        if id == self.active {
            let vis = self.visible();
            match vis.get(pos.min(vis.len().saturating_sub(1))) {
                Some(&g) => {
                    let next = self.tabs[g].id;
                    self.activate(next);
                }
                None => self.show_home(),
            }
        }
        self.ui_dirty = true;
        self.session_dirty = true;
    }

    // ------------------------------------------------------------ tabs

    fn ensure_view(&mut self, idx: usize, visible: bool) {
        if self.tabs[idx].view.is_some() {
            return;
        }
        let (id, private, internal) = (self.tabs[idx].id, self.tabs[idx].private, self.tabs[idx].internal);
        let source = match internal {
            Some(k) => Source::Html(self.internal_html(k, private)),
            None => Source::Url(self.tabs[idx].url.clone()),
        };
        let spec = Spec {
            id,
            private,
            source,
            bounds: self.content_rect(),
            visible,
            env: &self.env,
            tracking: &self.settings.tracking_prevention,
        };
        match tab::build(&self.window, spec, &self.proxy, &self.block, &self.tabs[idx].shared, &self.dl_ops) {
            Ok(view) => {
                if self.tabs[idx].muted {
                    tab::set_muted(&view, true);
                }
                self.tabs[idx].view = Some(view);
                self.tabs[idx].fresh = true;
                // Re-stack explicitly: creation order alone does not settle the
                // compositor's layer order, and the chrome could show through.
                self.stack_ui(self.ui_should_be_on_top());
            }
            Err(e) => eprintln!("webview creation failed: {e}"),
        }
    }

    /// A tab's icon for the strip. Sessions saved before icons were remembered
    /// (and tabs that never loaded) fall back to the site's cached Tablo icon,
    /// the large icon learned from it, or its /favicon.ico (usually in the HTTP cache).
    fn tab_icon(&self, t: &Tab) -> String {
        if !t.icon.is_empty() || t.internal.is_some() {
            return t.icon.clone();
        }
        let Some(host) = host_of(&t.url).filter(|_| t.url.starts_with("http")) else { return String::new() };
        let site = site_key(host).to_ascii_lowercase();
        if let Some(data) = self.icon_cache.borrow_mut().peek(&site) {
            return data;
        }
        if let Some(url) = self.icons.map.get(&site).filter(|u| !u.is_empty()) {
            return url.clone();
        }
        let scheme = if t.url.starts_with("https:") { "https" } else { "http" };
        format!("{scheme}://{host}/favicon.ico")
    }

    fn newtab_html(&self, private: bool) -> String {
        pages::newtab(&self.with_icons(pages::tablo_tiles(&self.tablo.items), true), private)
    }

    /// Fills in icons: the disk-cached copy (instant) or the best known URL.
    /// With `fetch`, a missing icon is downloaded into the cache in the background.
    fn with_icons(&self, mut tiles: Vec<pages::Tile>, fetch: bool) -> Vec<pages::Tile> {
        let mut cache = self.icon_cache.borrow_mut();
        for t in &mut tiles {
            let site = site_key(&t.host).to_ascii_lowercase();
            let learned = self.icons.map.get(&site).cloned().unwrap_or_default();
            let cached = if fetch && !t.host.is_empty() {
                let is_ip = t.host.parse::<std::net::IpAddr>().is_ok() || t.host.starts_with('[');
                let scheme = if is_ip { "http" } else { "https" };
                let mut tries = Vec::new();
                if !learned.is_empty() {
                    tries.push(learned.clone());
                }
                if !is_ip {
                    tries.push(format!("https://favicon.yandex.net/favicon/v2/{}?size=120", t.host));
                }
                tries.push(format!("{scheme}://{}/apple-touch-icon.png", t.host));
                tries.push(format!("{scheme}://{}/favicon.ico", t.host));
                cache.get(&site, tries, &self.proxy)
            } else {
                cache.peek(&site)
            };
            t.icon = cached.unwrap_or(learned);
        }
        tiles
    }

    fn activate(&mut self, id: u32) {
        let Some(idx) = self.idx(id) else { return };
        if self.fs == Fs::Content {
            self.set_fs(Fs::Off);
        }
        self.close_panel(false);
        let sid = self.tabs[idx].space;
        if sid != self.space {
            // e.g. "switch to tab" from search: follow the tab into its space.
            let (cur, active) = (self.space, self.active);
            if let Some(s) = self.space_mut(cur) {
                s.active = active;
            }
            self.space = sid;
        }
        if self.active != id {
            if let Some(old) = self.active_idx() {
                let t = &mut self.tabs[old];
                t.last_active = Instant::now();
                if let Some(v) = &t.view {
                    let _ = v.set_visible(false);
                    let _ = v.set_memory_usage_level(MemoryUsageLevel::Low);
                }
            }
            self.active = id;
        }
        if let Some(s) = self.space_mut(sid) {
            s.active = id;
        }
        self.ensure_view(idx, true);
        let rect = self.content_rect();
        let t = &mut self.tabs[idx];
        t.last_active = Instant::now();
        if let Some(v) = &t.view {
            let _ = v.set_bounds(rect);
            let _ = v.set_memory_usage_level(MemoryUsageLevel::Normal);
            let _ = v.set_visible(true);
            if !self.ui_focus {
                let _ = v.focus();
            }
        }
        self.update_title();
        self.ui_dirty = true;
        self.session_dirty = true;
    }

    /// Global index for a new tab in the current space: right after `after`
    /// (but never among pinned tabs), or at the end of the space.
    fn insert_at(&self, after: Option<usize>) -> usize {
        let vis = self.visible();
        let pinned_end = vis.iter().filter(|&&g| self.tabs[g].pinned).last().map_or(0, |&g| g + 1);
        match after {
            Some(i) => (i + 1).max(pinned_end).min(self.tabs.len()),
            None => self.tabs.len(),
        }
    }

    /// Global index of the first unpinned tab of the current space.
    fn pin_boundary(&self) -> usize {
        let vis = self.visible();
        vis.iter()
            .find(|&&g| !self.tabs[g].pinned)
            .copied()
            .or_else(|| vis.last().map(|&g| g + 1))
            .unwrap_or(self.tabs.len())
    }

    fn new_tab(&mut self, private: bool) -> u32 {
        let id = self.alloc_id();
        let at = self.insert_at(None);
        self.tabs.insert(at, Tab::new(id, self.space, "", private));
        self.ui_focus = true; // the address bar gets focus, not the page
        self.activate(id);
        self.focus_url();
        id
    }

    fn open(&mut self, url: &str, after: Option<usize>, private: bool, activate: bool) -> u32 {
        let id = self.alloc_id();
        let space = after.map_or(self.space, |i| self.tabs[i].space);
        let at = self.insert_at(after);
        self.tabs.insert(at, Tab::new(id, space, url, private));
        if activate {
            self.ui_focus = false;
            self.activate(id);
        }
        self.ui_dirty = true;
        self.session_dirty = true;
        id
    }

    fn close(&mut self, id: u32) {
        let Some(idx) = self.idx(id) else { return };
        let pos = self.visible().iter().position(|&g| g == idx);
        let t = self.tabs.remove(idx);
        if !t.private && t.internal != Some(Internal::NewTab) {
            self.closed.push(SessionTab { url: t.session_url(), title: t.title.clone(), ..Default::default() });
            if self.closed.len() > 30 {
                self.closed.remove(0);
            }
        }
        let in_current = t.space == self.space;
        drop(t);
        if self.active == id {
            self.active = 0;
            let vis = self.visible();
            match vis.get(pos.unwrap_or(0).min(vis.len().saturating_sub(1))) {
                Some(&g) => {
                    let next = self.tabs[g].id;
                    self.activate(next);
                }
                None if in_current => self.show_home(),
                None => {}
            }
        }
        self.ui_dirty = true;
        self.session_dirty = true;
    }

    fn cycle(&mut self, delta: isize) {
        let vis = self.visible();
        let n = vis.len() as isize;
        if let Some(p) = vis.iter().position(|&g| self.tabs[g].id == self.active) {
            let j = ((p as isize + delta) % n + n) % n;
            let id = self.tabs[vis[j as usize]].id;
            self.activate(id);
        }
    }

    fn navigate(&mut self, url: String) {
        let Some(idx) = self.active_idx() else { return };
        let internal = Internal::from_url(&url);
        {
            let t = &mut self.tabs[idx];
            t.internal = internal;
            t.url = if internal.is_some() { String::new() } else { url.clone() };
            if internal.is_some() {
                t.title.clear();
                t.icon.clear();
            }
        }
        match &self.tabs[idx].view {
            None => self.activate(self.active), // wakes the tab with the new URL
            Some(v) => {
                let private = self.tabs[idx].private;
                let _ = match internal {
                    Some(k) => v.load_html(&self.internal_html(k, private)),
                    None => v.load_url(&url),
                };
            }
        }
        self.focus_content();
        self.ui_dirty = true;
    }

    fn reload(&mut self) {
        let Some(idx) = self.active_idx() else { return };
        if self.recover_reload(idx) {
            return;
        }
        let t = &self.tabs[idx];
        let Some(v) = &t.view else { return };
        let _ = match t.internal {
            Some(k) => v.load_html(&self.internal_html(k, t.private)),
            None => v.reload(),
        };
    }

    /// `to` is a position among the current space's tabs (without the moved one).
    fn move_tab(&mut self, id: u32, to: usize) {
        let Some(from) = self.idx(id) else { return };
        let t = self.tabs.remove(from);
        let vis = self.visible();
        let pinned = vis.iter().filter(|&&g| self.tabs[g].pinned).count();
        let to = if t.pinned { to.min(pinned) } else { to.clamp(pinned, vis.len()) };
        let at = match vis.get(to) {
            Some(&g) => g,
            None => vis.last().map_or(self.tabs.len(), |&g| g + 1),
        };
        self.tabs.insert(at, t);
        self.ui_dirty = true;
        self.session_dirty = true;
    }

    fn toggle_pin(&mut self, id: u32) {
        let Some(i) = self.idx(id) else { return };
        let mut t = self.tabs.remove(i);
        t.pinned = !t.pinned;
        let at = self.pin_boundary();
        self.tabs.insert(at, t);
        self.ui_dirty = true;
        self.session_dirty = true;
    }

    fn toggle_mute(&mut self, id: u32) {
        let Some(i) = self.idx(id) else { return };
        let t = &mut self.tabs[i];
        t.muted = !t.muted;
        if let Some(v) = &t.view {
            tab::set_muted(v, t.muted);
        }
        self.ui_dirty = true;
    }

    fn sleep_tab(&mut self, i: usize) {
        let t = &mut self.tabs[i];
        if t.id != self.active && t.view.is_some() {
            t.view = None;
            t.loading = false;
            t.audio = false;
            self.ui_dirty = true;
        }
    }

    fn toggle_bookmark(&mut self) {
        let Some(i) = self.active_idx() else { return };
        let t = &self.tabs[i];
        if t.internal.is_none() && !t.url.is_empty() {
            self.bookmarks.toggle(&t.url, &t.display_title());
            self.ui_dirty = true;
        }
    }

    fn toggle_shield(&mut self) {
        let Some(i) = self.active_idx() else { return };
        let Some(host) = host_of(&self.tabs[i].url).map(|h| site_key(h).to_ascii_lowercase()) else { return };
        let now_off = {
            let mut ex = self.block.exceptions.borrow_mut();
            // Turning protection back on removes every exception that covers this
            // site, a parent domain's too (switched off on youtube.com, now on
            // consent.youtube.com: the shield is grey here and must turn green).
            let covering: Vec<String> =
                ex.iter().filter(|e| host == **e || host.ends_with(&format!(".{e}"))).cloned().collect();
            if covering.is_empty() {
                ex.insert(host.clone());
                true
            } else {
                for e in covering {
                    ex.remove(&e);
                }
                false
            }
        };
        self.settings.shield_off = self.block.exceptions.borrow().iter().cloned().collect();
        self.settings.save();
        self.tabs[i].shared.bypass.set(now_off);
        if let Some(v) = &self.tabs[i].view {
            let _ = v.reload();
        }
        self.ui_dirty = true;
    }

    // ------------------------------------------------------------ ad blocker updates

    fn adblock_config(&self) -> blocker::Config {
        blocker::Config {
            cookies: self.settings.adblock_cookies,
            annoyances: self.settings.adblock_annoyances,
            extra: self.settings.adblock_lists.clone(),
        }
    }

    /// Starts a background update when lists are old, settings or blocklist.txt
    /// changed, or the user asked (`force`).
    fn adblock_check(&mut self, force: bool) {
        if self.ab_busy || (!force && Instant::now() < self.ab_next_try) {
            return;
        }
        let cfg = self.adblock_config();
        let download = match (force, blocker::due(&cfg)) {
            (true, _) | (_, blocker::Due::Download) => true,
            (_, blocker::Due::Rebuild) => false,
            (_, blocker::Due::Nothing) => return,
        };
        self.ab_busy = true;
        self.ab_next_try = Instant::now() + Duration::from_secs(60);
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let ev = match blocker::update(&cfg, download) {
                Ok(engine) => UserEvent::Adblock(tab::EngineBox(Box::new(engine))),
                Err(e) => UserEvent::AdblockFailed(e),
            };
            let _ = p.send_event(ev);
        });
        if download {
            self.adblock_resources();
        }
    }

    /// uBO's scriptlets are ES modules: a hidden page without any privileges
    /// imports them and posts back adblock-rust resources (see blocker.rs).
    fn adblock_resources(&mut self) {
        if self.ab_res_view.is_some() {
            return;
        }
        let p = self.proxy.clone();
        let view = WebViewBuilder::new()
            .with_environment(self.env.clone())
            .with_visible(false)
            .with_bounds(Rect { position: PhysicalPosition::new(0, 0).into(), size: PhysicalSize::new(1, 1).into() })
            .with_html(blocker::RESOURCES_PAGE)
            .with_navigation_handler(|url| url.starts_with("about:") || url.starts_with("data:"))
            .with_ipc_handler(move |req| {
                let _ = p.send_event(UserEvent::AdblockResources(req.into_body()));
            })
            .build_as_child(&self.window);
        match view {
            Ok(v) => self.ab_res_view = Some((v, Instant::now())),
            Err(e) => eprintln!("adblock resources page: {e}"),
        }
    }

    fn adblock_status(&self) -> String {
        if self.ab_busy {
            return "Фильтры обновляются…".into();
        }
        if !self.ab_error.is_empty() {
            return "Обновить фильтры (не удалось)".into();
        }
        match blocker::last_fetched() {
            0 => "Скачать фильтры".into(),
            t => {
                let days = store::now().saturating_sub(t) / 86_400;
                match days {
                    0 => "Обновить фильтры (свежие)".into(),
                    1 => "Обновить фильтры (вчера)".into(),
                    d => format!("Обновить фильтры ({d} дн. назад)"),
                }
            }
        }
    }

    /// Lists, settings or blocklist.txt changed: recompile soon.
    fn adblock_changed(&mut self) {
        self.settings.save();
        self.ab_next_try = Instant::now();
        self.adblock_check(false);
    }

    // ------------------------------------------------------------ page features

    /// New filter lists: open pages get their site's new hiding rules right away
    /// (single-page sites like YouTube may never reload on their own).
    fn push_cosmetics(&self) {
        let spare = self.spare.as_ref().map(|s| &s.tab);
        for t in self.tabs.iter().chain(spare) {
            let Some(view) = &t.view else { continue };
            if !self.block.enabled.get() || t.shared.bypass.get() || t.internal.is_some() {
                continue;
            }
            let Some(c) = self.block.cosmetic(&t.url) else { continue };
            *t.shared.cos_exceptions.borrow_mut() = c.exceptions.clone();
            if let Ok(mut msg) = serde_json::from_str::<Value>(&c.json) {
                msg["mb"] = json!("hide");
                tab::post_json(view, &msg.to_string());
            }
        }
    }

    fn view_of(&self, id: u32) -> Option<&WebView> {
        match self.idx(id) {
            Some(i) => self.tabs[i].view.as_ref(),
            None => self.spare.as_ref().filter(|s| s.tab.id == id).and_then(|s| s.tab.view.as_ref()),
        }
    }

    /// A message from a page's content script.
    fn page_message(&mut self, id: u32, v: Value) {
        match v["mb"].as_str() {
            Some("lang") => {
                let letters = v["letters"].as_u64().unwrap_or(0);
                let cyr = v["cyr"].as_u64().unwrap_or(0);
                let lang = v["l"].as_str().unwrap_or("");
                let target = self.settings.translate_to.clone();
                let foreign = if letters >= 200 {
                    // Mostly non-Cyrillic text (for a Russian target), or a declared other language.
                    if target == "ru" { cyr * 10 < letters * 3 } else { !lang.is_empty() && !lang.starts_with(&target) }
                } else {
                    !lang.is_empty() && !lang.starts_with(&target)
                };
                let active = self.active;
                if let Some(i) = self.idx(id) {
                    if self.tabs[i].foreign != foreign {
                        self.tabs[i].foreign = foreign;
                        self.ui_dirty |= id == active;
                    }
                } else if let Some(t) = self.spare_tab(id) {
                    t.foreign = foreign;
                }
            }
            Some("sb") => {
                let Some(p) = v["p"].as_str().filter(|p| p.len() == 4 && p.bytes().all(|b| b.is_ascii_hexdigit()))
                else {
                    return;
                };
                if !self.settings.sponsorblock {
                    return;
                }
                if let Some(body) = self.sb_cache.get(p) {
                    let msg = json!({ "mb": "sb", "p": p, "d": body }).to_string();
                    if let Some(view) = self.view_of(id) {
                        tab::post_json(view, &msg);
                    }
                    return;
                }
                let cats = serde_json::to_string(&self.settings.sponsorblock_categories).unwrap_or_default();
                let url = format!(
                    "https://sponsor.ajay.app/api/skipSegments/{p}?categories={}&actionTypes={}",
                    net::form_encode(&cats),
                    net::form_encode(r#"["skip","mute"]"#)
                );
                let (p, prefix) = (self.proxy.clone(), p.to_ascii_lowercase());
                std::thread::spawn(move || {
                    let body = match net::get(&url) {
                        Ok(r) if r.ok() && serde_json::from_slice::<Value>(&r.body).is_ok() => r.text(),
                        Ok(r) if r.status == 404 => "[]".to_string(), // no segments for these videos
                        _ => return,
                    };
                    let _ = p.send_event(UserEvent::SponsorBlock(id, prefix, body));
                });
            }
            Some("int") => self.internal_message(id, &v),
            Some("tr") => {
                let to =self.idx(id).map(|i| self.tabs[i].shared.translate.borrow().clone()).unwrap_or_default();
                let Some(rid) = v["id"].as_u64() else { return };
                if to.is_empty() || !crate::translate::valid_lang(&to) {
                    return;
                }
                let q: Vec<String> =
                    v["q"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                if q.is_empty() || q.len() > 64 || q.iter().map(String::len).sum::<usize>() > 40_000 {
                    return;
                }
                let p = self.proxy.clone();
                std::thread::spawn(move || {
                    let msg = match crate::translate::translate(&q, &to) {
                        Ok(r) => json!({ "mb": "tr", "id": rid, "r": r }),
                        Err(e) => json!({ "mb": "tr", "id": rid, "err": e }),
                    };
                    let _ = p.send_event(UserEvent::PageReply(id, msg.to_string()));
                });
            }
            _ => {}
        }
    }

    /// Translate the page (and the following pages of this tab), or show the original.
    fn toggle_translate(&mut self) {
        let Some(i) = self.active_idx() else { return };
        let t = &self.tabs[i];
        if t.internal.is_some() || host_of(&t.url).is_none() {
            return;
        }
        let on = t.shared.translate.borrow().is_empty();
        let to = if on { self.settings.translate_to.clone() } else { String::new() };
        *t.shared.translate.borrow_mut() = to.clone();
        if let Some(v) = &t.view {
            tab::post_json(v, &json!({ "mb": "tr", "on": on, "to": to }).to_string());
        }
        self.ui_dirty = true;
    }

    fn open_history(&mut self) {
        let existing = self
            .tabs
            .iter()
            .find(|t| t.space == self.space && t.internal == Some(Internal::History))
            .map(|t| t.id);
        if let Some(id) = existing {
            self.activate(id);
            self.reload();
        } else {
            let after = self.active_idx();
            self.open(HISTORY_URL, after, false, true);
        }
    }

    /// HTML of a built-in page.
    fn internal_html(&self, k: Internal, private: bool) -> String {
        match k {
            Internal::NewTab => self.newtab_html(private),
            Internal::History => pages::history(&self.history),
            Internal::Bookmarks => {
                let tiles = self
                    .bookmarks
                    .items
                    .iter()
                    .map(|b| pages::Tile {
                        url: b.url.clone(),
                        title: b.title.clone(),
                        host: host_of(&b.url).unwrap_or_default().to_string(),
                        icon: String::new(),
                    })
                    .collect();
                internal::bookmarks_page(&self.bookmarks.items, &self.with_icons(tiles, false), &self.int_token)
            }
            Internal::Settings => internal::settings_page(&self.settings, &self.int_token),
        }
    }

    /// Opens bookmarks / settings: switches to the open tab (refreshed) or makes a new one.
    fn open_internal(&mut self, k: Internal) {
        let existing = self.tabs.iter().find(|t| t.space == self.space && t.internal == Some(k)).map(|t| t.id);
        if let Some(id) = existing {
            self.activate(id);
            self.reload();
        } else {
            let after = self.active_idx();
            self.open(k.url(), after, false, true);
        }
    }

    /// A message from the bookmarks / settings page. Gate: the tab shows one of those
    /// pages right now, the message carries this run's random token (only the page's own
    /// HTML has it) and its document is not a web page (`src` is added by tab.rs).
    fn internal_message(&mut self, id: u32, v: &Value) {
        let Some(i) = self.idx(id) else { return };
        let kind = match self.tabs[i].internal {
            Some(k @ (Internal::Bookmarks | Internal::Settings)) => k,
            _ => return,
        };
        let from_web = v["src"].as_str().is_none_or(|s| s.to_ascii_lowercase().starts_with("http"));
        if from_web || v["t"].as_str() != Some(self.int_token.as_str()) {
            return;
        }
        let a = v["a"].as_str().unwrap_or("");
        match (kind, a) {
            (Internal::Bookmarks, a) if a.starts_with("bm_") => {
                if internal::bookmark_op(&mut self.bookmarks, a, v) {
                    self.bookmarks.save();
                    self.ui_dirty = true;
                }
            }
            (Internal::Settings, "set") => {
                let ok = match internal::set_setting(&mut self.settings, v["k"].as_str().unwrap_or(""), &v["v"]) {
                    Some(effect) => {
                        self.settings.save();
                        self.apply_setting(effect);
                        true
                    }
                    None => false,
                };
                let msg = json!({ "mb": "int", "ok": ok, "s": internal::settings_json(&self.settings) });
                if let Some(view) = self.view_of(id) {
                    tab::post_json(view, &msg.to_string());
                }
            }
            (Internal::Settings, "adblock_update") => {
                self.ab_error.clear();
                self.adblock_check(true);
            }
            (Internal::Settings, "open_data") => {
                let _ = std::process::Command::new("explorer").arg(store::data_dir()).spawn();
            }
            (Internal::Settings, "open_bookmarks") => self.open_internal(Internal::Bookmarks),
            _ => {}
        }
    }

    /// Side effects of a changed setting (the value is already stored and saved).
    fn apply_setting(&mut self, effect: internal::Effect) {
        use internal::Effect;
        match effect {
            Effect::None | Effect::Tablo => {
                if effect == Effect::Tablo {
                    self.refresh_tablo();
                }
            }
            Effect::AdblockSwitch => self.block.enabled.set(self.settings.adblock),
            Effect::AdblockRules => self.adblock_changed(),
            Effect::SponsorSwitch => self.block.sponsorblock.set(self.settings.sponsorblock),
            Effect::SponsorCategories => self.sb_cache.clear(),
            Effect::Tracking => {
                let spare = self.spare.as_ref().map(|s| &s.tab);
                for view in self.tabs.iter().chain(spare).filter_map(|t| t.view.as_ref()) {
                    tab::set_tracking(view, &self.settings.tracking_prevention);
                }
            }
        }
    }

    fn update_title(&self) {
        match self.active_idx() {
            Some(i) => self.window.set_title(&format!("{} - {}", self.tabs[i].display_title(), crate::APP_NAME)),
            None => self.window.set_title(crate::APP_NAME),
        }
    }

    // ------------------------------------------------------------ events

    pub fn handle(&mut self, ev: UserEvent) {
        match ev {
            UserEvent::Title(id, title) => {
                if let Some(i) = self.idx(id) {
                    let t = &mut self.tabs[i];
                    t.title = title;
                    if !t.private && t.internal.is_none() {
                        self.history.set_title(&t.url, &t.title);
                    }
                    if id == self.active {
                        self.update_title();
                    }
                    self.ui_dirty = true;
                    self.session_dirty = true;
                } else if let Some(t) = self.spare_tab(id) {
                    t.title = title;
                }
            }
            UserEvent::Url(id) => {
                if let Some(s) = self.spare.as_mut().filter(|s| s.tab.id == id) {
                    let url = s.tab.view.as_ref().and_then(|v| v.url().ok()).unwrap_or_default();
                    s.committed |= url.starts_with("http");
                } else {
                    self.sync_url(id);
                }
            }
            UserEvent::Load(id, started) => {
                if let Some(i) = self.idx(id) {
                    self.tabs[i].loading = started;
                    if !started {
                        let t = &mut self.tabs[i];
                        if t.fresh {
                            t.fresh = false;
                            if let Some(v) = &t.view {
                                let _ = v.set_background_color((255, 255, 255, 255));
                            }
                        }
                        self.sync_url(id);
                        if let Some(i) = self.idx(id) {
                            self.probe_icon(i);
                        }
                    }
                    self.ui_dirty = true;
                } else if let Some(t) = self.spare_tab(id) {
                    t.loading = started;
                    if !started && t.fresh {
                        t.fresh = false;
                        if let Some(v) = &t.view {
                            let _ = v.set_background_color((255, 255, 255, 255));
                        }
                    }
                }
            }
            UserEvent::Nav(id) => self.ui_dirty |= id == self.active,
            UserEvent::Favicon(id, url) => {
                if let Some(i) = self.idx(id) {
                    self.tabs[i].icon = url;
                    self.ui_dirty = true;
                } else if let Some(t) = self.spare_tab(id) {
                    t.icon = url;
                }
            }
            UserEvent::Audio(id, playing) => {
                if let Some(i) = self.idx(id) {
                    self.tabs[i].audio = playing;
                    self.ui_dirty = true;
                }
            }
            UserEvent::Fullscreen(id, on) => {
                if id == self.active {
                    match (on, self.fs) {
                        (true, Fs::Off) => self.set_fs(Fs::Content),
                        (false, Fs::Content) => self.set_fs(Fs::Off),
                        _ => {}
                    }
                }
            }
            UserEvent::OpenTab { url, opener } => {
                let (after, private) = match self.idx(opener) {
                    Some(i) => (Some(i), self.tabs[i].private),
                    None => (None, false),
                };
                self.open(&url, after, private, true);
            }
            UserEvent::External(arg) => {
                if let Some(url) = omni::resolve(&arg, &self.settings) {
                    self.handle(UserEvent::OpenTab { url, opener: 0 });
                }
                crate::shell::raise(self.hwnd());
            }
            UserEvent::Key(cmd) => self.exec(cmd),
            UserEvent::Blocked(id) => {
                if let Some(i) = self.idx(id) {
                    self.tabs[i].shared.notify_pending.set(false);
                    self.ui_dirty |= id == self.active;
                } else if let Some(t) = self.spare_tab(id) {
                    t.shared.notify_pending.set(false);
                }
            }
            UserEvent::Ipc(raw) => {
                if let Ok(msg) = serde_json::from_str::<Msg>(&raw) {
                    self.ipc(msg);
                }
            }
            UserEvent::GlassIpc(raw) => {
                if let Ok(msg) = serde_json::from_str::<Msg>(&raw) {
                    self.from_glass = true;
                    self.ipc(msg);
                    self.from_glass = false;
                }
            }
            UserEvent::WarmGlass => {
                if let Some(g) = self.glass.as_mut() {
                    g.create_ui(&self.env, UI_HTML, &self.proxy);
                }
            }
            UserEvent::DlStart { id, url, path, total, page, private } => {
                self.downloads.insert(
                    0,
                    DownloadItem { id, url, page, path, total, state: 0, time: store::now(), private, ..Default::default() },
                );
                self.downloads_changed(false);
            }
            UserEvent::Recover(ev) => self.recover_event(ev),
            UserEvent::DlProgress(id, got, all) => {
                if let Some(d) = self.downloads.iter_mut().find(|d| d.id == id) {
                    d.received = got;
                    d.total = all;
                }
                self.downloads_changed(false);
            }
            UserEvent::DlState(id, state, got, all, path) => {
                let mut open_now = None;
                if let Some(d) = self.downloads.iter_mut().find(|d| d.id == id) {
                    d.state = state;
                    d.received = got;
                    d.total = all;
                    d.paused = false;
                    if !path.is_empty() {
                        d.path = path;
                    }
                    let ext = file_ext(&d.path);
                    if state == 1 && !ext.is_empty() && self.settings.auto_open.contains(&ext) {
                        open_now = Some(d.path.clone());
                    }
                }
                if state == 1 && self.panel != Some(Panel::Downloads) && !self.dl_watch {
                    self.dl_unseen = true;
                }
                if let Some(p) = open_now {
                    shell_open(&p);
                }
                self.downloads_changed(true);
            }
            UserEvent::IconCached => self.refresh_tablo(),
            UserEvent::Extensions => {
                if self.panel == Some(Panel::Extensions) {
                    let _ = self.ui.evaluate_script(&format!("mb.extData({})", ext::json(&self.exts)));
                }
                self.push_ext_actions();
            }
            // Clicking into the page closes a dropdown panel, as clicking beside it did
            // (the page shows live around it, so the click lands on the page).
            UserEvent::PageFocus(id) => {
                if id == self.active && self.panel_up && matches!(self.panel, Some(Panel::Downloads | Panel::Extensions)) {
                    self.close_panel(false);
                    self.ui_focus = false;
                }
            }
            UserEvent::PopupTick(g) => {
                if let Some(p) = self.xpop.as_mut().filter(|p| p.generation() == g) {
                    p.tick(&self.proxy);
                }
            }
            UserEvent::PopupSize(g, res) => {
                if let Some(p) = self.xpop.as_mut().filter(|p| p.generation() == g) {
                    if p.apply(&res) {
                        self.close_ext_popup();
                    }
                }
            }
            UserEvent::PopupFocus(g) => {
                if let Some(p) = self.xpop.as_mut().filter(|p| p.generation() == g) {
                    p.got_focus();
                }
            }
            UserEvent::PopupBlur(g) => {
                if self.xpop.as_ref().is_some_and(|p| p.generation() == g && p.settled()) {
                    self.close_ext_popup();
                }
            }
            UserEvent::Page(id, v) => self.page_message(id, v),
            UserEvent::PageReply(id, json) => {
                if let Some(v) = self.view_of(id) {
                    tab::post_json(v, &json);
                }
            }
            UserEvent::SponsorBlock(id, prefix, body) => {
                let msg = json!({ "mb": "sb", "p": prefix, "d": body }).to_string();
                self.sb_cache.insert(prefix, body);
                if let Some(v) = self.view_of(id) {
                    tab::post_json(v, &msg);
                }
            }
            UserEvent::Adblock(engine) => {
                self.ab_busy = false;
                self.ab_error.clear();
                self.block.set_engine(*engine.0);
                self.push_cosmetics();
                if *tab::DEBUG {
                    eprintln!("adblock: new engine in use");
                }
            }
            UserEvent::AdblockFailed(err) => {
                self.ab_busy = false;
                eprintln!("adblock: {err}");
                self.ab_error = err;
                self.ab_next_try = Instant::now() + Duration::from_secs(30 * 60);
            }
            UserEvent::AdblockCheck => self.adblock_check(false),
            UserEvent::ExtInstalled(path) => ext::add(&self.ui, &path, &self.exts, &self.proxy),
            UserEvent::AdblockResources(raw) => {
                let v = serde_json::from_str::<Value>(&raw).unwrap_or_default();
                if let Some(e) = v["error"].as_str() {
                    eprintln!("adblock resources: {e}");
                }
                if let Some(r) = v.get("resources") {
                    self.ab_res_view = None;
                    if let Some(list) = blocker::save_resources(&r.to_string()) {
                        let n = list.len();
                        self.block.with_engine(|e| e.use_resources(list));
                        if *tab::DEBUG {
                            eprintln!("adblock: {n} scriptlet/redirect resources");
                        }
                    }
                }
            }
            UserEvent::Icon(site, res) => {
                let url: String = serde_json::from_str(&res).unwrap_or_default();
                let url = if url.starts_with("https://") || url.starts_with("http://") { url } else { String::new() };
                let found = !url.is_empty();
                if found {
                    // The site's own icon beats the cached fallback: fetch it again.
                    self.icon_cache.borrow_mut().invalidate(&site);
                }
                self.icons.set(site, url);
                if found {
                    self.refresh_tablo();
                }
            }
            UserEvent::Preview(jpeg) => {
                if self.panel_wait.is_some() && self.panel.is_none() {
                    self.prepare_panel(jpeg);
                } else if self.drop_wait {
                    let _ = self.ui.evaluate_script(&format!("mb.backdrop('data:image/jpeg;base64,{jpeg}')"));
                }
            }
        }
    }

    fn sync_url(&mut self, id: u32) {
        let Some(i) = self.idx(id) else { return };
        let t = &mut self.tabs[i];
        let Some(Ok(url)) = t.view.as_ref().map(|v| v.url()) else { return };
        if url == "about:blank" || url.is_empty() {
            return;
        }
        if t.url != url {
            t.internal = None;
            t.url = url;
            if !t.private && t.url.starts_with("http") {
                self.history.visit(&t.url, &t.title);
            }
            self.session_dirty = true;
            self.ui_dirty = true;
        }
    }

    fn exec(&mut self, cmd: Cmd) {
        if *tab::DEBUG {
            eprintln!("exec {cmd:?}");
        }
        match cmd {
            Cmd::NewTab => {
                if self.home {
                    let _ = self.ui.focus();
                    let _ = self.ui.evaluate_script("mb.focusLaunch()");
                } else {
                    self.open_panel(Panel::Launch);
                }
            }
            Cmd::NewPrivate => {
                self.close_panel(false);
                self.new_tab(true);
            }
            Cmd::Close => self.close(self.active),
            Cmd::Reopen => {
                if let Some(st) = self.closed.pop() {
                    let after = self.active_idx();
                    self.open(&st.url, after, false, true);
                }
            }
            Cmd::Next => self.cycle(1),
            Cmd::Prev => self.cycle(-1),
            Cmd::Nth(n) => {
                if let Some(&g) = self.visible().get(n) {
                    let id = self.tabs[g].id;
                    self.activate(id);
                }
            }
            Cmd::Last => {
                if let Some(&g) = self.visible().last() {
                    let id = self.tabs[g].id;
                    self.activate(id);
                }
            }
            Cmd::FocusUrl => {
                self.close_panel(false);
                self.focus_url();
            }
            Cmd::Reload => self.reload(),
            Cmd::Back => {
                if let Some(v) = self.active_view() {
                    let _ = v.go_back();
                }
            }
            Cmd::Forward => {
                if let Some(v) = self.active_view() {
                    let _ = v.go_forward();
                }
            }
            Cmd::Bookmark => self.toggle_bookmark(),
            Cmd::Fullscreen => {
                self.close_panel(false);
                self.set_fs(if self.fs == Fs::Off { Fs::Window } else { Fs::Off });
            }
            Cmd::History => self.open_history(),
            Cmd::Bookmarks => self.open_internal(Internal::Bookmarks),
            Cmd::Mute => self.toggle_mute(self.active),
            Cmd::Space(n) => {
                if let Some(sid) = self.spaces.get(n).map(|s| s.id) {
                    self.close_panel(false);
                    self.switch_space(sid);
                }
            }
        }
    }

    fn ipc(&mut self, m: Msg) {
        match m.cmd.as_str() {
            "ready" if self.from_glass => {
                if let Some(g) = self.glass.as_mut() {
                    g.ready = true;
                }
            }
            "ready" => {
                if self.glass.is_some() {
                    // Load the glass page once the browser is idle, not during startup.
                    let p = self.proxy.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        let _ = p.send_event(UserEvent::WarmGlass);
                    });
                }
                if !self.ui_ready {
                    // Filter lists: checked once startup is over.
                    let p = self.proxy.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(5));
                        let _ = p.send_event(UserEvent::AdblockCheck);
                    });
                }
                self.ui_ready = true;
                self.ui_dirty = true;
                self.push_ext_actions();
                if self.active == 0 {
                    self.home = false;
                    self.show_home();
                } else if self.ui_focus {
                    self.focus_url();
                }
            }
            "new" => {
                self.close_panel(false);
                self.new_tab(m.flag);
            }
            "close" => self.close(m.id),
            "activate" => {
                self.ui_focus = false;
                self.activate(m.id);
            }
            "panel" => {
                if m.flag {
                    self.open_panel(Panel::parse(&m.text));
                } else {
                    self.close_panel(true);
                    if self.active == 0 {
                        self.show_home(); // an empty space goes back to its home
                    }
                }
            }
            "panelReady" if self.glass_on => {
                if self.from_glass {
                    self.glass_ready();
                }
            }
            "panelReady" => self.panel_ready(),
            // The address bar's suggestions, drawn in the glass over the Tablo.
            "gdrop" => {
                if let (Some(ui), Ok(v)) = (self.glass_ui(), serde_json::from_str::<Value>(&m.text)) {
                    let _ = ui.evaluate_script(&format!("mb.gdrop({v})"));
                }
            }
            "glassDl" => {
                if let Some(ui) = self.glass_ui() {
                    let _ = ui.evaluate_script("mb.toggleDl()");
                }
            }
            // An input in the Tablo (add/edit form) wants the keyboard, or is done.
            "glassFocus" if self.glass_on => {
                if m.flag {
                    if let Some(g) = &self.glass {
                        g.take_focus();
                    }
                } else {
                    self.window.set_focus();
                    let _ = self.ui.focus();
                    let _ = self.ui.evaluate_script("mb.focusLaunch()");
                }
            }
            "spaceGo" => {
                self.close_panel(false);
                self.switch_space(m.id);
            }
            "spaceNew" => {
                self.close_panel(false);
                let color = m.url.unwrap_or_default();
                let sid = self.create_space(&m.text, &color);
                self.switch_space(sid);
            }
            "spaceRename" => {
                if let Some(s) = self.space_mut(m.id) {
                    let name: String = m.text.trim().chars().take(40).collect();
                    if !name.is_empty() {
                        s.name = name;
                    }
                }
                self.ui_dirty = true;
                self.session_dirty = true;
                self.refresh_spaces_panel();
            }
            "spaceIcon" => {
                // One emoji (possibly with modifiers) or "" to show the tab count again.
                let icon = m.text.trim();
                if let Some(s) = self.space_mut(m.id) {
                    if icon.chars().count() <= 8 {
                        s.icon = icon.to_string();
                    }
                }
                self.ui_dirty = true;
                self.session_dirty = true;
                self.refresh_spaces_panel();
            }
            "spaceColor" => {
                if let Some(s) = self.space_mut(m.id) {
                    if m.text.starts_with('#') && m.text.len() <= 9 {
                        s.color = m.text.clone();
                    }
                }
                self.ui_dirty = true;
                self.session_dirty = true;
                self.refresh_spaces_panel();
            }
            "dl" => self.download_action(&m.text, m.id),
            "clip" => {
                if matches!(self.panel, Some(Panel::Downloads | Panel::Extensions)) && !self.from_glass {
                    if let Ok(rects) = serde_json::from_str::<Vec<[f64; 4]>>(&m.text) {
                        self.ui_clip = Some(rects);
                        if self.panel_up {
                            self.apply_ui_clip();
                        }
                    }
                }
            }
            "ext" => self.extension_action(&m.text, m.url.as_deref().unwrap_or_default(), m.flag),
            // An extension's toolbar button: x = its right edge, y = the toolbar's bottom.
            "extPopup" => {
                let id = m.url.unwrap_or_default();
                let just_closed =
                    self.xpop_closed.take().is_some_and(|(e, at)| e == id && at.elapsed() < Duration::from_millis(350));
                if !just_closed {
                    self.open_ext_popup(&id, m.x, m.y);
                }
            }
            "dlWatch" => {
                // The downloads list opened (closed) as a layer over the Tablo.
                self.dl_watch = m.flag && self.panel.is_some();
                if self.dl_watch {
                    self.dl_unseen = false;
                    self.ui_dirty = true;
                    let _ = self.panel_ui().evaluate_script(&format!("mb.downloadsData({})", self.downloads_json()));
                }
            }
            "prerender" => {
                if let Some(u) = m.url {
                    self.prerender(&u);
                }
            }
            "tabToSpace" => self.move_to_space(m.id, m.num as u32),
            "tabloView" => {
                // "size:small" | "width:auto" ...
                if let Some((key, val)) = m.text.split_once(':') {
                    match (key, val) {
                        ("size", "small" | "medium" | "large") => self.settings.tablo_size = val.into(),
                        ("width", "auto" | "small" | "medium" | "large") => self.settings.tablo_width = val.into(),
                        _ => {}
                    }
                    self.settings.save();
                }
                self.refresh_tablo();
            }
            "tabloAdd" => {
                if let Some(item) = self.tablo_item(&m.text, &m.title) {
                    self.tablo.items.retain(|t| t.url != item.url);
                    self.tablo.items.push(item);
                }
                self.tablo_changed();
            }
            "tabloEdit" => {
                let i = m.num as usize;
                if i < self.tablo.items.len() {
                    if let Some(item) = self.tablo_item(&m.text, &m.title) {
                        self.tablo.items[i] = item;
                    }
                }
                self.tablo_changed();
            }
            "tabloDel" => {
                let i = m.num as usize;
                if i < self.tablo.items.len() {
                    self.tablo.items.remove(i);
                }
                self.tablo_changed();
            }
            "tabloMove" => {
                let (from, to) = (m.num as usize, m.id as usize);
                if from < self.tablo.items.len() {
                    let item = self.tablo.items.remove(from);
                    let to = to.min(self.tablo.items.len());
                    self.tablo.items.insert(to, item);
                }
                self.tablo_changed();
            }
            "spaceDel" => {
                self.delete_space(m.id);
                self.refresh_spaces_panel();
            }
            "drag" => {
                let _ = self.window.drag_window();
            }
            "resize" => {
                let dir = match m.text.as_str() {
                    "nw" => ResizeDirection::NorthWest,
                    "ne" => ResizeDirection::NorthEast,
                    _ => ResizeDirection::North,
                };
                let _ = self.window.drag_resize_window(dir);
            }
            "min" => self.window.set_minimized(true),
            "max" => self.window.set_maximized(!self.window.is_maximized()),
            "quit" => unsafe {
                // Same path as Alt+F4, so the session gets saved.
                let _ = PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            "go" => {
                let from_launcher = self.panel.is_some();
                // Typing in the address bar while the Tablo is open means "new tab".
                let tablo_open = self.panel == Some(Panel::Launch);
                // Keep the prerender webview alive past close_panel: it may become the tab.
                let spare = if tablo_open { self.spare.take() } else { None };
                self.close_panel(false);
                // Picked suggestion → its URL; typed text → resolve (URL or search).
                let target = match m.url {
                    Some(u) if omni::is_navigable(&u) || Internal::from_url(&u).is_some() => Some(u),
                    _ if Internal::from_url(m.text.trim()).is_some() => Some(m.text.trim().to_string()),
                    _ => omni::resolve(&m.text, &self.settings),
                };
                if let Some(url) = target {
                    // An empty space has no "current tab" to navigate: always a new one.
                    if m.flag || tablo_open || self.active_idx().is_none() {
                        // From the launcher a new tab goes to the end, like Ctrl+T.
                        let after = if from_launcher { None } else { self.active_idx() };
                        let private = self.active_idx().map(|i| self.tabs[i].private).unwrap_or(false);
                        match spare {
                            Some(s) if s.tab.private == private && (s.target == url || !s.committed) => {
                                self.adopt_spare(s, &url);
                            }
                            _ => {
                                self.open(&url, after, private, true);
                            }
                        }
                    } else {
                        self.navigate(url);
                    }
                }
                if self.active == 0 {
                    self.show_home(); // nothing was opened: keep the empty space's home
                }
            }
            "back" => self.exec(Cmd::Back),
            "fwd" => self.exec(Cmd::Forward),
            "reload" => self.reload(),
            "stop" => {
                if let Some(v) = self.active_view() {
                    tab::stop(v);
                }
            }
            "star" => self.toggle_bookmark(),
            "shield" => self.toggle_shield(),
            "translate" => self.toggle_translate(),
            "mute" => self.toggle_mute(m.id),
            "move" => self.move_tab(m.id, m.num as usize),
            "suggest" => {
                let tabs: Vec<omni::TabInfo> = self
                    .tabs
                    .iter()
                    .filter(|t| t.id != self.active && t.internal.is_none())
                    .map(|t| omni::TabInfo { id: t.id, title: &t.title, url: &t.url })
                    .collect();
                let (items, complete) =
                    omni::suggest(&m.text, &tabs, &self.history, &self.bookmarks, &self.settings);
                let payload = json!({ "items": items, "complete": complete });
                let reply = if self.from_glass { self.glass_ui().unwrap_or(&self.ui) } else { &self.ui };
                let _ = reply.evaluate_script(&format!("mb.suggest({},{payload})", m.num));
            }
            "overlay" => {
                if m.flag {
                    self.open_drop();
                } else {
                    self.close_drop();
                }
            }
            "dropReady" => {
                if self.drop_wait {
                    self.drop_ready();
                }
            }
            "focus" if !self.from_glass => self.ui_focus = m.flag,
            "content" => self.focus_content(),
            "menu" => self.main_menu(m.x, m.y),
            "tabmenu" => self.tab_menu(m.id),
            _ => {}
        }
    }

    // ------------------------------------------------------------ UI sync

    pub fn flush_ui(&mut self) {
        if !self.ui_dirty || !self.ui_ready {
            return;
        }
        self.ui_dirty = false;
        let tabs: Vec<_> = self
            .tabs
            .iter()
            .filter(|t| t.space == self.space)
            .map(|t| {
                json!({
                    "id": t.id, "title": t.display_title(), "icon": self.tab_icon(t), "loading": t.loading && t.view.is_some(),
                    "audio": t.audio, "muted": t.muted, "private": t.private, "pinned": t.pinned,
                    "sleeping": t.view.is_none() && t.id != self.active,
                    "page": match t.internal {
                        Some(Internal::NewTab) => "newtab",
                        Some(Internal::History) => "history",
                        Some(Internal::Bookmarks) => "bookmarks",
                        Some(Internal::Settings) => "settings",
                        None => "",
                    },
                })
            })
            .collect();
        let mut state = match self.active_idx() {
            Some(i) => {
                let t = &self.tabs[i];
                let url = match t.internal {
                    Some(k) => k.url(),
                    None => &t.url,
                };
                let view = t.view.as_ref();
                let site_ok = host_of(&t.url).is_some();
                json!({
                    "tabs": tabs, "active": t.id, "url": url,
                    "back": view.and_then(|v| v.can_go_back().ok()).unwrap_or(false),
                    "fwd": view.and_then(|v| v.can_go_forward().ok()).unwrap_or(false),
                    "loading": t.loading, "private": t.private,
                    "star": t.internal.is_none() && self.bookmarks.contains(&t.url),
                    "canStar": t.internal.is_none() && !t.url.is_empty(),
                    "title": t.display_title(),
                    "blocked": t.shared.blocked.get(),
                    "shield": self.block.enabled.get() && !t.shared.bypass.get(),
                    "site": site_ok,
                    // Translate button: shown on foreign pages and while translating.
                    "tr": !t.shared.translate.borrow().is_empty(),
                    "trShow": site_ok && t.internal.is_none() && (t.foreign || !t.shared.translate.borrow().is_empty()),
                })
            }
            None => json!({ "tabs": tabs, "active": 0, "url": "" }),
        };
        state["max"] = json!(self.maximized);
        let (running, got, all) = self.downloads.iter().filter(|d| d.state == 0).fold((0, 0i64, 0i64), |(n, g, a), d| {
            (n + 1, g + d.received, a + d.total.max(0))
        });
        state["dl"] = json!({
            "active": running,
            "progress": if all > 0 { got as f64 / all as f64 } else { -1.0 },
            "unseen": self.dl_unseen,
        });
        if let Some(s) = self.spaces.iter().find(|s| s.id == self.space) {
            state["space"] = json!({ "id": s.id, "name": s.name, "color": s.color });
        }
        state["spaces"] = json!(self.spaces_list());
        let _ = self.ui.evaluate_script(&format!("mb.state({state})"));
    }

    // ------------------------------------------------------------ housekeeping

    /// Periodic: put idle background tabs to sleep, persist state.
    pub fn tick(&mut self) {
        // Old lists, changed settings or an edited blocklist.txt → background update.
        self.adblock_check(false);
        // Default extensions (VOT): installed once, then kept up to date.
        crate::defext::maybe_check(&self.proxy);
        if self.ab_res_view.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(120)) {
            self.ab_res_view = None; // the CDN never answered; next update tries again
        }
        if self.settings.sleep_minutes > 0 {
            let limit = Duration::from_secs(self.settings.sleep_minutes * 60);
            for i in 0..self.tabs.len() {
                let t = &self.tabs[i];
                if t.view.is_some() && !t.pinned && !t.private && !t.audio && t.last_active.elapsed() > limit {
                    self.sleep_tab(i);
                }
            }
        }
        if self.session_dirty {
            self.save_session();
        }
        self.history.flush();
        self.icons.flush();
    }

    fn save_session(&mut self) {
        self.session_dirty = false;
        let mut spaces = Vec::new();
        for s in &self.spaces {
            // Private and empty new tabs are not worth restoring.
            let saved: Vec<&Tab> = self
                .tabs
                .iter()
                .filter(|t| t.space == s.id)
                .filter(|t| !t.private && (t.internal != Some(Internal::NewTab) || t.id == self.active))
                .collect();
            let remembered = if s.id == self.space { self.active } else { s.active };
            spaces.push(SessionSpace {
                name: s.name.clone(),
                color: s.color.clone(),
                icon: s.icon.clone(),
                active: saved.iter().position(|t| t.id == remembered).unwrap_or(0),
                tabs: saved
                    .iter()
                    .map(|t| SessionTab {
                        url: t.session_url(),
                        title: t.title.clone(),
                        pinned: t.pinned,
                        icon: if t.icon.starts_with("http") { t.icon.clone() } else { String::new() },
                    })
                    .collect(),
            });
        }
        let window = self.win.map(|w| WinState { maximized: self.window.is_maximized(), ..w });
        Session {
            tabs: Vec::new(),
            active: 0,
            window,
            space: self.spaces.iter().position(|s| s.id == self.space).unwrap_or(0),
            spaces,
        }
        .save();
    }

    pub fn shutdown(&mut self) {
        self.save_session();
        self.history.flush_sync();
        self.icons.flush();
    }

    // ------------------------------------------------------------ native menus

    fn hwnd(&self) -> HWND {
        HWND(self.window.hwnd() as _)
    }

    fn popup(&self, items: &[MenuItem], at: Option<(i32, i32)>, right_align: bool) -> u32 {
        unsafe {
            let Ok(menu) = build_menu(items) else { return 0 };
            let (x, y) = at.unwrap_or_else(|| {
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                (p.x, p.y)
            });
            let mut flags = TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON;
            if right_align {
                flags |= TPM_RIGHTALIGN;
            }
            let _ = SetForegroundWindow(self.hwnd());
            let cmd = TrackPopupMenu(menu, flags, x, y, None, self.hwnd(), None);
            let _ = DestroyMenu(menu);
            self.restore_focus();
            cmd.0 as u32
        }
    }

    fn main_menu(&mut self, x: f64, y: f64) {
        let sf = self.window.scale_factor();
        let origin = self.window.inner_position().unwrap_or_default();
        let at = (origin.x + (x * sf) as i32, origin.y + (y * sf) as i32);
        let search = self.settings.search.clone();
        let s = &self.settings;
        let can_translate = self.active_idx().is_some_and(|i| host_of(&self.tabs[i].url).is_some() && self.tabs[i].internal.is_none());
        let translating = self.active_idx().is_some_and(|i| !self.tabs[i].shared.translate.borrow().is_empty());
        let ab_status = self.adblock_status();
        // Only what people reach for. Print / DevTools live on Ctrl+P / F12, tab sleep
        // is always on, blocking is switched per site with the shield.
        let items = [
            MenuItem::Item(1, "Новая вкладка\tCtrl+T"),
            MenuItem::Item(2, "Новая приватная вкладка\tCtrl+Shift+N"),
            MenuItem::Item(3, "Открыть закрытую вкладку\tCtrl+Shift+T"),
            MenuItem::Sep,
            MenuItem::Item(4, "История\tCtrl+H"),
            MenuItem::Item(60, "Закладки\tCtrl+Shift+O"),
            MenuItem::Item(61, "Настройки"),
            MenuItem::Item(5, "Закладка на эту страницу\tCtrl+D"),
            MenuItem::Item(8, "Загрузки"),
            MenuItem::Item(9, "Расширения"),
            MenuItem::Enabled(10, if translating { "Показать оригинал" } else { "Перевести страницу" }, can_translate),
            MenuItem::Sep,
            MenuItem::Sub(
                "Блокировка рекламы",
                vec![
                    MenuItem::Check(40, "Блокировать рекламу и трекеры", s.adblock),
                    MenuItem::Check(41, "Скрывать окна о cookie", s.adblock_cookies),
                    MenuItem::Check(42, "Скрывать раздражающие элементы", s.adblock_annoyances),
                    MenuItem::Check(43, "SponsorBlock: пропускать спонсоров на YouTube", s.sponsorblock),
                    MenuItem::Sep,
                    MenuItem::Enabled(44, &ab_status, !self.ab_busy),
                    MenuItem::Item(45, "Мои фильтры…"),
                ],
            ),
            MenuItem::Sub(
                "Поисковая система",
                vec![
                    MenuItem::Check(20, "Google", search == "google"),
                    MenuItem::Check(21, "Яндекс", search == "yandex"),
                    MenuItem::Check(22, "DuckDuckGo", search == "duckduckgo"),
                    MenuItem::Check(23, "Bing", search == "bing"),
                ],
            ),
            MenuItem::Sep,
            MenuItem::Item(30, "Очистить историю"),
            MenuItem::Item(31, "Удалить cookies и кэш…"),
            MenuItem::Item(32, "Папка настроек"),
            MenuItem::Sep,
            MenuItem::Item(50, "Установить и сделать браузером по умолчанию"),
            MenuItem::Item(51, "Удалить установку и регистрацию браузера"),
        ];
        match self.popup(&items, Some(at), true) {
            1 => self.exec(Cmd::NewTab),
            2 => self.exec(Cmd::NewPrivate),
            3 => self.exec(Cmd::Reopen),
            4 => self.exec(Cmd::History),
            60 => self.exec(Cmd::Bookmarks),
            61 => self.open_internal(Internal::Settings),
            5 => self.exec(Cmd::Bookmark),
            8 => self.open_panel(Panel::Downloads),
            9 => self.open_panel(Panel::Extensions),
            10 => self.toggle_translate(),
            40 => {
                self.settings.adblock = !self.settings.adblock;
                self.block.enabled.set(self.settings.adblock);
                self.settings.save();
                if let Some(v) = self.active_view() {
                    let _ = v.reload();
                }
            }
            41 => {
                self.settings.adblock_cookies = !self.settings.adblock_cookies;
                self.adblock_changed();
            }
            42 => {
                self.settings.adblock_annoyances = !self.settings.adblock_annoyances;
                self.adblock_changed();
            }
            43 => {
                self.settings.sponsorblock = !self.settings.sponsorblock;
                self.block.sponsorblock.set(self.settings.sponsorblock);
                self.settings.save();
            }
            44 => {
                self.ab_error.clear();
                self.adblock_check(true);
            }
            45 => {
                let _ = std::process::Command::new("notepad").arg(blocker::user_list_path()).spawn();
            }
            n @ 20..=23 => {
                self.settings.search = ["google", "yandex", "duckduckgo", "bing"][(n - 20) as usize].into();
                self.settings.save();
            }
            30 => {
                if self.confirm("Очистить всю историю посещений?") {
                    self.history.clear();
                    self.history.flush();
                }
            }
            31 => {
                if self.confirm("Удалить cookies, кэш и данные всех сайтов?\nВы выйдете из всех аккаунтов.") {
                    let _ = self.ui.clear_all_browsing_data();
                }
            }
            32 => {
                let _ = std::process::Command::new("explorer").arg(store::data_dir()).spawn();
            }
            50 => crate::shell::install_async(),
            51 => {
                if self.confirm("Удалить установленную копию браузера и его регистрацию в Windows?\nВаши данные не будут затронуты.") {
                    crate::shell::uninstall_async();
                }
            }
            _ => {}
        }
        self.ui_dirty = true;
    }

    fn tab_menu(&mut self, id: u32) {
        let Some(i) = self.idx(id) else { return };
        let t = &self.tabs[i];
        let can_sleep = t.id != self.active && t.view.is_some();
        let (url, private, pinned, muted) = (t.session_url(), t.private, t.pinned, t.muted);
        let others: Vec<(u32, String)> =
            self.spaces.iter().filter(|s| s.id != self.space).map(|s| (s.id, s.name.clone())).collect();
        let mut move_items: Vec<MenuItem> =
            others.iter().enumerate().map(|(n, (_, name))| MenuItem::Item(100 + n as u32, name)).collect();
        if !move_items.is_empty() {
            move_items.push(MenuItem::Sep);
        }
        move_items.push(MenuItem::Item(99, "Новое пространство"));
        let items = [
            MenuItem::Item(1, "Новая вкладка справа"),
            MenuItem::Item(9, "Обновить"),
            MenuItem::Item(2, "Дублировать"),
            MenuItem::Item(3, if pinned { "Открепить" } else { "Закрепить" }),
            MenuItem::Item(4, if muted { "Включить звук" } else { "Выключить звук" }),
            MenuItem::Enabled(5, "Усыпить вкладку", can_sleep),
            MenuItem::Sub("Переместить в пространство", move_items),
            MenuItem::Sep,
            MenuItem::Item(6, "Закрыть\tCtrl+W"),
            MenuItem::Item(7, "Закрыть другие вкладки"),
            MenuItem::Item(8, "Закрыть вкладки справа"),
        ];
        match self.popup(&items, None, false) {
            1 => {
                let nid = self.alloc_id();
                let at = self.insert_at(Some(i));
                self.tabs.insert(at, Tab::new(nid, self.space, "", private));
                self.ui_focus = true;
                self.activate(nid);
                self.focus_url();
            }
            2 => {
                self.open(&url, Some(i), private, true);
            }
            3 => self.toggle_pin(id),
            4 => self.toggle_mute(id),
            5 => self.sleep_tab(i),
            6 => self.close(id),
            7 => {
                let ids: Vec<u32> = self
                    .visible()
                    .iter()
                    .map(|&g| &self.tabs[g])
                    .filter(|t| t.id != id && !t.pinned)
                    .map(|t| t.id)
                    .collect();
                self.activate(id);
                for x in ids {
                    self.close(x);
                }
            }
            8 => {
                let vis = self.visible();
                let pos = vis.iter().position(|&g| g == i).unwrap_or(vis.len());
                let ids: Vec<u32> = vis[pos + 1..].iter().map(|&g| self.tabs[g].id).collect();
                if ids.contains(&self.active) {
                    self.activate(id);
                }
                for x in ids {
                    self.close(x);
                }
            }
            9 => {
                if id == self.active {
                    self.reload();
                } else if let Some(v) = &self.tabs[i].view {
                    let _ = v.reload();
                }
            }
            99 => {
                let sid = self.create_space("", "");
                self.move_to_space(id, sid);
                if let Some(s) = self.space_mut(sid) {
                    s.active = id;
                }
                self.switch_space(sid);
            }
            n if n >= 100 => {
                if let Some(&(sid, _)) = others.get((n - 100) as usize) {
                    self.move_to_space(id, sid);
                }
            }
            _ => {}
        }
    }

    fn confirm(&self, text: &str) -> bool {
        let yes = unsafe {
            MessageBoxW(Some(self.hwnd()), &HSTRING::from(text), &HSTRING::from(crate::APP_NAME), MB_YESNO | MB_ICONWARNING)
                == IDYES
        };
        self.restore_focus();
        yes
    }

    /// Keys that reach the frame window itself (focus got stranded there, e.g. after
    /// a native menu): run the shortcut, otherwise hand focus back to the page.
    pub fn frame_key(&mut self, code: tao::keyboard::KeyCode, repeat: bool) {
        use tao::keyboard::KeyCode as K;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
        let vk = match code {
            K::KeyT => 'T' as u32,
            K::KeyN => 'N' as u32,
            K::KeyW => 'W' as u32,
            K::KeyL => 'L' as u32,
            K::KeyD => 'D' as u32,
            K::KeyR => 'R' as u32,
            K::KeyH => 'H' as u32,
            K::KeyM => 'M' as u32,
            K::KeyO => 'O' as u32,
            K::Digit1 => '1' as u32,
            K::Digit2 => '2' as u32,
            K::Digit3 => '3' as u32,
            K::Digit4 => '4' as u32,
            K::Digit5 => '5' as u32,
            K::Digit6 => '6' as u32,
            K::Digit7 => '7' as u32,
            K::Digit8 => '8' as u32,
            K::Digit9 => '9' as u32,
            K::Tab => 0x09,
            K::PageUp => 0x21,
            K::PageDown => 0x22,
            K::ArrowLeft => 0x25,
            K::ArrowRight => 0x27,
            K::F4 => 0x73,
            K::F5 => 0x74,
            K::F6 => 0x75,
            K::F11 => 0x7A,
            _ => 0,
        };
        let down = |k: u16| unsafe { GetKeyState(k as i32) < 0 };
        if *tab::DEBUG {
            eprintln!("frame key {code:?} vk={vk:#x} ctrl={}", down(VK_CONTROL.0));
        }
        match tab::map_key(vk, down(VK_CONTROL.0), down(VK_SHIFT.0), down(VK_MENU.0)) {
            Some(cmd) if !repeat || matches!(cmd, Cmd::Next | Cmd::Prev) => self.exec(cmd),
            Some(_) => {}
            None => self.restore_focus(),
        }
    }

}

/// "C:\\x\\Sketch.PNG" -> "png".
fn file_ext(path: &str) -> String {
    Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Opens a file with its default program.
fn shell_open(path: &str) {
    if !path.is_empty() && Path::new(path).exists() {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
}

fn chrome_rect(window: &Window, logical_h: f64) -> Rect {
    let size = window.inner_size();
    let h = ((logical_h * window.scale_factor()).round() as u32).min(size.height);
    Rect { position: PhysicalPosition::new(0, 0).into(), size: PhysicalSize::new(size.width, h).into() }
}

enum MenuItem<'a> {
    Item(u32, &'a str),
    Enabled(u32, &'a str, bool),
    Check(u32, &'a str, bool),
    Sub(&'a str, Vec<MenuItem<'a>>),
    Sep,
}

unsafe fn build_menu(items: &[MenuItem]) -> windows::core::Result<HMENU> {
    unsafe {
        let menu = CreatePopupMenu()?;
        for item in items {
            match item {
                MenuItem::Item(id, text) => AppendMenuW(menu, MF_STRING, *id as usize, &HSTRING::from(*text))?,
                MenuItem::Enabled(id, text, on) => {
                    let flags = if *on { MF_STRING } else { MF_STRING | MF_GRAYED };
                    AppendMenuW(menu, flags, *id as usize, &HSTRING::from(*text))?
                }
                MenuItem::Check(id, text, on) => {
                    let flags = if *on { MF_STRING | MF_CHECKED } else { MF_STRING };
                    AppendMenuW(menu, flags, *id as usize, &HSTRING::from(*text))?
                }
                MenuItem::Sub(text, sub) => {
                    let s = build_menu(sub)?;
                    AppendMenuW(menu, MF_POPUP, s.0 as usize, &HSTRING::from(*text))?
                }
                MenuItem::Sep => AppendMenuW(menu, MF_SEPARATOR, 0, None)?,
            }
        }
        Ok(menu)
    }
}

/// The app icon (meteor) from the exe's resources: `assets/mbrowser.ico`, embedded
/// by build.rs as resource #1. `metric` picks the frame: `SM_CXSMICON` for the
/// title bar, `SM_CXICON` for the taskbar and Alt+Tab.
fn app_icon(metric: SYSTEM_METRICS_INDEX) -> Option<Icon> {
    let px = unsafe { GetSystemMetrics(metric) }.max(16) as u32;
    Icon::from_resource(1, Some(PhysicalSize::new(px, px))).ok()
}
