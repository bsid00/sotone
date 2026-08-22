//! Raw Input: how this fork sees the keyboard and the mouse on Windows.
//!
//! Sotone fork (2026-08-16, patch 4). Upstream rdev — and this fork up to patch
//! 3 — listened through `SetWindowsHookExA(WH_KEYBOARD_LL / WH_MOUSE_LL)`. A
//! low-level hook is *synchronous*: the OS hands every input event on the
//! machine to the hooking thread and waits for it before the foreground
//! application sees the event, with a timeout (`LowLevelHooksTimeout`, capped
//! at 1000 ms since Windows 10 1709) after which the hook is dropped from the
//! chain without a word. Microsoft's own `LowLevelKeyboardProc` remarks
//! recommend raw input instead for monitoring input aimed at other threads.
//!
//! So this module implements Microsoft's documented "Using Raw Input" pattern:
//! a message-only window (`HWND_MESSAGE` parent) plus `RegisterRawInputDevices`
//! with `RIDEV_INPUTSINK`, which is what asks for input while the window is not
//! in the foreground and requires a non-NULL `hwndTarget`. Packets then arrive
//! as ordinary queued `WM_INPUT` messages on the thread that owns the window,
//! so the thread MUST pump messages — see `listen.rs`. Nothing outside this
//! process waits on that pump: a slow reader grows its own message queue and
//! nothing else on the machine can feel it.
//!
//! Three deliberate omissions, so nobody adds them back by reflex:
//!
//! * **No `RIDEV_NOLEGACY`.** It suppresses the registering process's *own*
//!   legacy `WM_KEYDOWN`/`WM_MOUSEMOVE` messages. It does nothing to anyone
//!   else's input, so it would buy this listener nothing.
//! * **No unregistering.** Removal is `{dwFlags: RIDEV_REMOVE, hwndTarget:
//!   NULL}` and the call *fails* if `hwndTarget` is non-NULL — an easy trap.
//!   The scope here is fixed for the life of the process (`listen` never
//!   returns in practice), so the sink simply dies with the process.
//! * **No `hDevice == NULL` filtering.** It looks like a free "ignore injected
//!   input" test and is not one: some touchpads report a NULL device handle for
//!   perfectly real hardware, so the filter would silently deafen those users.
//!   This listener observes everything the OS gives it.

use crate::rdev::{Button, EventType};
use crate::windows::keycodes::key_from_code;
use std::iter::once;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::Once;
use winapi::shared::minwindef::{DWORD, LPVOID, UINT};
use winapi::shared::ntdef::USHORT;
use winapi::shared::windef::HWND;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::libloaderapi::GetModuleHandleW;
use winapi::um::winuser::{
    CreateWindowExW, GetRawInputData, RegisterClassW, RegisterRawInputDevices, HRAWINPUT,
    HWND_MESSAGE, KEYBOARD_OVERRUN_MAKE_CODE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RIDEV_INPUTSINK, RID_INPUT, RI_KEY_BREAK, RI_KEY_E0, RI_MOUSE_BUTTON_4_DOWN,
    RI_MOUSE_BUTTON_4_UP, RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP, RI_MOUSE_LEFT_BUTTON_DOWN,
    RI_MOUSE_LEFT_BUTTON_UP, RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_UP,
    RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP, WNDCLASSW, WNDPROC,
};

// HID usage page 1 ("generic desktop") and the two usages on it that name the
// device classes this crate listens to. Registration is per class and
// independent: registering the keyboard does not disturb a mouse registration
// and vice versa, which is what lets `HookScope::Keyboard` leave the mouse
// alone entirely.
const HID_USAGE_PAGE_GENERIC: USHORT = 0x01;
pub(crate) const HID_USAGE_GENERIC_MOUSE: USHORT = 0x02;
pub(crate) const HID_USAGE_GENERIC_KEYBOARD: USHORT = 0x06;

// ---------------------------------------------------------------------------
// Decode — pure functions, no OS state, so Sotone can unit-test the vocabulary
// ---------------------------------------------------------------------------

