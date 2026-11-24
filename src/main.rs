#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Meteor Browser (MBrowser) — a fast, lightweight browser: Rust shell over the system
//! WebView2 (Chromium) engine. Tabs sleep when idle, ads and trackers are
//! cut at the network level, startup is instant even with many tabs.

mod app;
mod blocker;
mod defext;
mod ext;
mod glass;
mod iconcache;
mod internal;
mod net;
mod omni;
mod pages;
mod shell;
mod store;
mod tab;
mod xpopup;
mod translate;
mod update;

use std::time::{Duration, Instant};

use tao::{
    event::{ElementState, Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use windows::{
    Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
    core::HSTRING,
};

/// Housekeeping interval: tab sleeping + saving state.
const TICK: Duration = Duration::from_secs(20);

/// User-visible name. The short internal name (exe, data folders, env vars)
/// stays `mbrowser` / `MBrowser` (M = Meteor), so existing profiles keep working.
pub const APP_NAME: &str = "Meteor Browser";

fn main() {
    // Restarted by the previous process (crash recovery, update): let it finish
    // exiting first, or this one would hand its request over to it and quit.
    app::recover::on_start();
    let event_loop = EventLoopBuilder::<tab::UserEvent>::with_user_event().build();
    // A second launch on the same profile hands its URL to the running browser and exits.
    if !shell::single_instance(event_loop.create_proxy()) {
        return;
    }
    update::cleanup();
    let mut app = match app::App::new(&event_loop) {
        Ok(app) => app,
        Err(e) => {
            let text = format!(
                "Не удалось запустить браузер: {e}\n\n\
                 Нужен Microsoft Edge WebView2 Runtime:\n\
                 https://developer.microsoft.com/microsoft-edge/webview2/"
            );
            unsafe {
                MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(APP_NAME), MB_OK | MB_ICONERROR);
            }
            return;
        }
    };

    let mut next_tick = Instant::now() + TICK;
    event_loop.run(move |event, _, control_flow| {
        if let Event::NewEvents(StartCause::ResumeTimeReached { .. }) = event {
            if Instant::now() >= next_tick {
                app.tick();
                next_tick = Instant::now() + TICK;
            }
            app.poll_downloads();
        }
        // Housekeeping, plus a check of the running downloads every few seconds.
        let wake = app.dl_poll_at().map_or(next_tick, |t| t.min(next_tick));
        *control_flow = ControlFlow::WaitUntil(wake);
        match event {
            Event::NewEvents(_) => {}
            Event::WindowEvent { window_id, event, .. } if app.is_glass(window_id) => app.glass_event(&event),
            Event::WindowEvent { window_id, event, .. } if app.is_xpop(window_id) => app.xpop_event(&event),
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    app.shutdown();
                    *control_flow = ControlFlow::Exit;
                }
                WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => app.layout(),
                WindowEvent::Moved(_) => app.moved(),
                WindowEvent::Focused(true) => app.restore_focus(),
                WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                    app.frame_key(event.physical_key, event.repeat)
                }
                _ => {}
            },
            Event::UserEvent(ev) => app.handle(ev),
            Event::MainEventsCleared => app.flush_ui(),
            _ => {}
        }
    });
}
