//! Extension popups: the page an extension shows when its toolbar button is
//! clicked (SponsorBlock's panel, Tampermonkey's script list), in a small
//! borderless window under the button. WebView2 has no UI of its own for
//! extension buttons or popups.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::Value;
use tao::{
    event_loop::EventLoopWindowTarget,
    platform::windows::{WindowBuilderExtWindows, WindowExtWindows},
    window::{Window, WindowBuilder, WindowId},
};
use webview2_com::{FocusChangedEventHandler, Microsoft::Web::WebView2::Win32::ICoreWebView2Environment};
use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Dwm::{DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute},
    UI::WindowsAndMessaging::{
        GetClientRect, SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos, ShowWindow,
    },
};
use wry::{
    NewWindowResponse, Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows,
    dpi::{PhysicalPosition, PhysicalSize},
};

use crate::{
    ext,
    tab::{Proxy, UserEvent},
};

pub struct Popup {
    /// Declared first: dropped before its window.
    view: Option<WebView>,
    /// Extension whose popup is open ("" = none).
    pub ext: String,
    /// Screen px: right edge of the toolbar button, bottom of the toolbar.
    anchor: (i32, i32),
    scale: f64,
    size: (u32, u32),
    shown: Option<Instant>,
    /// It has had the keyboard focus since it was shown (so losing it means a click away).
    focused: bool,
    ticks: u32,
    /// Bumped on every open/close; stale timer ticks and callbacks are ignored.
    generation: Arc<AtomicU32>,
    window: Window,
}