// Virtual-key codes, redefined here for the same reason `keycodes.rs`
// redefines its table: winapi types them as `c_int`, and everything on this
// path is a `u16` straight out of a `RAWKEYBOARD`.
//
// The first three are the *generic* modifiers, and they are the whole reason
// `sided_vkey` exists. `WH_KEYBOARD_LL` reported the sided codes (0xA0..=0xA5);
// raw input reports the generic ones, so an unnormalised raw backend would
// quietly turn every `ShiftLeft` binding into `Key::Unknown(16)`.
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_LSHIFT: u16 = 0xA0;
const VK_RSHIFT: u16 = 0xA1;
const VK_LCONTROL: u16 = 0xA2;
const VK_RCONTROL: u16 = 0xA3;
const VK_LMENU: u16 = 0xA4;
const VK_RMENU: u16 = 0xA5;

// Set 1 scan codes of the two shift keys. Shift is the one modifier the E0
// flag cannot side: both shifts are unextended, they differ only here.
const SCAN_LEFT_SHIFT: u16 = 0x2A;
const SCAN_RIGHT_SHIFT: u16 = 0x36;

/// The first `VKey` value that maps to no virtual key at all. Microsoft's own
/// raw input sample discards these.
const VK_UNMAPPED: u16 = 0xFF;

/// One `RAWKEYBOARD` packet as an [`EventType`], or `None` when the packet
/// carries no key transition worth reporting.
///
/// `flags` is `RAWKEYBOARD::Flags`: `RI_KEY_BREAK` means a release, its absence
/// a press (including auto-repeat, which Windows delivers as repeated makes —
/// upstream's hook path forwarded those too, and consumers de-duplicate).
///
/// Two packets are dropped silently, both straight from Microsoft's sample: a
/// `VKey` at or above [`VK_UNMAPPED`], which is "not mapped to any virtual key
/// code", and the keyboard-overrun make code, which is the driver saying it
/// lost track rather than a key at all.
///
/// Pure: no OS call, no global state. That is deliberate — Sotone's test suite
/// drives this directly, because a backend this fork cannot unit-test is a
/// backend whose vocabulary drifts unnoticed.
pub fn raw_keyboard_event(vkey: u16, make_code: u16, flags: u16) -> Option<EventType> {
    if vkey >= VK_UNMAPPED || DWORD::from(make_code) == KEYBOARD_OVERRUN_MAKE_CODE {
        return None;
    }

    let key = key_from_code(sided_vkey(vkey, make_code, flags));
    if flags & (RI_KEY_BREAK as u16) != 0 {
        Some(EventType::KeyRelease(key))
    } else {
        Some(EventType::KeyPress(key))
    }
}

/// The sided virtual-key code for a generic modifier, so the key vocabulary is
/// exactly the one `WH_KEYBOARD_LL` produced.
///
/// | `VKey`             | sided by      | left     | right    |
/// |--------------------|---------------|----------|----------|
/// | `VK_SHIFT` 0x10    | make code     | 0xA0     | 0xA1     |
/// | `VK_CONTROL` 0x11  | `RI_KEY_E0`   | 0xA2     | 0xA3     |
/// | `VK_MENU` 0x12     | `RI_KEY_E0`   | 0xA4     | 0xA5     |
///
/// A six-line table rather than `MapVirtualKeyW`: the mapping is fixed by the
/// PS/2 scan code set, one syscall per keystroke would buy nothing, and that
/// API family lives next door to the input *synthesis* calls this project does
/// not link at all.
fn sided_vkey(vkey: u16, make_code: u16, flags: u16) -> u16 {
    let extended = flags & (RI_KEY_E0 as u16) != 0;
    match vkey {
        VK_SHIFT => match make_code {
            SCAN_LEFT_SHIFT => VK_LSHIFT,
            SCAN_RIGHT_SHIFT => VK_RSHIFT,
            // A keyboard that reports neither scan code is one there is no
            // table for; left is the safer guess, and it keeps the key
            // nameable instead of leaving an unbindable `Unknown(16)`.
            _ => VK_LSHIFT,
        },
        VK_CONTROL if extended => VK_RCONTROL,
        VK_CONTROL => VK_LCONTROL,
        VK_MENU if extended => VK_RMENU,
        VK_MENU => VK_LMENU,
        already_sided => already_sided,
    }
}

