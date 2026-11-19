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
    let event_loop = EventLoopBuilder::<tab::UserEvent>::with_user_event().build();
    // A second launch on the same profile hands its URL to the running browser and exits.
    if !shell::single_instance(event_loop.create_proxy()) {
        return;
    }
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
        *control_flow = ControlFlow::WaitUntil(next_tick);
        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                app.tick();
                next_tick = Instant::now() + TICK;
                *control_flow = ControlFlow::WaitUntil(next_tick);
            }
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
