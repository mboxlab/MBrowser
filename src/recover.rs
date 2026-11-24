//! Crash recovery: a tab's renderer died or hangs → reload it once, then show a
//! calm "page crashed" page; the engine (browser process) or the chrome UI died →
//! save the session and restart the app process. Child module of `app` so it can
//! reach `App` / `Tab` internals; the hooks elsewhere are one-liners.

use std::{
    collections::{HashMap, HashSet},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use webview2_com::{Microsoft::Web::WebView2::Win32::*, ProcessFailedEventHandler};
use windows::{
    Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
    core::{HSTRING, Interface},
};
use wry::{WebView, WebViewExtWindows};

use super::*;

/// A second renderer crash of the same tab within this time shows the crash page.
const AGAIN: Duration = Duration::from_secs(30);
/// How long an unresponsive page is given to come back before we offer a reload.
const HANG_WAIT: Duration = Duration::from_secs(20);
/// After this many restarts inside `WINDOW` seconds we stop restarting.
const MAX_RESTARTS: u32 = 2;
const WINDOW: u64 = 60;
/// "count,first-restart-unix-time", passed to the restarted process.
const ENV: &str = "MBROWSER_RESTARTS";
/// Event id of the chrome UI webview (tabs have ids >= 2).
pub const UI: u32 = 0;

#[derive(Debug)]
pub enum Ev {
    /// A webview process failed: (tab id or `UI`, process kind, reason).
    Failed(u32, i32, i32),
    /// The unresponsive page answered a script: it is alive.
    Alive(u32),
    /// `HANG_WAIT` passed since the page stopped responding.
    HangCheck(u32),
}

#[derive(Default)]
pub struct State {
    /// Last renderer crash per tab.
    crashed: HashMap<u32, Instant>,
    /// Tabs currently showing the crash page.
    shown: HashSet<u32>,
    /// Tabs whose renderer stopped responding (cleared when it answers).
    hung: HashSet<u32>,
    restarting: bool,
}

/// Hook `ProcessFailed` of a webview (content tab or the chrome UI).
pub unsafe fn hook(view: &WebView, id: u32, proxy: &Proxy) {
    let proxy = proxy.clone();
    let mut token = 0i64;
    unsafe {
        let _ = view.webview().add_ProcessFailed(
            &ProcessFailedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
                args.ProcessFailedKind(&mut kind)?;
                let mut reason = COREWEBVIEW2_PROCESS_FAILED_REASON::default();
                if let Ok(a2) = args.cast::<ICoreWebView2ProcessFailedEventArgs2>() {
                    let _ = a2.Reason(&mut reason);
                }
                let _ = proxy.send_event(UserEvent::Recover(Ev::Failed(id, kind.0, reason.0)));
                Ok(())
            })),
            &mut token,
        );
    }
}

/// Set on the process started by "Перезапустить и обновить".
const UPDATED_ENV: &str = "MBROWSER_UPDATED";

/// Start of a restarted process: give the dead engine (or the previous process)
/// a moment to release the profile.
pub fn on_start() {
    if restarted() {
        std::thread::sleep(Duration::from_millis(900));
    }
}

