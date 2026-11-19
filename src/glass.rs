//! Live blur under the Tablo.
//!
//! A webview cannot blur the pixels of another webview, and the Windows
//! compositor's own effects cannot sample WebView2 content either. What *can*
//! see it is the system backdrop of a top-level window: Windows 11 blurs
//! whatever lies under the window, on the GPU, every frame (the "acrylic" of
//! its own menus). So the Tablo lives in a borderless, transparent window owned
//! by the browser window and laid exactly over the page area; the page keeps
//! playing underneath and shows through, blurred.
//!
//! The window never takes activation from the browser (typing stays in the
//! address bar); since system backdrops only draw for active windows, it is
//! told to look active instead.

use tao::{
    event_loop::EventLoopWindowTarget,
    platform::windows::{WindowBuilderExtWindows, WindowExtWindows},
    window::{Window, WindowBuilder, WindowId},
};
use windows::{
    Win32::{
        Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Dwm::{
            DWMSBT_TRANSIENTWINDOW, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_SYSTEMBACKDROP_TYPE,
            DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWINDOWATTRIBUTE,
            DwmSetWindowAttribute,
        },
        System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW},
        UI::{
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::{
                SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetForegroundWindow,
                SetWindowPos, ShowWindow, WM_ERASEBKGND, WM_NCACTIVATE,
            },
        },
    },
    core::{HSTRING, PCWSTR},
};
use wry::{
    Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows,
    dpi::{PhysicalPosition, PhysicalSize},
};

use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment;

use crate::tab::{self, Proxy, UserEvent};

pub struct Glass {
    /// Second, transparent instance of the chrome page (it draws the Tablo).
    /// Declared first: dropped before its window.
    pub ui: Option<WebView>,
    /// The page inside has loaded and said "ready".
    pub ready: bool,
    pub shown: bool,
    window: Window,
}

impl Glass {
    /// The (hidden) window; cheap. The webview comes later, see `create_ui`.
    pub fn new<T: 'static>(target: &EventLoopWindowTarget<T>, owner: &Window) -> Option<Self> {
        let window = WindowBuilder::new()
            .with_title(crate::APP_NAME)
            .with_decorations(false)
            .with_undecorated_shadow(false)
            .with_transparent(true)
            // Erased with black, which the transparent surface treats as "see-through".
            .with_background_color((0, 0, 0, 0))
            .with_focusable(false) // WS_EX_NOACTIVATE: clicks don't steal the address bar's focus
            .with_visible(false)
            .with_skip_taskbar(true)
            .with_owner_window(owner.hwnd())
            .build(target)
            .ok()?;
        let hwnd = HWND(window.hwnd() as _);
        let set = |attr: DWMWINDOWATTRIBUTE, v: u32| unsafe {
            let _ = DwmSetWindowAttribute(hwnd, attr, (&v as *const u32).cast(), 4);
        };
        set(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
        set(DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_TRANSIENTWINDOW.0 as u32);
        set(DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND.0 as u32);
        set(DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE);
        Some(Self { ui: None, ready: false, shown: false, window })
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    fn hwnd(&self) -> HWND {
        HWND(self.window.hwnd() as _)
    }

    /// Loads the chrome page in "glass" mode (transparent, Tablo only).
    pub fn create_ui(&mut self, env: &ICoreWebView2Environment, html: &str, proxy: &Proxy) {
        if self.ui.is_some() {
            return;
        }
        let size = self.window.inner_size();
        let p = proxy.clone();
        let built = WebViewBuilder::new()
            .with_environment(env.clone())
            .with_transparent(true)
            .with_focused(false)
            .with_bounds(Rect {
                position: PhysicalPosition::new(0, 0).into(),
                size: PhysicalSize::new(size.width.max(1), size.height.max(1)).into(),
            })
            .with_initialization_script("window.__glass = true;")
            .with_html(html)
            .with_navigation_handler(|url| url.starts_with("about:") || url.starts_with("data:"))
            .with_ipc_handler(move |req| {
                let _ = p.send_event(UserEvent::GlassIpc(req.into_body()));
            })
            .with_hotkeys_zoom(false)
            .with_browser_accelerator_keys(false)
            .with_devtools(cfg!(debug_assertions))
            .build_as_child(&self.window);
        match built {
            Ok(ui) => {
                unsafe {
                    let _ = tab::hook_keys(&ui, proxy);
                    // The webview container shares a class whose brush is dark (see
                    // tab::dark_container_class); here it must stay unpainted.
                    let _ = SetWindowSubclass(ui.hwnd(), Some(no_erase), 1, 0);
                }
                self.ui = Some(ui);
            }
            Err(e) => eprintln!("glass webview failed: {e}"),
        }
    }

    /// Covers this screen rectangle (physical pixels).
    pub fn place(&self, x: i32, y: i32, w: u32, h: u32) {
        unsafe {
            let _ = SetWindowPos(self.hwnd(), None, x, y, w as i32, h as i32, SWP_NOACTIVATE | SWP_NOZORDER);
        }
        if let Some(ui) = &self.ui {
            let _ = ui.set_bounds(Rect {
                position: PhysicalPosition::new(0, 0).into(),
                size: PhysicalSize::new(w, h).into(),
            });
        }
    }

    pub fn show(&mut self) {
        unsafe {
            // Directly, not through tao: its "show" would activate the window.
            let _ = ShowWindow(self.hwnd(), SW_SHOWNOACTIVATE);
        }
        self.keep_active_look();
        self.shown = true;
    }

    pub fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd(), SW_HIDE);
        }
        self.shown = false;
    }

    /// System backdrops render only for active windows (inactive ones get a flat
    /// fill). The glass is never the active window, so it claims to look active.
    pub fn keep_active_look(&self) {
        unsafe {
            SendMessageW(self.hwnd(), WM_NCACTIVATE, Some(WPARAM(1)), Some(LPARAM(0)));
        }
    }

    /// Keyboard into the Tablo itself (its add/edit form).
    pub fn take_focus(&self) {
        unsafe {
            let _ = SetForegroundWindow(self.hwnd());
        }
        if let Some(ui) = &self.ui {
            let _ = ui.focus();
        }
    }
}

unsafe extern "system" fn no_erase(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM, _: usize, _: usize) -> LRESULT {
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }
    unsafe { DefSubclassProc(hwnd, msg, w, l) }
}

/// Live blur needs Windows 11 22H2+ (system backdrops) with "Transparency
/// effects" on; otherwise the Tablo keeps using a blurred snapshot.
pub fn supported() -> bool {
    let build = reg_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "CurrentBuildNumber")
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let transparency =
        reg_dword(HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "EnableTransparency")
            .unwrap_or(1);
    build >= 22621 && transparency != 0
}

fn reg_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    let (mut v, mut size) = (0u32, 4u32);
    let (key, value) = (HSTRING::from(key), HSTRING::from(value));
    let r = unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut v as *mut u32).cast()),
            Some(&mut size),
        )
    };
    (r == ERROR_SUCCESS).then_some(v)
}

fn reg_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let mut buf = [0u16; 64];
    let mut size = (buf.len() * 2) as u32;
    let (key, value) = (HSTRING::from(key), HSTRING::from(value));
    let r = unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_the_windows_build() {
        assert!(super::reg_string(
            super::HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "CurrentBuildNumber"
        )
        .is_some_and(|b| b.parse::<u32>().is_ok()));
    }
}
