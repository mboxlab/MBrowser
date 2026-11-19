//! Content webviews: creation and native WebView2 event hooks.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{
        LazyLock,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use tao::{event_loop::EventLoopProxy, window::Window};
use webview2_com::{
    AcceleratorKeyPressedEventHandler, AddScriptToExecuteOnDocumentCreatedCompletedHandler,
    BytesReceivedChangedEventHandler, CallDevToolsProtocolMethodCompletedHandler, CapturePreviewCompletedHandler,
    ContainsFullScreenElementChangedEventHandler,
    ContentLoadingEventHandler, DownloadStartingEventHandler, ExecuteScriptCompletedHandler,
    FaviconChangedEventHandler, HistoryChangedEventHandler, IsDocumentPlayingAudioChangedEventHandler,
    StateChangedEventHandler, Microsoft::Web::WebView2::Win32::*, NavigationStartingEventHandler,
    SourceChangedEventHandler, WebMessageReceivedEventHandler,
    WebResourceRequestedEventHandler, take_pwstr,
};
use windows::{
    Win32::{
        System::Com::{IStream, STREAM_SEEK_SET},
        UI::{
            Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT},
            Shell::SHCreateMemStream,
        },
    },
    core::{BOOL, HSTRING, Interface, PWSTR, w},
};
use wry::{
    NewWindowResponse, PageLoadEvent, Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows,
    WebViewExtWindows,
};

use crate::blocker::{BlockCtx, Verdict, host_of};

/// Injected into every web page before its own scripts: cosmetic filtering,
/// SponsorBlock, page translation (see content.js).
const CONTENT_JS: &str = include_str!("content.js");

