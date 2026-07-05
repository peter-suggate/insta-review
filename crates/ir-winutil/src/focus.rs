//! Foreground-window bookkeeping: remember the game window when the hotkey
//! fires, restore it when the review window closes. HWNDs travel as isize
//! so they can cross threads/state safely.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};

/// The current foreground window (the game, when called from the hotkey
/// path). 0 if none.
pub fn foreground_window() -> isize {
    unsafe { GetForegroundWindow().0 as isize }
}

/// Best-effort focus restore. Works reliably when our process was the
/// foreground process (which it is, right after the user closes our
/// window).
pub fn restore_foreground(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe { SetForegroundWindow(HWND(hwnd as *mut _)).as_bool() }
}
