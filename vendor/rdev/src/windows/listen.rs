use crate::rdev::{Event, EventType, HookScope, ListenError};
use crate::windows::raw::{
    create_sink_window, raw_keyboard_event, raw_mouse_events, read_raw_input, register_usage,
    HID_USAGE_GENERIC_KEYBOARD, HID_USAGE_GENERIC_MOUSE,
};
use std::mem::zeroed;
use std::ptr::null_mut;
use std::time::SystemTime;
use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
use winapi::shared::windef::HWND;
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::winuser::{
    DefWindowProcW, DispatchMessageW, GetMessageW, HRAWINPUT, MSG, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
    WM_INPUT,
};

static mut GLOBAL_CALLBACK: Option<Box<dyn FnMut(Event)>> = None;

/// Hands one decoded event to the user callback.
///
/// Sotone fork (2026-08-04): upstream translated every KeyPress to a character
/// here — `KEYBOARD.lock()` then `Keyboard::get_name`, i.e. GetForegroundWindow
/// -> AttachThreadInput -> GetKeyboardState -> ToUnicodeEx — inside the hook
/// callback, on every keystroke on the machine. That attach could deadlock
/// against the caller's own UI thread, and Windows then timed the low-level
/// hook out and discarded the event. `name` has been `None` ever since, and the
/// whole delivery path is now: decode, stamp, call, return.
unsafe fn emit(event_type: EventType) {
    let event = Event {
        event_type,
        // The same stamp the hook path took, at the same point in the flow: as
        // early as the event is understood, never at write time.
        time: SystemTime::now(),
        name: None,
    };
    // `addr_of_mut!` rather than upstream's `&mut GLOBAL_CALLBACK`: identical
    // codegen, but it avoids the `static_mut_refs` warning, which is no longer
    // lint-capped now that rdev is a path dependency. (`&raw mut` would need
    // Rust 1.82; this crate is built at the workspace's 1.77 MSRV.)
    if let Some(callback) = (*std::ptr::addr_of_mut!(GLOBAL_CALLBACK)).as_mut() {
        callback(event);
    }
}

/// Reads one `WM_INPUT` packet and forwards whatever it means.
///
/// Deliberately thin: read, decode with the pure functions in
/// [`crate::windows::raw`], forward. A packet that cannot be read is dropped —
/// one lost event, never a lost listener.
unsafe fn dispatch_raw_input(handle: HRAWINPUT) {
    let raw = match read_raw_input(handle) {
        Some(raw) => raw,
        None => return,
    };

    match raw.header.dwType {
        RIM_TYPEKEYBOARD => {
            let keyboard = raw.data.keyboard();
            if let Some(event_type) =
                raw_keyboard_event(keyboard.VKey, keyboard.MakeCode, keyboard.Flags)
            {
                emit(event_type);
            }
        }
        RIM_TYPEMOUSE => {
            let mouse = raw.data.mouse();
            // Registering the mouse usage means every movement packet lands
            // here — around a thousand a second from a gaming mouse — and this
            // crate has no consumer for motion or the wheel on the raw path.
            // The flags compare keeps that traffic to a compare and a jump: no
            // decode, no allocation, no user callback. Unlike the hook this
            // replaced, the cost is now paid inside this process's own message
            // queue; nothing outside it waits for us to drain it.
            if mouse.usButtonFlags != 0 {
                for event_type in raw_mouse_events(mouse.usButtonFlags) {
                    emit(event_type);
                }
            }
        }
        _ => {}
    }
}

/// The sink window's procedure. `WM_INPUT` is decoded and forwarded; every
/// message, including `WM_INPUT`, then goes to `DefWindowProcW` — which is not
/// optional for `WM_INPUT`, since that is where the system cleans the packet
/// up.
unsafe extern "system" fn raw_input_proc(
    window: HWND,
    message: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if message == WM_INPUT {
        dispatch_raw_input(l_param as HRAWINPUT);
    }
    DefWindowProcW(window, message, w_param, l_param)
}

pub fn listen<T>(callback: T) -> Result<(), ListenError>
where
    T: FnMut(Event) + 'static,
{
    listen_scoped(callback, HookScope::KeyboardAndMouse)
}

/// Sotone fork (2026-08-12, patch 3; rebuilt 2026-08-16, patch 4): `listen` with
/// a say in which device classes are watched. `HookScope::Keyboard` never
/// registers the mouse usage, so this process is absent from the mouse path
/// entirely; `KeyboardAndMouse` registers both.
///
/// Since patch 4 this is Raw Input, not `SetWindowsHookEx`: a message-only
/// window plus `RIDEV_INPUTSINK` registrations, then a plain `GetMessageW`
/// pump. See [`crate::windows::raw`] for why, and for the MSDN details.
///
/// Like every previous version it blocks the calling thread forever and spawns
/// nothing: the window, the registrations and the message pump all belong to
/// the caller's thread, and they must, because `WM_INPUT` is queued to the
/// thread that owns the window.
pub fn listen_scoped<T>(callback: T, scope: HookScope) -> Result<(), ListenError>
where
    T: FnMut(Event) + 'static,
{
    unsafe {
        GLOBAL_CALLBACK = Some(Box::new(callback));
    }

    // Neither of the two `ListenError` variants Windows has is a perfect name
    // for "the sink window could not be created", but the window exists only to
    // receive the keyboard registration that follows it, so a failure here is
    // reported as the keyboard listener failing to install. The `GetLastError`
    // code it carries is the part a reader can act on.
    let window = create_sink_window(Some(raw_input_proc)).map_err(ListenError::KeyHookError)?;

    // Registered separately rather than as one two-element array: the classes
    // are independent, and one call each is what lets a failure name the class
    // that failed instead of guessing.
    register_usage(window, HID_USAGE_GENERIC_KEYBOARD).map_err(ListenError::KeyHookError)?;
    if scope.includes_mouse() {
        register_usage(window, HID_USAGE_GENERIC_MOUSE).map_err(ListenError::MouseHookError)?;
    }

    // Nothing unregisters and nothing destroys the window: this call does not
    // return in any supported flow, the sink dies with the process, and the
    // removal call is a trap anyway (`RIDEV_REMOVE` fails unless `hwndTarget`
    // is NULL, the opposite of what registration required).
    message_loop()
}

/// The pump. `WM_INPUT` is an ordinary queued message, so raw input only works
/// while this runs.
///
/// `GetMessageW` returns 0 for `WM_QUIT` (a clean stop: the listener is over,
/// and the caller is told by `listen` returning `Ok`) and -1 for an error, which
/// ends the listener rather than spinning on a broken queue.
fn message_loop() -> Result<(), ListenError> {
    unsafe {
        let mut message: MSG = zeroed();
        loop {
            match GetMessageW(&mut message, null_mut(), 0, 0) {
                0 => return Ok(()),
                -1 => return Err(ListenError::KeyHookError(GetLastError())),
                _ => {
                    DispatchMessageW(&message);
                }
            }
        }
    }
}