/// Whether this process is a crash-recovery or update restart (the session must
/// be restored).
pub fn restarted() -> bool {
    std::env::var_os(ENV).is_some() || std::env::var_os(UPDATED_ENV).is_some()
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// (restarts so far, start of the counting window) from the environment.
fn restart_count() -> (u32, u64) {
    let v = std::env::var(ENV).unwrap_or_default();
    let mut it = v.split(',');
    let n = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let t = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    if n > 0 && now_secs().saturating_sub(t) < WINDOW { (n, t) } else { (0, now_secs()) }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The dark "page crashed / not responding" page. The button reloads the real URL.
fn crash_page(url: &str, title: &str, hung: bool) -> String {
    let (head, text) = if hung {
        ("Страница не отвечает", "Вкладка слишком долго не откликается. Остальные вкладки работают как обычно.")
    } else {
        ("Страница упала", "Вкладка неожиданно закрылась. Остальные вкладки не пострадали.")
    };
    let u = serde_json::to_string(url).unwrap_or_else(|_| "\"\"".into()).replace("</", "<\\/");
    let shown = if url.chars().count() > 90 { format!("{}…", url.chars().take(90).collect::<String>()) } else { url.to_string() };
    format!(
        r#"<!doctype html><html lang="ru"><head><meta charset="utf-8"><title>{title}</title>
<style>
*{{box-sizing:border-box}}html,body{{height:100%;margin:0}}
body{{background:#111216;color:#e9e9ee;font:15px "Segoe UI Variable Text","Segoe UI",system-ui,sans-serif;display:grid;place-items:center;padding:24px}}
main{{max-width:460px;text-align:center}}
.ic{{width:64px;height:64px;margin:0 auto 22px;border-radius:50%;background:#1b1c22;border:1px solid #ffffff14;display:grid;place-items:center;
  color:#8a96ff;font:600 30px "Segoe UI Variable Display","Segoe UI",system-ui}}
h1{{font:600 24px "Segoe UI Variable Display","Segoe UI",system-ui;margin:0 0 10px}}
p{{margin:0 0 6px;color:#9a9ca6;line-height:1.55}}
.u{{font-size:12px;color:#6c6e78;word-break:break-all;margin-bottom:26px}}
button{{height:40px;padding:0 26px;border:0;border-radius:20px;background:#8a96ff;color:#14151a;font-family:inherit;font-size:14px;font-weight:600;cursor:pointer;transition:filter .12s}}
button:hover{{filter:brightness(1.1)}}button:active{{filter:brightness(.92)}}
</style></head><body><main>
<div class="ic">!</div><h1>{head}</h1><p>{text}</p><div class="u">{shown}</div>
<button id="r">Обновить</button></main>
<script>const U={u};document.getElementById('r').onclick=()=>{{if(U)location.replace(U)}};</script>
</body></html>"#,
        title = esc(title),
        shown = esc(&shown),
    )
}

impl App {
    pub(super) fn recover_event(&mut self, ev: Ev) {
        match ev {
            Ev::Failed(id, kind, reason) => self.proc_failed(id, kind, reason),
            Ev::Alive(id) => {
                self.recover.hung.remove(&id);
            }
            Ev::HangCheck(id) => {
                if self.recover.hung.remove(&id) {
                    self.give_up_on(id, true);
                }
            }
        }
    }

    fn proc_failed(&mut self, id: u32, kind: i32, reason: i32) {
        if *tab::DEBUG {
            eprintln!("process failed: view={id} kind={kind} reason={reason}");
        }
        if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED.0 {
            return self.restart("движок браузера завершился");
        }
        if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED.0 {
            return if id == UI { self.restart("упал интерфейс браузера") } else { self.tab_crashed(id) };
        }
        if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE.0 && id != UI {
            self.tab_hung(id);
        }
        // GPU / utility / subframe renderers: Chromium restarts those itself.
    }

    /// A tab's renderer died: reload once; a repeat within 30 s → crash page.
    fn tab_crashed(&mut self, id: u32) {
        let Some(i) = self.idx(id) else {
            if self.spare.as_ref().is_some_and(|s| s.tab.id == id) {
                self.drop_spare(); // the hidden prerender is not worth recovering
            }
            return;
        };
        let again = self.recover.crashed.get(&id).is_some_and(|t| t.elapsed() < AGAIN);
        self.recover.crashed.insert(id, Instant::now());
        self.recover.hung.remove(&id);
        self.tabs[i].loading = false;
        self.ui_dirty = true;
        if id != self.active {
            // Nobody is looking: drop the view, it reloads when the tab is activated.
            self.recover.shown.remove(&id);
            return self.sleep_tab(i);
        }
        if self.tabs[i].internal.is_some() {
            return self.rebuild_active(i); // our own pages: a fresh webview, no page
        }
        if again {
            return self.give_up_on(id, false);
        }
        self.recover.shown.remove(&id);
        if self.tabs[i].view.as_ref().is_none_or(|v| v.reload().is_err()) {
            self.rebuild_active(i);
        }
    }

    /// The renderer stopped responding: wait, then offer a reload (no dialogs).
    fn tab_hung(&mut self, id: u32) {
        let Some(i) = self.idx(id) else { return };
        if !self.recover.hung.insert(id) {
            return;
        }
        // Answers as soon as the renderer comes back to life.
        let p = self.proxy.clone();
        if let Some(v) = &self.tabs[i].view {
            let _ = v.evaluate_script_with_callback("1", move |_| {
                let _ = p.send_event(UserEvent::Recover(Ev::Alive(id)));
            });
        }
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(HANG_WAIT);
            let _ = p.send_event(UserEvent::Recover(Ev::HangCheck(id)));
        });
    }

    /// Replace the dead / hung page by the crash page (inactive tab: just sleep it).
    fn give_up_on(&mut self, id: u32, hung: bool) {
        let Some(i) = self.idx(id) else { return };
        if id != self.active {
            self.recover.shown.remove(&id);
            return self.sleep_tab(i);
        }
        let t = &self.tabs[i];
        let title = if t.title.is_empty() { t.display_title() } else { t.title.clone() };
        let page = crash_page(&t.url, &title, hung);
        match &t.view {
            Some(v) if v.load_html(&page).is_ok() => {
                self.recover.shown.insert(id);
            }
            _ => self.rebuild_active(i),
        }
    }

    /// Throw the active tab's webview away and create a new one (reloads the page).
    fn rebuild_active(&mut self, i: usize) {
        let (id, old) = (self.tabs[i].id, self.tabs[i].view.take());
        self.retire_view(id, old); // a download it started keeps going
        self.tabs[i].trimmed = false;
        self.ensure_view(i, true);
        let rect = self.content_rect();
        if let Some(v) = &self.tabs[i].view {
            let _ = v.set_bounds(rect);
            let _ = v.set_visible(true);
            if !self.ui_focus {
                let _ = v.focus();
            }
        }
    }

    /// Reload while the crash page is up must load the real URL, not the page itself.
    pub(super) fn recover_reload(&mut self, i: usize) -> bool {
        let t = &self.tabs[i];
        if !self.recover.shown.remove(&t.id) {
            return false;
        }
        let Some(v) = &t.view else { return false };
        // The user may have navigated away from the crash page since.
        if !v.url().is_ok_and(|u| u.is_empty() || u == "about:blank") || t.url.is_empty() {
            return false;
        }
        let _ = v.load_url(&t.url);
        true
    }

    /// The engine (or the chrome UI) is gone and every webview with it: save the
    /// session and start a fresh process of the same exe (same env), then exit.
    /// A restart is simpler and more robust than re-creating the environment, the
    /// chrome and every tab view in place. Gives up after 2 restarts in a minute.
    fn restart(&mut self, why: &str) {
        if std::mem::replace(&mut self.recover.restarting, true) {
            return; // every webview reports the same engine death
        }
        eprintln!("engine failure: {why}");
        self.shutdown();
        let (n, first) = restart_count();
        let spawned = n < MAX_RESTARTS
            && std::env::current_exe()
                .and_then(|exe| Command::new(exe).env(ENV, format!("{},{first}", n + 1)).spawn())
                .is_ok();
        // Automated checks (debug, background): no modal box that would take the focus.
        let quiet = cfg!(debug_assertions) && std::env::var_os("MBROWSER_BACKGROUND").is_some();
        if !spawned && !quiet {
            let text = format!(
                "Браузер не смог восстановиться после сбоя ({why}).\n\nВкладки сохранены — запустите {} снова.",
                crate::APP_NAME
            );
            unsafe {
                MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(crate::APP_NAME), MB_OK | MB_ICONERROR);
            }
        }
        std::process::exit(if spawned { 0 } else { 1 });
    }

    /// "Перезапустить и обновить": the new exe is already in place; save the
    /// session, start it (it restores the tabs) and leave.
    pub(super) fn restart_for_update(&mut self) {
        let Some((_, exe)) = crate::update::staged() else { return };
        self.shutdown();
        match Command::new(&exe).env(UPDATED_ENV, "1").spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => {
                let text = format!("Не удалось запустить новую версию:\n{e}\n\nОна запустится сама при следующем открытии браузера.");
                unsafe {
                    MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(crate::APP_NAME), MB_OK | MB_ICONERROR);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_page_escapes_and_keeps_url() {
        let p = crash_page("http://x/?a=</script><b>", "<i>t</i>", false);
        assert!(p.contains("&lt;i&gt;t&lt;/i&gt;"));
        assert!(!p.contains("</script><b>"));
        assert!(p.contains("Страница упала"));
        assert!(crash_page("http://x/", "t", true).contains("не отвечает"));
    }

    #[test]
    fn restart_window_expires() {
        // No variable → no restarts counted.
        assert_eq!(restart_count().0, 0);
    }
}
