//! Global hotkey via RegisterHotKey. System-level registration: fires even
//! while a raw-input game (CS2) has focus, no injection involved. The
//! WM_HOTKEY message also grants us foreground-window rights, which the
//! review window will rely on (M3).

use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use tracing::{info, warn};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_HOTKEY, WM_QUIT,
};

/// A parsed hotkey like "ctrl+alt+r" or "f9".
#[derive(Debug, Clone, Copy)]
pub struct Hotkey {
    pub modifiers: HOT_KEY_MODIFIERS,
    pub vk: u32,
}

impl Hotkey {
    /// Parse "ctrl+alt+r", "shift+f9", "f10", …
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut modifiers = MOD_NOREPEAT;
        let mut vk = None;
        for part in spec.split('+').map(|p| p.trim().to_ascii_lowercase()) {
            match part.as_str() {
                "ctrl" | "control" => modifiers |= MOD_CONTROL,
                "alt" => modifiers |= MOD_ALT,
                "shift" => modifiers |= MOD_SHIFT,
                "win" | "super" => modifiers |= MOD_WIN,
                key => vk = Some(parse_key(key)?),
            }
        }
        Ok(Self {
            modifiers,
            vk: vk.ok_or_else(|| format!("no key in hotkey spec {spec:?}"))?,
        })
    }
}

fn parse_key(key: &str) -> Result<u32, String> {
    // Function keys.
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Ok(0x70 + n - 1); // VK_F1..
        }
    }
    let bytes = key.as_bytes();
    if bytes.len() == 1 {
        let c = bytes[0].to_ascii_uppercase();
        if c.is_ascii_alphanumeric() {
            return Ok(c as u32); // VK_A..Z / VK_0..9 match ASCII
        }
    }
    Err(format!("unsupported key {key:?}"))
}

/// Runs a message loop on its own thread; sends `()` on every hotkey press.
pub struct HotkeyListener {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl HotkeyListener {
    pub fn start(hotkey: Hotkey, on_press: Sender<()>) -> Result<Self, String> {
        let (id_tx, id_rx) = std::sync::mpsc::channel();
        let join = std::thread::Builder::new()
            .name("ir-hotkey".into())
            .spawn(move || unsafe {
                let thread_id = GetCurrentThreadId();
                // RegisterHotKey must happen on the thread that pumps
                // messages (hwnd == NULL → WM_HOTKEY lands in this queue).
                let registered = RegisterHotKey(None, 1, hotkey.modifiers, hotkey.vk).is_ok();
                let _ = id_tx.send(if registered { Ok(thread_id) } else { Err(()) });
                if !registered {
                    return;
                }
                info!(?hotkey, "hotkey registered");

                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_HOTKEY {
                        if on_press.send(()).is_err() {
                            break;
                        }
                    } else {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                if let Err(e) = UnregisterHotKey(None, 1) {
                    warn!("UnregisterHotKey: {e}");
                }
            })
            .map_err(|e| format!("spawn hotkey thread: {e}"))?;

        match id_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                join: Some(join),
            }),
            _ => Err("RegisterHotKey failed (hotkey already in use?)".into()),
        }
    }

    pub fn stop(mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