/// Stand-in history entries. WebView2 cannot be handed a back/forward list, so
/// a fresh webview (reopened, woken or restored tab) gets one stand-in page per
/// earlier entry before its own page. Loaded by us it stays blank; reached with
/// Back / Forward it replaces itself with the page it stands for. Served from
/// memory (see `serve_hold`), never from the network.
const HOLD_PREFIX: &str = "https://mb-restore.localhost/?u=";
const HOLD_HTML: &str = r#"<!doctype html><meta charset="utf-8"><meta name="referrer" content="no-referrer">
<link rel="icon" href="data:,"><title></title><script>
(()=>{const u=new URLSearchParams(location.search).get('u')||'';
const go=()=>{if(/^https?:\/\//i.test(u))location.replace(u)};
const n=performance.getEntriesByType('navigation')[0];
if(n&&n.type!=='navigate')go();
addEventListener('pageshow',e=>{if(e.persisted)go()});})();
</script>"#;

/// Stand-in history entry for `url`.
pub fn hold_url(url: &str) -> String {
    format!("{HOLD_PREFIX}{}", crate::store::encode_query(url))
}

pub fn is_hold(url: &str) -> bool {
    url.starts_with(HOLD_PREFIX)
}

/// The page a stand-in entry stands for.
pub fn hold_target(url: &str) -> Option<String> {
    let b = url.strip_prefix(HOLD_PREFIX)?.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
            .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (b[i], hex) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(if c == b'+' { b' ' } else { c });
                i += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Answers a stand-in entry's request from memory. False for any other request.
unsafe fn serve_hold(
    env: &ICoreWebView2Environment,
    args: &ICoreWebView2WebResourceRequestedEventArgs,
) -> windows::core::Result<bool> {
    unsafe {
        let mut uri = PWSTR::null();
        args.Request()?.Uri(&mut uri)?;
        if !is_hold(&take_pwstr(uri)) {
            return Ok(false);
        }
        let stream = SHCreateMemStream(Some(HOLD_HTML.as_bytes()));
        let headers = w!("Content-Type: text/html; charset=utf-8\r\nCache-Control: no-store");
        args.SetResponse(&env.CreateWebResourceResponse(stream.as_ref(), 200, w!("OK"), headers)?)?;
        Ok(true)
    }
}

/// Asks for the tab's back/forward list; it arrives as `UserEvent::NavList`.
pub fn query_history(view: &WebView, id: u32, proxy: &Proxy) {
    let p = proxy.clone();
    let done = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |res, json| {
        if res.is_ok() {
            let _ = p.send_event(UserEvent::NavList(id, json));
        }
        Ok(())
    }));
    unsafe {
        let _ = view.webview().CallDevToolsProtocolMethod(w!("Page.getNavigationHistory"), w!("{}"), &done);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cmd {
    NewTab,
    NewPrivate,
    Close,
    Reopen,
    Next,
    Prev,
    Nth(usize),
    Last,
    FocusUrl,
    Reload,
    Back,
    Forward,
    Bookmark,
    Fullscreen,
    History,
    /// Bookmarks manager (Ctrl+Shift+O).
    Bookmarks,
    Mute,
    /// Switch to space N (Ctrl+Shift+1…9).
    Space(usize),
}

#[derive(Debug)]
pub enum UserEvent {
    Title(u32, String),
    Url(u32),
    Load(u32, bool),
    Nav(u32),
    /// A tab's back/forward list: CDP `Page.getNavigationHistory` JSON.
    NavList(u32, String),
    Favicon(u32, String),
    Audio(u32, bool),
    Fullscreen(u32, bool),
    OpenTab { url: String, opener: u32 },
    /// Request from a second launch (its command-line argument, may be empty).
    External(String),
    Key(Cmd),
    Blocked(u32),
    Ipc(String),
    /// IPC from the Tablo's glass window (a second chrome instance).
    GlassIpc(String),
    /// Idle after startup: load the glass window's page in advance.
    WarmGlass,
    /// JPEG screenshot of the active page (base64), backdrop for the launcher.
    Preview(String),
    /// (site, JSON result of the icon probe): the page's largest declared icon.
    Icon(String, String),
    /// The list of installed extensions changed (or was read).
    Extensions,
    /// A page webview got the keyboard focus (closes dropdown panels).
    PageFocus(u32),
    /// Extension popup (generation): time to measure it / its measurement / it lost focus.
    PopupTick(u32),
    PopupSize(u32, String),
    PopupBlur(u32),
    PopupFocus(u32),
    /// A message from a page's content script (SponsorBlock, translation, language).
    Page(u32, serde_json::Value),
    /// JSON to post back to a tab's content script.
    PageReply(u32, String),
    /// SponsorBlock answer for a hash prefix: (tab, prefix, JSON body).
    SponsorBlock(u32, String, String),
    /// A freshly compiled ad-blocking engine (background updater).
    Adblock(EngineBox),
    /// The updater failed (message for the menu).
    AdblockFailed(String),
    /// Time to see whether the filter lists need an update.
    AdblockCheck,
    /// A default extension (VOT) was installed or updated in this folder.
    ExtInstalled(std::path::PathBuf),
    /// uBO resources (scriptlets, redirects) built by the hidden resources page.
    AdblockResources(String),
    /// A Tablo icon finished downloading into the disk cache.
    IconCached,
    /// A download began (our own downloads button replaces Edge's flyout).
    DlStart { id: u32, url: String, path: String, total: i64, page: String, private: bool },
    /// (download id, bytes received, total bytes or -1).
    DlProgress(u32, i64, i64),
    /// (download id, state: 1 done / 2 failed / 3 cancelled, received, total, final path).
    DlState(u32, u8, i64, i64, String),
    /// A webview process failed / hung (see recover.rs).
    Recover(crate::app::recover::Ev),
}

pub type Proxy = EventLoopProxy<UserEvent>;

/// The engine has no Debug impl; events must be Debug.
pub struct EngineBox(pub Box<adblock::Engine>);
impl std::fmt::Debug for EngineBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Engine")
    }
}

/// Running downloads by id, so the UI can pause / resume / cancel them.
pub type Downloads = Rc<RefCell<HashMap<u32, ICoreWebView2DownloadOperation>>>;

/// Download ids (also used for entries loaded from disk).
pub static NEXT_DL: AtomicU32 = AtomicU32::new(1);

/// `MBROWSER_DEBUG=1` logs every third-party request (handy for tuning blocklist.txt).
pub static DEBUG: LazyLock<bool> = LazyLock::new(|| std::env::var_os("MBROWSER_DEBUG").is_some());

/// State shared between a tab and its native callbacks (all on the UI thread).
#[derive(Default)]
pub struct TabShared {
    pub blocked: Cell<u32>,
    pub notify_pending: Cell<bool>,
    pub page_host: RefCell<String>,
    pub bypass: Cell<bool>,
    /// Top-level document URL (the "source" of every request) and the URL of the
    /// navigation in flight (its own request is the main frame, never blocked).
    pub page_url: RefCell<String>,
    pub nav_url: RefCell<String>,
    /// Generic-hiding exceptions of the current site.
    pub cos_exceptions: RefCell<HashSet<String>>,
    /// Document-start scripts registered for the current navigation.
    script_ids: RefCell<Vec<String>>,
    script_gen: Cell<u32>,
    /// Same scripts, run by hand if the registration came too late (ContentLoading).
    pending_scripts: RefCell<Option<Vec<String>>>,
    /// Translate every page of this tab into this language ("" = off).
    pub translate: RefCell<String>,
}

pub enum Source {
    Url(String),
    Html(String),
    /// No navigation at all (a warm spare that gets its URL later).
    Blank,
}

pub struct Spec<'a> {
    pub id: u32,
    pub private: bool,
    pub source: Source,
    pub bounds: Rect,
    pub visible: bool,
    pub env: &'a ICoreWebView2Environment,
    pub tracking: &'a str,
}

