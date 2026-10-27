//! Docks each terminal's own top-level Chrome window over the right-hand browser pane.
//!
//! Cross-process `SetParent`/`WS_CHILD` hosting paints Chrome but breaks its omnibox and
//! accelerators, so the real Chrome window stays top-level and follows the pane rectangle.
use crate::browser::{BrowserInfo, BrowserRect, BrowserSupervisor};
use parking_lot::Mutex;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tauri::{AppHandle, Manager};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT},
    Graphics::{
        Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS},
        Gdi::ClientToScreen,
    },
    UI::WindowsAndMessaging::*,
};

#[derive(Clone)]
pub struct View {
    pub instance_id: String,
    pub generation: u64,
    pub rect: BrowserRect,
}
#[derive(Default)]
pub struct BrowserHost {
    view: Mutex<Option<View>>,
}
struct Saved {
    ex_style: isize,
    rect: RECT,
}
struct Scan {
    pid: u32,
    windows: Vec<HWND>,
}

unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> i32 {
    let scan = &mut *(data as *mut Scan);
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    let mut class = [0u16; 64];
    let length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    let is_chrome = String::from_utf16_lossy(&class[..length.max(0) as usize]) == "Chrome_WidgetWin_1";
    let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
    // Menus, tooltips and other helper surfaces are Chrome_WidgetWin_1 too; browser windows have a caption.
    if pid == scan.pid && is_chrome && style & WS_CAPTION == WS_CAPTION {
        scan.windows.push(hwnd);
    }
    1
}
fn windows_of(pid: u32) -> Vec<HWND> {
    let mut scan = Scan { pid, windows: Vec::new() };
    unsafe { EnumWindows(Some(collect), &mut scan as *mut _ as LPARAM) };
    scan.windows
}
fn frame_insets(hwnd: HWND) -> (i32, i32, i32, i32) {
    let (mut window, mut frame) = (RECT::default(), RECT::default());
    unsafe {
        GetWindowRect(hwnd, &mut window);
        if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS as u32, &mut frame as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32) != 0 {
            return (0, 0, 0, 0);
        }
    }
    (frame.left - window.left, frame.top - window.top, window.right - frame.right, window.bottom - frame.bottom)
}
fn is_above(main: HWND, window: HWND) -> bool {
    let mut current = main;
    for _ in 0..256 {
        current = unsafe { GetWindow(current, GW_HWNDPREV) };
        if current.is_null() {
            return false;
        }
        if current == window {
            return true;
        }
    }
    false
}
fn place(hwnd: HWND, x: i32, y: i32, width: i32, height: i32) -> Result<(), String> {
    let (left, top, right, bottom) = frame_insets(hwnd);
    let target = (x - left, y - top, width + left + right, height + top + bottom);
    let mut current = RECT::default();
    unsafe {
        if IsIconic(hwnd) != 0 || IsZoomed(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        GetWindowRect(hwnd, &mut current);
        let moved = (current.left, current.top, current.right - current.left, current.bottom - current.top) != target;
        if !moved && IsWindowVisible(hwnd) != 0 {
            return Ok(());
        }
        let flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW | if moved { 0 } else { SWP_NOMOVE | SWP_NOSIZE };
        if SetWindowPos(hwnd, std::ptr::null_mut(), target.0, target.1, target.2, target.3, flags) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(())
}

impl BrowserHost {
    pub fn set_view(&self, view: Option<View>) {
        *self.view.lock() = view;
    }
    pub fn start(self: &Arc<Self>, app: AppHandle, browser: Arc<BrowserSupervisor>) {
        let host = self.clone();
        std::thread::spawn(move || {
            let mut saved: HashMap<isize, Saved> = HashMap::new();
            loop {
                let rows = browser.snapshot();
                let headed = rows.iter().any(|row| row.pid.is_some() && !row.headless);
                if !headed && saved.is_empty() {
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
                host.reconcile(&app, &browser, &rows, &mut saved);
                std::thread::sleep(Duration::from_millis(40));
            }
        });
    }
    fn reconcile(&self, app: &AppHandle, browser: &BrowserSupervisor, rows: &[BrowserInfo], saved: &mut HashMap<isize, Saved>) {
        let Some(window) = app.get_webview_window("main") else { return };
        let Ok(main) = window.hwnd() else { return };
        let main = main.0 as HWND;
        let view = self.view.lock().clone();
        let live: Vec<(&BrowserInfo, Vec<HWND>)> = rows.iter().filter(|row| row.pid.is_some() && !row.headless).map(|row| (row, windows_of(row.pid.unwrap()))).collect();
        saved.retain(|hwnd, _| live.iter().any(|(_, windows)| windows.contains(&(*hwnd as HWND))));
        let dock = Dock::capture(main, view);
        for (info, windows) in &live {
            for &hwnd in windows {
                if info.embedded {
                    dock.embed(browser, saved, info, hwnd);
                } else {
                    restore_window(saved, hwnd, &dock.origin);
                }
            }
        }
    }
}

/// Per-pass host state: the main window, the pane view and the client-area origin.
struct Dock {
    main: HWND,
    foreground: HWND,
    minimized: bool,
    origin: POINT,
    view: Option<View>,
}
impl Dock {
    fn capture(main: HWND, view: Option<View>) -> Self {
        unsafe {
            let foreground = GetForegroundWindow();
            let minimized = IsIconic(main) != 0;
            let mut origin = POINT { x: 0, y: 0 };
            ClientToScreen(main, &mut origin);
            Self { main, foreground, minimized, origin, view }
        }
    }
    fn embed(&self, browser: &BrowserSupervisor, saved: &mut HashMap<isize, Saved>, info: &BrowserInfo, hwnd: HWND) {
        let key = hwnd as isize;
        if !saved.contains_key(&key) {
            saved.insert(key, take_over_window(hwnd));
        }
        let active = self.view.as_ref().filter(|v| v.instance_id == info.instance_id && v.generation == info.generation && !self.minimized && v.rect.width >= 1.0 && v.rect.height >= 1.0);
        match active {
            Some(v) => self.dock_into(browser, info, hwnd, v),
            None => hide_window(hwnd),
        }
    }
    fn dock_into(&self, browser: &BrowserSupervisor, info: &BrowserInfo, hwnd: HWND, v: &View) {
        if let Err(error) = place(hwnd, self.origin.x + v.rect.x as i32, self.origin.y + v.rect.y as i32, v.rect.width as i32, v.rect.height as i32) {
            browser.report_embedding_error(&info.instance_id, info.generation, &format!("{}: {error}", crate::browser::EMBEDDING_ERROR));
        } else if self.foreground == self.main && !is_above(self.main, hwnd) {
            // Reordering our own host works where cross-process HWND_TOP is ignored.
            unsafe { SetWindowPos(self.main, hwnd, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
        }
    }
}

/// Records the window's rect and style, then hides it from the taskbar.
fn take_over_window(hwnd: HWND) -> Saved {
    unsafe {
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect);
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style | WS_EX_TOOLWINDOW as isize);
        Saved { ex_style, rect }
    }
}
fn hide_window(hwnd: HWND) {
    unsafe {
        if IsWindowVisible(hwnd) != 0 {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
}
fn restore_window(saved: &mut HashMap<isize, Saved>, hwnd: HWND, origin: &POINT) {
    let Some(original) = saved.remove(&(hwnd as isize)) else { return };
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, original.ex_style & !(WS_EX_TOOLWINDOW as isize));
        let (x, y, width, height) = if original.rect.left < -20000 {
            (origin.x + 60, origin.y + 60, 1200, 800)
        } else {
            (original.rect.left, original.rect.top, original.rect.right - original.rect.left, original.rect.bottom - original.rect.top)
        };
        SetWindowPos(hwnd, std::ptr::null_mut(), x, y, width, height, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED);
        ShowWindow(hwnd, SW_SHOWNORMAL);
    }
}
