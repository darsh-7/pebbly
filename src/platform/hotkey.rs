//! The timer picker's shortcut (settings; Win+Alt+N by default — Win+Alt+T, the first choice,
//! is often taken: PowerToys holds it).

use windows::core::Result;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};

use crate::settings::{Hotkey, QuestionHotkey, SwitchHotkey};

const ID: i32 = 1;

fn keys(h: Hotkey) -> Option<(HOT_KEY_MODIFIERS, char)> {
    match h {
        Hotkey::WinAltN => Some((MOD_WIN | MOD_ALT, 'N')),
        Hotkey::WinAltT => Some((MOD_WIN | MOD_ALT, 'T')),
        Hotkey::CtrlAltN => Some((MOD_CONTROL | MOD_ALT, 'N')),
        Hotkey::WinShiftN => Some((MOD_WIN | MOD_SHIFT, 'N')),
        Hotkey::Off => None,
    }
}

/// Registers `h` (replacing the previous one); `Off` only unregisters.
pub fn register_hotkey(owner: HWND, h: Hotkey) -> Result<()> {
    register(owner, ID, keys(h))
}

/// The question card's shortcut: its own id, so WM_HOTKEY tells the two apart.
pub const QUESTION_ID: i32 = 2;

pub fn register_question_hotkey(owner: HWND, h: QuestionHotkey) -> Result<()> {
    let keys = match h {
        QuestionHotkey::WinAltC => Some((MOD_WIN | MOD_ALT, 'C')),
        QuestionHotkey::WinShiftC => Some((MOD_WIN | MOD_SHIFT, 'C')),
        QuestionHotkey::Off => None,
    };
    register(owner, QUESTION_ID, keys)
}

fn register(owner: HWND, id: i32, keys: Option<(HOT_KEY_MODIFIERS, char)>) -> Result<()> {
    register_vk(owner, id, keys.map(|(mods, key)| (mods, key as u32)))
}

fn register_vk(owner: HWND, id: i32, keys: Option<(HOT_KEY_MODIFIERS, u32)>) -> Result<()> {
    unsafe {
        let _ = UnregisterHotKey(Some(owner), id);
        match keys {
            Some((mods, vk)) => RegisterHotKey(Some(owner), id, mods | MOD_NOREPEAT, vk),
            None => Ok(()),
        }
    }
}

/// The stack's shortcuts: ← back, → on, ↑ the latest alert.
pub const PREV_ID: i32 = 3;
pub const NEXT_ID: i32 = 4;
pub const LATEST_ID: i32 = 5;

/// Registers the three (replacing the previous ones); the first that fails is returned, the
/// others are still tried (one taken by another app shouldn't cost the rest).
pub fn register_switch_hotkeys(owner: HWND, h: SwitchHotkey) -> Result<()> {
    let mods = match h {
        SwitchHotkey::WinAltArrows => Some(MOD_WIN | MOD_ALT),
        SwitchHotkey::CtrlAltArrows => Some(MOD_CONTROL | MOD_ALT),
        SwitchHotkey::Off => None,
    };
    // VK_LEFT, VK_RIGHT, VK_UP.
    let results = [(PREV_ID, 0x25), (NEXT_ID, 0x27), (LATEST_ID, 0x26)].map(|(id, vk)| register_vk(owner, id, mods.map(|m| (m, vk))));
    results.into_iter().find(|r| r.is_err()).unwrap_or(Ok(()))
}