pub fn build(
    window: &Window,
    spec: Spec,
    proxy: &Proxy,
    block: &Rc<BlockCtx>,
    shared: &Rc<TabShared>,
    downloads: &Downloads,
) -> wry::Result<WebView> {
    let id = spec.id;
    let private = spec.private;
    let (p1, p2, p3) = (proxy.clone(), proxy.clone(), proxy.clone());

    let builder = WebViewBuilder::new()
        .with_environment(spec.env.clone())
        .with_incognito(spec.private)
        .with_bounds(spec.bounds)
        .with_visible(spec.visible)
        .with_focused(spec.visible)
        // Dark until the first page paints: no white flash on a dark browser.
        // Switched back to white after the first load (see App::handle).
        .with_background_color((17, 18, 22, 255))
        .with_devtools(true)
        .with_document_title_changed_handler(move |t| {
            let _ = p1.send_event(UserEvent::Title(id, t));
        })
        .with_on_page_load_handler(move |ev, _url| {
            let _ = p2.send_event(UserEvent::Load(id, matches!(ev, PageLoadEvent::Started)));
        })
        .with_new_window_req_handler(move |url, features| {
            // window.open() with explicit geometry is usually an auth/payment popup that
            // needs `window.opener` — let WebView2 open a real popup for it.
            // Everything else (target=_blank, middle click) becomes a tab.
            if features.size.is_some() || features.position.is_some() {
                return NewWindowResponse::Allow;
            }
            let _ = p3.send_event(UserEvent::OpenTab { url, opener: id });
            NewWindowResponse::Deny
        });

    let builder = match spec.source {
        Source::Url(u) => builder.with_url(u),
        Source::Html(h) => builder.with_html(h),
        Source::Blank => builder,
    };
    let view = builder.build_as_child(window)?;
    clip_siblings(&view);

    unsafe {
        let _ = hook_keys(&view, proxy);
        if let Err(e) = hook_content(&view, id, proxy, block, shared, spec.tracking) {
            eprintln!("tab hooks failed: {e}");
        }
        let _ = hook_downloads(&view, proxy, downloads, private);
        crate::app::recover::hook(&view, id, proxy);
        // The page took the keyboard focus (the user clicked into it).
        let p = proxy.clone();
        let mut token = 0i64;
        let _ = view.controller().add_GotFocus(
            &webview2_com::FocusChangedEventHandler::create(Box::new(move |_, _| {
                let _ = p.send_event(UserEvent::PageFocus(id));
                Ok(())
            })),
            &mut token,
        );
    }
    Ok(view)
}

/// Browser shortcuts must work no matter which webview has keyboard focus.
pub unsafe fn hook_keys(view: &WebView, proxy: &Proxy) -> windows::core::Result<()> {
    let proxy = proxy.clone();
    let mut token = 0i64;
    unsafe {
        view.controller().add_AcceleratorKeyPressed(
            &AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut kind = COREWEBVIEW2_KEY_EVENT_KIND::default();
                args.KeyEventKind(&mut kind)?;
                if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
                    && kind != COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN
                {
                    return Ok(());
                }
                let mut vk = 0u32;
                args.VirtualKey(&mut vk)?;
                let down = |k: u16| GetKeyState(k as i32) < 0;
                let (ctrl, shift, alt) = (down(VK_CONTROL.0), down(VK_SHIFT.0), down(VK_MENU.0));
                if *DEBUG {
                    eprintln!("accel vk={vk:#x} ctrl={ctrl} shift={shift} alt={alt}");
                }
                if let Some(cmd) = map_key(vk, ctrl, shift, alt) {
                    args.SetHandled(true)?;
                    let mut status = COREWEBVIEW2_PHYSICAL_KEY_STATUS::default();
                    args.PhysicalKeyStatus(&mut status)?;
                    let repeat = status.WasKeyDown.as_bool();
                    if !repeat || matches!(cmd, Cmd::Next | Cmd::Prev) {
                        let _ = proxy.send_event(UserEvent::Key(cmd));
                    }
                }
                Ok(())
            })),
            &mut token,
        )
    }
}