/// Every `RAWMOUSE::usButtonFlags` bit that names a button transition, with the
/// event it means. Presses first: one packet can carry several flags at once —
/// a click and its release, or two buttons in the same report — and a consumer
/// tracking held state has to see the press before the matching release.
///
/// The side buttons keep the exact spelling the `WM_XBUTTON*` path produced:
/// `RI_MOUSE_BUTTON_4_*` is XBUTTON1 → `Button::Unknown(1)`,
/// `RI_MOUSE_BUTTON_5_*` is XBUTTON2 → `Button::Unknown(2)`. Sotone's config
/// tokens `MouseX1`/`MouseX2` are those two numbers.
const BUTTON_TRANSITIONS: [(u16, EventType); 10] = [
    (
        RI_MOUSE_LEFT_BUTTON_DOWN,
        EventType::ButtonPress(Button::Left),
    ),
    (
        RI_MOUSE_RIGHT_BUTTON_DOWN,
        EventType::ButtonPress(Button::Right),
    ),
    (
        RI_MOUSE_MIDDLE_BUTTON_DOWN,
        EventType::ButtonPress(Button::Middle),
    ),
    (
        RI_MOUSE_BUTTON_4_DOWN,
        EventType::ButtonPress(Button::Unknown(1)),
    ),
    (
        RI_MOUSE_BUTTON_5_DOWN,
        EventType::ButtonPress(Button::Unknown(2)),
    ),
    (
        RI_MOUSE_LEFT_BUTTON_UP,
        EventType::ButtonRelease(Button::Left),
    ),
    (
        RI_MOUSE_RIGHT_BUTTON_UP,
        EventType::ButtonRelease(Button::Right),
    ),
    (
        RI_MOUSE_MIDDLE_BUTTON_UP,
        EventType::ButtonRelease(Button::Middle),
    ),
    (
        RI_MOUSE_BUTTON_4_UP,
        EventType::ButtonRelease(Button::Unknown(1)),
    ),
    (
        RI_MOUSE_BUTTON_5_UP,
        EventType::ButtonRelease(Button::Unknown(2)),
    ),
];

/// Every button transition in one `RAWMOUSE` packet, presses before releases.
///
/// Empty for a packet that carries no button flag at all: pointer motion (the
/// overwhelming majority — a gaming mouse reports around a thousand a second)
/// and the wheel, which this crate has no consumer for on the raw path. The
/// caller short-circuits on `usButtonFlags == 0` before ever reaching here, so
/// motion costs a compare and a jump.
///
/// Pure, like [`raw_keyboard_event`], and tested from Sotone the same way.
pub fn raw_mouse_events(us_button_flags: u16) -> Vec<EventType> {
    let mut events = Vec::new();
    for &(flag, event_type) in BUTTON_TRANSITIONS.iter() {
        if us_button_flags & flag != 0 {
            events.push(event_type);
        }
    }
    events
}

// ---------------------------------------------------------------------------
// Plumbing — the message-only window and the two registrations
// ---------------------------------------------------------------------------

/// Window class name. Nothing looks this up; it exists because
/// `CreateWindowExW` needs a registered class.
const WINDOW_CLASS_NAME: &str = "RdevRawInputSink";

// A window class is process-global and registering it twice fails with
// ERROR_CLASS_ALREADY_EXISTS, so it is registered exactly once. The atom and
// the error travel out of the `Once` in statics because `call_once` cannot
// return a value. The class is registered with the first window procedure it
// is given; this crate only ever passes one.
static REGISTER_CLASS: Once = Once::new();
static CLASS_ATOM: AtomicU16 = AtomicU16::new(0);
static CLASS_ERROR: AtomicU32 = AtomicU32::new(0);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(once(0)).collect()
}

