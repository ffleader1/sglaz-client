//! Windows-only tray UI.
//!
//! On Windows the binary is built with the `windows` subsystem, so it starts
//! with no terminal at all. This module gives the daemon a face:
//!
//!   * a **system-tray icon** (bottom-right, next to the clock),
//!   * a **hidden log console** that captures all `println!`/`eprintln!` output
//!     even while hidden, so history is there when you open it,
//!   * **left-click the tray icon** to toggle that console show/hide,
//!   * **right-click → Quit** to actually stop the agent.
//!
//! The console's X (close) button is disabled: closing it can only hide it, it
//! can never kill the process. Only "Quit" ends the daemon.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Console::{
    AllocConsole, AttachConsole, GetConsoleWindow, ATTACH_PARENT_PROCESS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DeleteMenu, DispatchMessageW, GetMessageW, GetSystemMenu, SetForegroundWindow, ShowWindow,
    TranslateMessage, MENU_ITEM_FLAGS, MF_BYCOMMAND, MSG, SC_CLOSE, SW_HIDE, SW_SHOW,
};

/// Tracks whether the log console is currently visible.
static CONSOLE_VISIBLE: AtomicBool = AtomicBool::new(false);

/// Attach to the console of the launching terminal (cmd/PowerShell) so that
/// `-connect`/`--help` output is visible. No-op when double-clicked (no parent
/// console) — that path prints nowhere, which is fine for a GUI subsystem exe.
pub fn attach_parent_console() {
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn console_hwnd() -> HWND {
    unsafe { GetConsoleWindow() }
}

fn show_console() {
    let h = console_hwnd();
    if !h.0.is_null() {
        unsafe {
            let _ = ShowWindow(h, SW_SHOW);
            let _ = SetForegroundWindow(h);
        }
        CONSOLE_VISIBLE.store(true, Ordering::SeqCst);
    }
}

fn hide_console() {
    let h = console_hwnd();
    if !h.0.is_null() {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
        CONSOLE_VISIBLE.store(false, Ordering::SeqCst);
    }
}

fn toggle_console() {
    if CONSOLE_VISIBLE.load(Ordering::SeqCst) {
        hide_console();
    } else {
        show_console();
    }
}

/// Allocate a console for logs, hide it immediately, and disable its close (X)
/// button so it can only ever be hidden — never used to kill the process.
fn setup_console() {
    unsafe {
        let _ = AllocConsole();
    }
    // AllocConsole shows the window; hide it right away (brief flash at most).
    hide_console();

    let h = console_hwnd();
    if !h.0.is_null() {
        unsafe {
            let menu = GetSystemMenu(h, false);
            if !menu.is_invalid() {
                let _ = DeleteMenu(menu, SC_CLOSE, MENU_ITEM_FLAGS(MF_BYCOMMAND.0));
            }
        }
    }
}

/// A simple 32×32 teal-dot icon, generated in code so we don't ship an .ico.
fn make_icon() -> Result<Icon> {
    const SIZE: u32 = 32;
    let (cx, cy, r) = (16.0f32, 16.0f32, 14.0f32);
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let idx = ((y * SIZE + x) * 4) as usize;
            if dx * dx + dy * dy <= r * r {
                rgba[idx] = 0x2d;
                rgba[idx + 1] = 0xb0;
                rgba[idx + 2] = 0x9c;
                rgba[idx + 3] = 0xff;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).context("building tray icon")
}

/// Set up the hidden console + tray icon, run `daemon` on a background thread,
/// and pump the Windows message loop so the tray stays responsive. Never
/// returns normally — the process exits from the "Quit" menu item.
pub fn run(daemon: fn() -> Result<()>) -> Result<()> {
    setup_console();

    // The poll loop runs in the background. If it fails to start (e.g. not
    // enrolled), surface the console so the user can read the error.
    std::thread::spawn(move || {
        if let Err(e) = daemon() {
            eprintln!("sglaz: fatal: {:#}", e);
            show_console();
        }
    });

    let menu = Menu::new();
    let show_item = MenuItem::new("Show / hide logs", true, None);
    let quit_item = MenuItem::new("Quit sglaz", true, None);
    menu.append(&show_item).context("building tray menu")?;
    menu.append(&PredefinedMenuItem::separator())
        .context("building tray menu")?;
    menu.append(&quit_item).context("building tray menu")?;

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("sglaz — env sync agent")
        .with_icon(make_icon()?)
        .build()
        .context("creating tray icon")?;

    let menu_rx = MenuEvent::receiver();
    let tray_rx = TrayIconEvent::receiver();
    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();

    // Standard Win32 message loop. Tray/menu events arrive as window messages;
    // after dispatching we drain the crossbeam channels tray-icon posts to.
    unsafe {
        let mut msg = MSG::default();
        loop {
            let ret = GetMessageW(&mut msg, HWND::default(), 0, 0);
            if ret.0 <= 0 {
                break; // WM_QUIT (0) or error (-1)
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            while let Ok(ev) = menu_rx.try_recv() {
                if ev.id == quit_id {
                    std::process::exit(0);
                } else if ev.id == show_id {
                    toggle_console();
                }
            }
            while let Ok(ev) = tray_rx.try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = ev
                {
                    toggle_console();
                }
            }
        }
    }
    Ok(())
}