pub fn map_key(vk: u32, ctrl: bool, shift: bool, alt: bool) -> Option<Cmd> {
    const TAB: u32 = 0x09;
    const PGUP: u32 = 0x21;
    const PGDN: u32 = 0x22;
    const LEFT: u32 = 0x25;
    const RIGHT: u32 = 0x27;
    const F4: u32 = 0x73;
    const F5: u32 = 0x74;
    const F6: u32 = 0x75;
    const F11: u32 = 0x7A;
    let key = |c: char| c as u32;
    Some(match (ctrl, shift, alt, vk) {
        (true, false, false, k) if k == key('T') || k == key('N') => Cmd::NewTab,
        (true, true, false, k) if k == key('T') => Cmd::Reopen,
        (true, true, false, k) if k == key('N') => Cmd::NewPrivate,
        (true, true, false, k) if (key('1')..=key('9')).contains(&k) => Cmd::Space((k - key('1')) as usize),
        (true, false, false, k) if k == key('W') || k == F4 => Cmd::Close,
        (true, false, false, TAB) | (true, false, false, PGDN) => Cmd::Next,
        (true, true, false, TAB) | (true, false, false, PGUP) => Cmd::Prev,
        (true, false, false, k) if (key('1')..=key('8')).contains(&k) => Cmd::Nth((k - key('1')) as usize),
        (true, false, false, k) if k == key('9') => Cmd::Last,
        (true, false, false, k) if k == key('L') => Cmd::FocusUrl,
        (false, false, true, k) if k == key('D') => Cmd::FocusUrl,
        (false, false, false, F6) => Cmd::FocusUrl,
        (true, false, false, k) if k == key('R') => Cmd::Reload,
        (false, false, false, F5) => Cmd::Reload,
        (false, false, true, LEFT) => Cmd::Back,
        (false, false, true, RIGHT) => Cmd::Forward,
        (true, false, false, k) if k == key('D') => Cmd::Bookmark,
        (true, false, false, k) if k == key('H') => Cmd::History,
        (true, true, false, k) if k == key('O') => Cmd::Bookmarks,
        (true, false, false, k) if k == key('M') => Cmd::Mute,
        (false, false, false, F11) => Cmd::Fullscreen,
        _ => return None,
    })
}

/// Downloads go to our own panel: hide Edge's flyout, report start, progress
/// (at most 4 updates a second) and the final state.
unsafe fn hook_downloads(
    view: &WebView,
    proxy: &Proxy,
    downloads: &Downloads,
    private: bool,
) -> windows::core::Result<()> {
    let core = view.webview();
    let Ok(c4) = core.cast::<ICoreWebView2_4>() else { return Ok(()) };
    let (proxy, registry) = (proxy.clone(), downloads.clone());
    let mut token = 0i64;
    unsafe {
        c4.add_DownloadStarting(
            &DownloadStartingEventHandler::create(Box::new(move |sender, args| {
                let Some(args) = args else { return Ok(()) };
                args.SetHandled(true)?;
                let op = args.DownloadOperation()?;
                let id = NEXT_DL.fetch_add(1, Ordering::Relaxed);
                let read = |f: &dyn Fn(*mut PWSTR) -> windows::core::Result<()>| {
                    let mut s = PWSTR::null();
                    f(&mut s).map(|_| take_pwstr(s)).unwrap_or_default()
                };
                let url = read(&|p| op.Uri(p));
                let path = read(&|p| args.ResultFilePath(p));
                let page = sender.map(|s| read(&|p| s.Source(p))).unwrap_or_default();
                let mut total = 0i64;
                op.TotalBytesToReceive(&mut total)?;

                let mut t = 0i64;
                let last = Rc::new(Cell::new(Instant::now() - Duration::from_secs(1)));
                let p = proxy.clone();
                op.add_BytesReceivedChanged(
                    &BytesReceivedChangedEventHandler::create(Box::new(move |op, _| {
                        if last.get().elapsed() < Duration::from_millis(250) {
                            return Ok(());
                        }
                        last.set(Instant::now());
                        if let Some(op) = op {
                            let (mut got, mut all) = (0i64, 0i64);
                            op.BytesReceived(&mut got)?;
                            op.TotalBytesToReceive(&mut all)?;
                            let _ = p.send_event(UserEvent::DlProgress(id, got, all));
                        }
                        Ok(())
                    })),
                    &mut t,
                )?;

                let (p, reg) = (proxy.clone(), registry.clone());
                op.add_StateChanged(
                    &StateChangedEventHandler::create(Box::new(move |op, _| {
                        let Some(op) = op else { return Ok(()) };
                        let mut st = COREWEBVIEW2_DOWNLOAD_STATE::default();
                        op.State(&mut st)?;
                        let state = if st == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED {
                            1
                        } else if st == COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED {
                            let mut why = COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON::default();
                            op.InterruptReason(&mut why)?;
                            if why == COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_USER_CANCELED { 3 } else { 2 }
                        } else {
                            return Ok(()); // paused/resumed: progress events cover it
                        };
                        reg.borrow_mut().remove(&id);
                        let (mut got, mut all) = (0i64, 0i64);
                        op.BytesReceived(&mut got)?;
                        op.TotalBytesToReceive(&mut all)?;
                        let mut s = PWSTR::null();
                        let path = op.ResultFilePath(&mut s).map(|_| take_pwstr(s)).unwrap_or_default();
                        let _ = p.send_event(UserEvent::DlState(id, state, got, all, path));
                        Ok(())
                    })),
                    &mut t,
                )?;

                registry.borrow_mut().insert(id, op);
                let _ = proxy.send_event(UserEvent::DlStart { id, url, path, total, page, private });
                Ok(())
            })),
            &mut token,
        )
    }
}