/// Creates the message-only window that raw input is delivered to.
///
/// `HWND_MESSAGE` as the parent is what makes it message-only: no pixels, no
/// z-order, no taskbar presence, never shown, never activated — it exists to
/// own a message queue. Returns `GetLastError` on failure.
///
/// The window belongs to the calling thread, and so does every `WM_INPUT` it
/// receives: the caller must pump messages on *this* thread.
pub(crate) fn create_sink_window(window_proc: WNDPROC) -> Result<HWND, DWORD> {
    let class_name = wide(WINDOW_CLASS_NAME);
    let instance = unsafe { GetModuleHandleW(null_mut()) };

    REGISTER_CLASS.call_once(|| unsafe {
        let mut class: WNDCLASSW = zeroed();
        class.lpfnWndProc = window_proc;
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();

        let atom = RegisterClassW(&class);
        if atom == 0 {
            CLASS_ERROR.store(GetLastError(), Ordering::SeqCst);
        }
        CLASS_ATOM.store(atom, Ordering::SeqCst);
    });
    if CLASS_ATOM.load(Ordering::SeqCst) == 0 {
        return Err(CLASS_ERROR.load(Ordering::SeqCst));
    }

    let window = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            null_mut(),
        )
    };
    if window.is_null() {
        return Err(unsafe { GetLastError() });
    }
    Ok(window)
}

/// Registers one device class (keyboard or mouse) against `window` as an input
/// sink. Returns `GetLastError` on failure.
///
/// `RIDEV_INPUTSINK` is the flag that says "deliver this even when the target
/// window is not foreground", and it is why `hwndTarget` must be non-NULL.
/// Raw input is not exclusive: a game registering its own raw input does not
/// displace this sink, and this sink takes nothing away from the game.
pub(crate) fn register_usage(window: HWND, usage: USHORT) -> Result<(), DWORD> {
    let device = RAWINPUTDEVICE {
        usUsagePage: HID_USAGE_PAGE_GENERIC,
        usUsage: usage,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: window,
    };

    let registered =
        unsafe { RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as UINT) != 0 };
    if registered {
        Ok(())
    } else {
        Err(unsafe { GetLastError() })
    }
}

/// Reads the packet behind one `WM_INPUT` message.
///
/// `None` means this packet could not be read and is dropped — one lost event,
/// never a lost listener.
///
/// # Safety
///
/// `handle` must be the `lParam` of a `WM_INPUT` message currently being
/// handled; the packet it names is only valid until the message is passed on to
/// `DefWindowProc`.
pub(crate) unsafe fn read_raw_input(handle: HRAWINPUT) -> Option<RAWINPUT> {
    let header_size = size_of::<RAWINPUTHEADER>() as UINT;

    // The documented two-call pattern: NULL first to learn the size, which
    // returns 0 on success (and -1 on failure, unlike the second call).
    let mut size: UINT = 0;
    if GetRawInputData(handle, RID_INPUT, null_mut(), &mut size, header_size) != 0 {
        return None;
    }
    // Only the keyboard and mouse usages are registered, and both packets are
    // fixed-size and smaller than a `RAWINPUT`; a bigger one would have to be
    // HID, which never arrives here. Refusing it is cheaper than trusting it.
    if size as usize > size_of::<RAWINPUT>() {
        return None;
    }

    // The buffer is a `RAWINPUT` rather than a byte array on purpose: the
    // struct holds a HANDLE, so it needs pointer alignment that `[u8; N]` does
    // not promise.
    let mut raw: RAWINPUT = zeroed();
    let read = GetRawInputData(
        handle,
        RID_INPUT,
        &mut raw as *mut RAWINPUT as LPVOID,
        &mut size,
        header_size,
    );
    if read == UINT::MAX || (read as usize) < size_of::<RAWINPUTHEADER>() {
        return None;
    }
    Some(raw)
}