impl Popup {
    pub fn new<T: 'static>(target: &EventLoopWindowTarget<T>, owner: &Window) -> Option<Self> {
        let window = WindowBuilder::new()
            .with_title("MBrowser")
            .with_decorations(false)
            .with_undecorated_shadow(true)
            .with_resizable(false)
            .with_visible(false)
            .with_skip_taskbar(true)
            .with_owner_window(owner.hwnd())
            .build(target)
            .ok()?;
        unsafe {
            let round = DWMWCP_ROUND.0 as u32;
            let _ = DwmSetWindowAttribute(
                HWND(window.hwnd() as _),
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&round as *const u32).cast(),
                4,
            );
        }
        Some(Self {
            view: None,
            ext: String::new(),
            anchor: (0, 0),
            scale: 1.0,
            size: (0, 0),
            shown: None,
            focused: false,
            ticks: 0,
            generation: Arc::default(),
            window,
        })
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn is_open(&self) -> bool {
        self.view.is_some()
    }

    /// Loads the popup hidden; it is shown once its size is known (see `apply`).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &mut self,
        env: &ICoreWebView2Environment,
        ext_id: &str,
        url: &str,
        page_url: &str,
        anchor: (i32, i32),
        scale: f64,
        proxy: &Proxy,
    ) {
        self.close();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.ext = ext_id.to_string();
        self.anchor = anchor;
        self.scale = scale;
        self.size = ((360.0 * scale) as u32, (480.0 * scale) as u32);
        self.ticks = 0;
        self.focused = false;
        self.place();
        let p = proxy.clone();
        let built = WebViewBuilder::new()
            .with_environment(env.clone())
            .with_bounds(Rect {
                position: PhysicalPosition::new(0, 0).into(),
                size: PhysicalSize::new(self.size.0, self.size.1).into(),
            })
            .with_focused(false) // hidden until sized; focus comes with the show
            .with_initialization_script(&ext::popup_script(page_url))
            .with_new_window_req_handler(move |url, _| {
                // Links in a popup ("Help", "Website") open as tabs.
                let _ = p.send_event(UserEvent::OpenTab { url, opener: 0 });
                NewWindowResponse::Deny
            })
            .with_devtools(cfg!(debug_assertions))
            .build_as_child(&self.window);
        let view = match built {
            Ok(v) => v,
            Err(e) => {
                eprintln!("extension popup failed: {e}");
                return;
            }
        };
        // Focus leaving the popup (a click elsewhere) closes it, like a real popup.
        let p = proxy.clone();
        let mut token = 0i64;
        unsafe {
            let p2 = proxy.clone();
            let _ = view.controller().add_GotFocus(
                &FocusChangedEventHandler::create(Box::new(move |_, _| {
                    let _ = p2.send_event(UserEvent::PopupFocus(generation));
                    Ok(())
                })),
                &mut token,
            );
            let _ = view.controller().add_LostFocus(
                &FocusChangedEventHandler::create(Box::new(move |_, _| {
                    let _ = p.send_event(UserEvent::PopupBlur(generation));
                    Ok(())
                })),
                &mut token,
            );
        }
        // Navigated after creation: given at creation, an extension URL stays on about:blank.
        if let Err(e) = view.load_url(url) {
            eprintln!("extension popup: {e}");
        }
        self.view = Some(view);
        // Measure the page a few times a second while open: first to size and
        // show it, then to follow its size changes (and Esc).
        let (g, p) = (self.generation.clone(), proxy.clone());
        std::thread::spawn(move || {
            while g.load(Ordering::SeqCst) == generation {
                std::thread::sleep(Duration::from_millis(150));
                if g.load(Ordering::SeqCst) != generation || p.send_event(UserEvent::PopupTick(generation)).is_err() {
                    break;
                }
            }
        });
    }

    pub fn tick(&mut self, proxy: &Proxy) {
        let Some(view) = &self.view else { return };
        self.ticks += 1;
        let (p, generation) = (proxy.clone(), self.generation());
        let _ = view.evaluate_script_with_callback(ext::POPUP_MEASURE, move |res| {
            let _ = p.send_event(UserEvent::PopupSize(generation, res));
        });
        // A page that never reports a size is shown anyway at the default one.
        if self.shown.is_none() && self.ticks > 10 {
            self.show();
        }
    }

    /// Applies a measurement; true when the popup asked to close (Esc).
    pub fn apply(&mut self, res: &str) -> bool {
        // The script's value comes back JSON-encoded: a string holding JSON.
        let inner: String = serde_json::from_str(res).unwrap_or_default();
        let v: Value = serde_json::from_str(&inner).unwrap_or_default();
        if v["close"].as_bool() == Some(true) {
            return true;
        }
        let (w, h) = (v["w"].as_f64().unwrap_or(0.0), v["h"].as_f64().unwrap_or(0.0));
        if w >= 1.0 && h >= 1.0 {
            let size = (
                (w.clamp(25.0, 800.0) * self.scale).round() as u32,
                (h.clamp(25.0, 600.0) * self.scale).round() as u32,
            );
            if size != self.size {
                self.size = size;
                self.place();
            }
            if self.shown.is_none() {
                self.show();
            }
        }
        false
    }

    /// Under the button, right-aligned to it, never off the left screen edge.
    fn place(&self) {
        let (w, h) = self.size;
        let left = self.window.current_monitor().map_or(0, |m| m.position().x);
        let x = (self.anchor.0 - w as i32).max(left + 4);
        let hwnd = HWND(self.window.hwnd() as _);
        unsafe {
            let _ = SetWindowPos(hwnd, None, x, self.anchor.1, w as i32, h as i32, SWP_NOACTIVATE | SWP_NOZORDER);
            // Whatever frame the window keeps, the page gets exactly w × h.
            let mut c = RECT::default();
            if GetClientRect(hwnd, &mut c).is_ok() {
                let (dw, dh) = (w as i32 - (c.right - c.left), h as i32 - (c.bottom - c.top));
                if dw > 0 || dh > 0 {
                    let (dw, dh) = (dw.max(0), dh.max(0));
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        x - dw,
                        self.anchor.1,
                        w as i32 + dw,
                        h as i32 + dh,
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    );
                }
            }
        }
        if let Some(v) = &self.view {
            let _ = v.set_bounds(Rect {
                position: PhysicalPosition::new(0, 0).into(),
                size: PhysicalSize::new(w, h).into(),
            });
        }
    }

    fn show(&mut self) {
        // Shown directly (tao's own show is not used, so hide stays in step):
        // activated, so the popup takes the keyboard. Automated UI checks never
        // take the user's focus.
        let quiet = cfg!(debug_assertions) && std::env::var_os("MBROWSER_BACKGROUND").is_some();
        unsafe {
            let _ = ShowWindow(HWND(self.window.hwnd() as _), if quiet { SW_SHOWNOACTIVATE } else { SW_SHOW });
        }
        if let Some(v) = self.view.as_ref().filter(|_| !quiet) {
            let _ = v.focus();
        }
        self.shown = Some(Instant::now());
    }

    pub fn got_focus(&mut self) {
        if self.shown.is_some() {
            self.focused = true;
        }
    }

    /// Losing the focus closes it only once it really had it after showing.
    pub fn settled(&self) -> bool {
        self.focused && self.shown.is_some_and(|t| t.elapsed() > Duration::from_millis(150))
    }

    pub fn close(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.view = None;
        if self.shown.take().is_some() {
            unsafe {
                let _ = ShowWindow(HWND(self.window.hwnd() as _), SW_HIDE);
            }
        }
        self.ext.clear();
    }
}