/// Native tracking prevention (Edge lists), runs in-process — no overhead for us.
/// Per profile, so it applies to every tab sharing the view's profile right away.
pub fn set_tracking(view: &WebView, tracking: &str) {
    unsafe {
        if let Ok(c13) = view.webview().cast::<ICoreWebView2_13>() {
            if let Ok(p3) = c13.Profile().and_then(|p| p.cast::<ICoreWebView2Profile3>()) {
                let level = match tracking {
                    "none" => COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_NONE,
                    "basic" => COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_BASIC,
                    "strict" => COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_STRICT,
                    _ => COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_BALANCED,
                };
                let _ = p3.SetPreferredTrackingPreventionLevel(level);
            }
        }
    }
}

unsafe fn hook_content(
    view: &WebView,
    id: u32,
    proxy: &Proxy,
    block: &Rc<BlockCtx>,
    shared: &Rc<TabShared>,
    tracking: &str,
) -> windows::core::Result<()> {
    let core = view.webview();
    let mut token = 0i64;

    unsafe {
        set_tracking(view, tracking);

        // Password manager: offer to save new logins and fill saved ones (also those
        // imported by tools/import_vivaldi.py). WebView2 has saving off by default.
        if let Ok(s4) = core.Settings().and_then(|s| s.cast::<ICoreWebView2Settings4>()) {
            let _ = s4.SetIsPasswordAutosaveEnabled(true);
            let _ = s4.SetIsGeneralAutofillEnabled(true);
        }

        // URL changes, including SPA pushState navigations (YouTube etc.)
        let p = proxy.clone();
        core.add_SourceChanged(
            &SourceChangedEventHandler::create(Box::new(move |_, _| {
                let _ = p.send_event(UserEvent::Url(id));
                Ok(())
            })),
            &mut token,
        )?;

        let p = proxy.clone();
        core.add_HistoryChanged(
            &HistoryChangedEventHandler::create(Box::new(move |_, _| {
                let _ = p.send_event(UserEvent::Nav(id));
                Ok(())
            })),
            &mut token,
        )?;

        let p = proxy.clone();
        core.add_ContainsFullScreenElementChanged(
            &ContainsFullScreenElementChangedEventHandler::create(Box::new(move |sender, _| {
                if let Some(s) = sender {
                    let mut fs = BOOL::default();
                    s.ContainsFullScreenElement(&mut fs)?;
                    let _ = p.send_event(UserEvent::Fullscreen(id, fs.as_bool()));
                }
                Ok(())
            })),
            &mut token,
        )?;

        if let Ok(c15) = core.cast::<ICoreWebView2_15>() {
            let p = proxy.clone();
            c15.add_FaviconChanged(
                &FaviconChangedEventHandler::create(Box::new(move |sender, _| {
                    if let Some(c) = sender.and_then(|s| s.cast::<ICoreWebView2_15>().ok()) {
                        let mut uri = PWSTR::null();
                        c.FaviconUri(&mut uri)?;
                        let _ = p.send_event(UserEvent::Favicon(id, take_pwstr(uri)));
                    }
                    Ok(())
                })),
                &mut token,
            )?;
        }

        if let Ok(c8) = core.cast::<ICoreWebView2_8>() {
            let p = proxy.clone();
            c8.add_IsDocumentPlayingAudioChanged(
                &IsDocumentPlayingAudioChangedEventHandler::create(Box::new(move |sender, _| {
                    if let Some(c) = sender.and_then(|s| s.cast::<ICoreWebView2_8>().ok()) {
                        let mut playing = BOOL::default();
                        c.IsDocumentPlayingAudio(&mut playing)?;
                        let _ = p.send_event(UserEvent::Audio(id, playing.as_bool()));
                    }
                    Ok(())
                })),
                &mut token,
            )?;
        }

        // A navigation starts: remember the site, and register this page's scripts
        // (cosmetic rules, scriptlets, content.js) to run before its own scripts.
        {
            let (block, shared, p) = (block.clone(), shared.clone(), proxy.clone());
            core.add_NavigationStarting(
                &NavigationStartingEventHandler::create(Box::new(move |sender, args| {
                    let (Some(core), Some(args)) = (sender, args) else { return Ok(()) };
                    let mut uri = PWSTR::null();
                    args.Uri(&mut uri)?;
                    let uri = take_pwstr(uri);
                    let host = host_of(&uri).unwrap_or("").to_ascii_lowercase();
                    shared.bypass.set(block.is_exception(&host));
                    *shared.page_host.borrow_mut() = host;
                    *shared.page_url.borrow_mut() = uri.clone();
                    *shared.nav_url.borrow_mut() = uri.clone();
                    if shared.blocked.replace(0) != 0 {
                        shared.notify_pending.set(true);
                        let _ = p.send_event(UserEvent::Blocked(id));
                    }
                    let scripts = page_scripts(&block, &shared, &uri);
                    register_scripts(&core, &shared, &scripts);
                    *shared.pending_scripts.borrow_mut() = Some(scripts);
                    Ok(())
                })),
                &mut token,
            )?;
        }

        // Safety net: if the registration above lost the race with the new
        // document (or the first navigation started before our hooks), run the
        // same scripts now — still before the page's own scripts in most cases.
        {
            let (block, shared) = (block.clone(), shared.clone());
            core.add_ContentLoading(
                &ContentLoadingEventHandler::create(Box::new(move |sender, _| {
                    let Some(core) = sender else { return Ok(()) };
                    let scripts = match shared.pending_scripts.borrow_mut().take() {
                        Some(s) => s,
                        None => {
                            let mut src = PWSTR::null();
                            core.Source(&mut src)?;
                            let src = take_pwstr(src);
                            if shared.page_url.borrow().is_empty() {
                                *shared.page_url.borrow_mut() = src.clone();
                            }
                            page_scripts(&block, &shared, &src)
                        }
                    };
                    if scripts.is_empty() {
                        return Ok(());
                    }
                    let probe = HSTRING::from(format!("window['{}']===1", *SCRIPT_KEY));
                    let c = core.clone();
                    // Translation follows the tab from page to page, whichever copy
                    // of the script ended up running in this document.
                    let lang = shared.translate.borrow().clone();
                    let follow = (!lang.is_empty()).then(|| serde_json::json!({ "mb": "tr", "on": true, "to": lang }).to_string());
                    core.ExecuteScript(
                        &probe,
                        &ExecuteScriptCompletedHandler::create(Box::new(move |res, ran| {
                            if res.is_ok() && ran != "true" {
                                for s in &scripts {
                                    let done = ExecuteScriptCompletedHandler::create(Box::new(|_, _| Ok(())));
                                    let _ = c.ExecuteScript(&HSTRING::from(s.as_str()), &done);
                                }
                            }
                            if let Some(msg) = &follow {
                                let _ = c.PostWebMessageAsJson(&HSTRING::from(msg.as_str()));
                            }
                            Ok(())
                        })),
                    )?;
                    Ok(())
                })),
                &mut token,
            )?;
        }

        // The page's content script talks to us: generic cosmetic rules are
        // answered here; SponsorBlock / translation / language go to the app.
        {
            let (block, shared, p) = (block.clone(), shared.clone(), proxy.clone());
            core.add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(move |sender, args| {
                    let (Some(core), Some(args)) = (sender, args) else { return Ok(()) };
                    let mut json = PWSTR::null();
                    args.WebMessageAsJson(&mut json)?;
                    let json = take_pwstr(json);
                    if json.len() > 2_000_000 {
                        return Ok(());
                    }
                    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&json) else { return Ok(()) };
                    match v["mb"].as_str() {
                        // Management pages (bookmarks, settings): the app checks the token and
                        // refuses documents loaded from the web; `src` is set here, not by the page.
                        Some("int") => {
                            let mut src = PWSTR::null();
                            if args.Source(&mut src).is_ok() {
                                v["src"] = take_pwstr(src).into();
                                let _ = p.send_event(UserEvent::Page(id, v));
                            }
                        }
                        Some("cos") => {
                            if !block.enabled.get() || shared.bypass.get() {
                                return Ok(());
                            }
                            let names = |k: &str| -> Vec<&str> {
                                v[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).take(20_000).collect()).unwrap_or_default()
                            };
                            let sel = block.generic(&names("c"), &names("i"), &shared.cos_exceptions.borrow());
                            if !sel.is_empty() {
                                let msg = serde_json::json!({ "mb": "cos", "s": sel }).to_string();
                                core.PostWebMessageAsJson(&HSTRING::from(msg))?;
                            }
                        }
                        Some("sb" | "tr" | "lang") => {
                            let _ = p.send_event(UserEvent::Page(id, v));
                        }
                        _ => {}
                    }
                    Ok(())
                })),
                &mut token,
            )?;
        }

        // The network blocker sees every request. (Per-context "*" filters never
        // fire in WebView2, so media is skipped inside the handler instead.)
        let all = HSTRING::from("*");
        match core.cast::<ICoreWebView2_22>() {
            Ok(c22) => c22.AddWebResourceRequestedFilterWithRequestSourceKinds(
                &all,
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
            )?,
            Err(_) => core.AddWebResourceRequestedFilter(&all, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)?,
        }
        let env = view.environment();
        let (block, shared, p) = (block.clone(), shared.clone(), proxy.clone());
        core.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut ctx = COREWEBVIEW2_WEB_RESOURCE_CONTEXT::default();
                args.ResourceContext(&mut ctx)?;
                if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT && serve_hold(&env, &args)? {
                    return Ok(());
                }
                if !block.enabled.get() || shared.bypass.get() {
                    return Ok(());
                }
                // Audio/video segments never carry ads by themselves (their manifests do).
                if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA {
                    return Ok(());
                }
                let req = args.Request()?;
                let mut uri = PWSTR::null();
                req.Uri(&mut uri)?;
                let uri = take_pwstr(uri);
                if !uri.starts_with("http") && !uri.starts_with("ws") {
                    return Ok(());
                }
                let kind = match ctx {
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT => {
                        // The page itself is never blocked: the user asked for it.
                        if shared.page_url.borrow().is_empty() {
                            *shared.page_url.borrow_mut() = uri.clone();
                        }
                        if uri == *shared.nav_url.borrow() || uri == *shared.page_url.borrow() {
                            return Ok(());
                        }
                        "subdocument"
                    }
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET => "stylesheet",
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE => "image",
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT => "font",
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT => "script",
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST | COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH => {
                        "xmlhttprequest"
                    }
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET => "websocket",
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING => "ping",
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_CSP_VIOLATION_REPORT => "csp_report",
                    _ => "other",
                };
                let mut method = PWSTR::null();
                req.Method(&mut method)?;
                let method = take_pwstr(method);
                let verdict = block.check(&uri, &shared.page_url.borrow(), kind, &method);
                if *DEBUG {
                    let v = match verdict {
                        Verdict::Allow => "allow",
                        Verdict::Block => "BLOCK",
                        Verdict::Redirect(..) => "REDIRECT",
                    };
                    // `MBROWSER_DEBUG=1`: every request with its verdict (for tuning blocklist.txt).
                    eprintln!("{v} {kind} {}", uri.chars().take(140).collect::<String>());
                }
                let resp = match verdict {
                    Verdict::Allow => return Ok(()),
                    Verdict::Block => env.CreateWebResourceResponse(None::<&IStream>, 403, w!("Blocked"), w!(""))?,
                    // uBO's neutered stand-in (noop.js, google-ima.js…): pages keep working.
                    Verdict::Redirect(mime, body) => {
                        let stream = SHCreateMemStream(Some(&body));
                        let headers = HSTRING::from(format!("Content-Type: {mime}\r\nAccess-Control-Allow-Origin: *"));
                        env.CreateWebResourceResponse(stream.as_ref(), 200, w!("OK"), &headers)?
                    }
                };
                args.SetResponse(&resp)?;
                shared.blocked.set(shared.blocked.get() + 1);
                if !shared.notify_pending.replace(true) {
                    let _ = p.send_event(UserEvent::Blocked(id));
                }
                Ok(())
            })),
            &mut token,
        )?;
    }
    Ok(())
}

