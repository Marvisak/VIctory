//! The progress window shown while shaders and pipelines are made before
//! the title starts (`crate::startup`): what is being done, how far, how
//! long is left, and the last log lines ([`crate::progress`]), redrawn four
//! times a second. Plain GDI on a thread of its own: nothing of the GPU's,
//! which is busy with the work. Esc or closing it starts the title at once
//! ([`skipped`]).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DeleteObject,
    DrawTextW, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetClientRect, GetMessageW, IDC_ARROW, KillTimer, LoadCursorW, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, SW_SHOW, SetTimer, ShowWindow, TranslateMessage,
    WM_APP, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_PAINT, WM_TIMER, WNDCLASSW,
    WS_CAPTION, WS_MINIMIZEBOX, WS_SYSMENU, WS_VISIBLE,
};

/// Esc or the window's close button was pressed.
static SKIPPED: AtomicBool = AtomicBool::new(false);
/// Asks the window to go without skipping.
const WM_DONE: u32 = WM_APP + 1;

/// Whether the player asked to start the title now.
pub(crate) fn skipped() -> bool {
    SKIPPED.load(Ordering::Relaxed)
}

/// The window, on its thread.
pub(crate) struct Splash {
    hwnd: std::sync::Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Shows the window, titled `title`.
pub(crate) fn show(title: &str) -> Splash {
    let hwnd = std::sync::Arc::new(AtomicUsize::new(0));
    let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
    let thread = {
        let hwnd = hwnd.clone();
        std::thread::Builder::new()
            .name("victory progress window".into())
            .spawn(move || run(&title, &hwnd))
            .ok()
    };
    Splash { hwnd, thread }
}

impl Splash {
    /// Closes the window and waits for its thread.
    pub(crate) fn close(mut self) {
        // The window may not be made yet: wait for it a little.
        for _ in 0..50 {
            let hwnd = self.hwnd.load(Ordering::Acquire);
            if hwnd != 0 {
                // SAFETY: a window of ours; its thread destroys it.
                unsafe { PostMessageW(hwnd as HWND, WM_DONE, 0, 0) };
                break;
            }
            if self.thread.as_ref().is_none_or(|t| t.is_finished()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(title: &[u16], out: &AtomicUsize) {
    let class: Vec<u16> = "VIctoryProgress\0".encode_utf16().collect();
    // SAFETY: plain window creation and its message loop on this thread.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: instance,
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            760,
            420,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }
        ShowWindow(hwnd, SW_SHOW);
        SetTimer(hwnd, 1, 250, None);
        out.store(hwnd as usize, Ordering::Release);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// The lines the window shows, top to bottom.
pub(crate) fn lines(s: &crate::progress::State) -> Vec<String> {
    let mut out = vec![
        if s.phase.is_empty() {
            "Preparing".to_owned()
        } else {
            s.phase.clone()
        },
        s.file.clone(),
        s.line(),
        String::new(),
    ];
    out.extend(s.log.iter().cloned());
    out.push(String::new());
    out.push(
        "Kept on disk: the next start skips this. Esc starts the game now (the rest goes on \
         behind it)."
            .to_owned(),
    );
    out
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: (all) messages of our window, on its thread.
    unsafe {
        match msg {
            WM_TIMER => {
                InvalidateRect(hwnd, std::ptr::null(), 0);
                0
            }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_KEYDOWN if wparam == usize::from(VK_ESCAPE) => {
                SKIPPED.store(true, Ordering::Relaxed);
                DestroyWindow(hwnd);
                0
            }
            WM_CLOSE => {
                SKIPPED.store(true, Ordering::Relaxed);
                DestroyWindow(hwnd);
                0
            }
            WM_DONE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                KillTimer(hwnd, 1);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Draws the progress: a dark background, the phase large, the rest small.
unsafe fn paint(hwnd: HWND) {
    // SAFETY: (all) painting our window between BeginPaint and EndPaint,
    // with objects made and deleted here.
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        let background = CreateSolidBrush(0x0020_1A18);
        FillRect(dc, &client, background);
        DeleteObject(background);
        SetBkMode(dc, TRANSPARENT as i32);
        let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
        let font = |height: i32, weight: i32| {
            CreateFontW(
                height,
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                0,
                0,
                0,
                5,
                0,
                face.as_ptr(),
            )
        };
        let (big, small) = (font(-26, 600), font(-15, 400));
        let lines = lines(&crate::progress::snapshot());
        let mut y = 20;
        for (k, line) in lines.iter().enumerate() {
            let (f, color, step) = match k {
                0 => (big, 0x00FF_FFFF, 38),
                1 | 2 => (small, 0x00E0_E0E0, 22),
                _ => (small, 0x0090_9090, 19),
            };
            // A blank line draws nothing (DrawTextW is given no empty text).
            if line.is_empty() {
                y += step;
                continue;
            }
            let old = SelectObject(dc, f);
            SetTextColor(dc, color);
            let mut text: Vec<u16> = line.encode_utf16().collect();
            let mut r = RECT {
                left: 24,
                top: y,
                right: client.right - 24,
                bottom: y + step,
            };
            DrawTextW(
                dc,
                text.as_mut_ptr(),
                text.len() as i32,
                &mut r,
                DT_LEFT | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(dc, old);
            y += step;
        }
        DeleteObject(big);
        DeleteObject(small);
        EndPaint(hwnd, &ps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window shows the phase, the file, the progress line, the log and
    /// how to skip, in that order.
    #[test]
    fn the_window_shows_phase_progress_log_and_how_to_skip() {
        let s = crate::progress::State {
            phase: "Translating shaders".into(),
            file: "shaders.rpf".into(),
            what: "containers".into(),
            done: 5,
            total: 10,
            log: ["one".to_owned(), "two".to_owned()].into(),
            ..Default::default()
        };
        let l = lines(&s);
        assert_eq!(l[0], "Translating shaders");
        assert_eq!(l[1], "shaders.rpf");
        assert_eq!(l[2], "5 of 10 containers (50%)");
        assert_eq!(&l[4..6], ["one", "two"]);
        assert!(l.last().unwrap().contains("Esc starts the game now"));
    }

    /// The window opens and closes without skipping, from another thread.
    #[test]
    fn the_window_opens_and_closes_without_skipping() {
        let splash = show("VIctory test");
        std::thread::sleep(std::time::Duration::from_millis(300));
        splash.close();
        assert!(!skipped());
    }
}