/// Marks a document whose content script already ran (random per run).
static SCRIPT_KEY: LazyLock<String> = LazyLock::new(|| {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("__mb{:x}", (t as u64) ^ ((std::process::id() as u64) << 20))
});

/// Document-start scripts for a page: the site's uBO scriptlets (if any) and
/// content.js with its cosmetic rules. Scriptlets are a separate script, so a
/// broken one cannot take the rest down. Empty for non-web pages.
fn page_scripts(block: &BlockCtx, shared: &TabShared, url: &str) -> Vec<String> {
    let Some(host) = host_of(url).filter(|_| !is_hold(url)).map(|h| h.to_ascii_lowercase()) else {
        shared.cos_exceptions.borrow_mut().clear();
        return Vec::new();
    };
    let host_js = serde_json::to_string(&host).unwrap_or_default();
    let key = &*SCRIPT_KEY;
    let blocking = block.enabled.get() && !shared.bypass.get();
    let cosmetic = if blocking { block.cosmetic(url) } else { None };
    *shared.cos_exceptions.borrow_mut() = cosmetic.as_ref().map(|c| c.exceptions.clone()).unwrap_or_default();
    let mut out = Vec::with_capacity(2);
    if let Some(c) = cosmetic.as_ref().filter(|c| !c.scriptlets.is_empty()) {
        out.push(format!(
            "(()=>{{if(window['{key}s']||location.hostname!=={host_js})return;\
             Object.defineProperty(window,'{key}s',{{value:1}});const scriptletGlobals={{}};\n{}\n}})();",
            c.scriptlets
        ));
    }
    let base = cosmetic.as_ref().map_or(r#"{"hide":[],"proc":[],"generic":false}"#, |c| c.json.as_str());
    let extra = serde_json::json!({ "sb": block.sponsorblock.get(), "tr": *shared.translate.borrow() });
    out.push(format!(
        "(()=>{{if(window['{key}']||location.hostname!=={host_js})return;\
         Object.defineProperty(window,'{key}',{{value:1}});\n(function(CFG){{\n{CONTENT_JS}\n}})(Object.assign({base},{extra}));}})();"
    ));
    out
}

/// Swaps the document-start scripts: the previous page's are removed, the new
/// ones are added (asynchronously; a late answer for an older navigation is undone).
fn register_scripts(core: &ICoreWebView2, shared: &Rc<TabShared>, scripts: &[String]) {
    unsafe {
        for old in shared.script_ids.borrow_mut().drain(..) {
            let _ = core.RemoveScriptToExecuteOnDocumentCreated(&HSTRING::from(old.as_str()));
        }
        let generation = shared.script_gen.get().wrapping_add(1);
        shared.script_gen.set(generation);
        for s in scripts {
            let (sh, c) = (shared.clone(), core.clone());
            let done = AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(move |res, sid| {
                if res.is_ok() {
                    if sh.script_gen.get() == generation {
                        sh.script_ids.borrow_mut().push(sid);
                    } else {
                        let _ = c.RemoveScriptToExecuteOnDocumentCreated(&HSTRING::from(sid));
                    }
                }
                Ok(())
            }));
            let _ = core.AddScriptToExecuteOnDocumentCreated(&HSTRING::from(s.as_str()), &done);
        }
    }
}

/// Sends a JSON message to the page's content script.
pub fn post_json(view: &WebView, json: &str) {
    unsafe {
        let _ = view.webview().PostWebMessageAsJson(&HSTRING::from(json));
    }
}

/// Screenshots the visible page asynchronously; result arrives as `UserEvent::Preview`.
pub fn capture(view: &WebView, proxy: &Proxy) {
    unsafe {
        let Some(stream) = SHCreateMemStream(None) else { return };
        let (s, p) = (stream.clone(), proxy.clone());
        let done = CapturePreviewCompletedHandler::create(Box::new(move |result| {
            result?;
            s.Seek(0, STREAM_SEEK_SET, None)?;
            let mut bytes = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let mut n = 0u32;
                s.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut n)).ok()?;
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buf[..n as usize]);
            }
            let _ = p.send_event(UserEvent::Preview(base64(&bytes)));
            Ok(())
        }));
        let _ = view.webview().CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_JPEG, &stream, &done);
    }
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_rfc4648() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(super::base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn hold_url_round_trips() {
        for u in ["https://x.ru/a b?q=1+2&r=%2F#f", "https://пример.рф/путь?з=я", "http://h/"] {
            let h = super::hold_url(u);
            assert!(super::is_hold(&h));
            assert_eq!(super::hold_target(&h).as_deref(), Some(u));
        }
        assert_eq!(super::hold_target("https://x.ru/"), None);
        // Broken escapes are kept as they are.
        assert_eq!(super::hold_target("https://mb-restore.localhost/?u=a%zz%4").as_deref(), Some("a%zz%4"));
    }
}

/// Webview container windows (wry's "WRY_WEBVIEW" class) have no background
/// brush, so a new tab shows white until the engine presents its first frame.
/// A dark class brush makes that moment dark instead.
pub fn dark_container_class(view: &WebView) {
    use windows::Win32::{
        Foundation::COLORREF,
        Graphics::Gdi::CreateSolidBrush,
        UI::WindowsAndMessaging::{GCLP_HBRBACKGROUND, SetClassLongPtrW},
    };
    unsafe {
        let brush = CreateSolidBrush(COLORREF(0x0016_1211)); // RGB(17, 18, 22)
        SetClassLongPtrW(view.hwnd(), GCLP_HBRBACKGROUND, brush.0 as isize);
    }
}

/// Overlapping sibling webviews must respect z-order when painting; without
/// WS_CLIPSIBLINGS a webview lying *under* the page can briefly draw over it.
pub fn clip_siblings(view: &WebView) {
    use windows::Win32::UI::WindowsAndMessaging::{GWL_STYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_CLIPSIBLINGS};
    unsafe {
        let hwnd = view.hwnd();
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        SetWindowLongPtrW(hwnd, GWL_STYLE, style | WS_CLIPSIBLINGS.0 as isize);
    }
}

pub fn set_muted(view: &WebView, muted: bool) {
    unsafe {
        if let Ok(c8) = view.webview().cast::<ICoreWebView2_8>() {
            let _ = c8.SetIsMuted(muted);
        }
    }
}

pub fn stop(view: &WebView) {
    unsafe {
        let _ = view.webview().Stop();
    }
}

pub fn notify_moved(view: &WebView) {
    unsafe {
        let _ = view.controller().NotifyParentWindowPositionChanged();
    }
}
