# rdev, vendored and stripped for Sotone

Read this before diffing against upstream.

**Upstream:** `rdev` 0.5.3, Nicolas Patry, MIT
(<https://github.com/Narsil/rdev>, crates.io). `LICENSE` is upstream's,
unchanged; attribution is required and kept.

Copied from the local cargo registry cache (no network). The crate keeps its
name and version so the workspace's `rdev = "=0.5.3"` pin plus
`[patch.crates-io] rdev = { path = "vendor/rdev" }` resolves here.
`.cargo-checksum.json`, `Cargo.lock` and `Cargo.toml.orig` are deliberately
absent; with a checksum file present cargo would reject the edits below.
`examples/`, `tests/`, `README.md`/`README.tpl` and the CI workflow are not
vendored.

## Patch 1: no foreground-thread work in the hook callback

Date: 2026-08-04. Bug: physical F4/F8 presses vanished (no cue, no notice, no
trace anywhere in Sotone) whenever Sotone's own window held focus.

Upstream `src/windows/listen.rs::raw_callback` called
`KEYBOARD.lock()` then `Keyboard::get_name(lpdata)` on **every KeyPress,
system-wide, inside WH_KEYBOARD_LL**. `get_name` → `set_global_state` does:

```
GetForegroundWindow -> AttachThreadInput(hook thread, foreground thread)
                    -> GetKeyboardState -> ToUnicodeEx
```

When the foreground window belongs to the same process as the hook (Sotone's
Tauri UI thread, with WebView2's synchronous keyboard handling), that attach
can deadlock against our own UI thread. Windows times the low-level hook out
and silently discards the call, so the event never reaches the callback at all.
Sotone never reads `Event::name`, so this was dead weight in the hottest
callback on the machine.

Changed:

- `src/windows/listen.rs`: the `get_name` call is gone; `name` is always
  `None`. The callback is now `convert` + `SystemTime::now()` + the user
  callback + `CallNextHookEx`. No lock, no allocation past the `Event`.
- `src/windows/keyboard.rs`: `get_name` and `set_global_state` deleted (their
  only caller was the hook). No `AttachThreadInput` and no `GetKeyboardState`
  remain anywhere in this tree. `Keyboard` / `KeyboardState` (i.e. `add`,
  `reset`, `get_code_name`) are kept as public API; they still call
  `ToUnicodeEx`, but only when a caller asks, never from the hook.
- `src/windows/common.rs`: the process-global `KEYBOARD: Mutex<Keyboard>`
  deleted, along with `get_scan_code` / `TRUE` / `FALSE`, which had no other
  caller.

Consequence: `Event::name` is `None` for Windows listen events. Linux and macOS
listen paths are untouched by this patch.

## Patch 2: invariants 1 and 5 at the source level

Sotone's README promises the binary contains no code capable of generating
keystrokes or mouse events, and none capable of swallowing them. That is now
true of the vendored source, not merely of our call sites:

- Deleted `src/{windows,linux,macos}/simulate.rs` (the `SendInput` /
  `XTestFakeKeyEvent` / `CGEvent` senders) and every `simulate` export in
  `src/lib.rs` and the platform `mod.rs` files.
- Deleted `src/{windows,linux,macos}/grab.rs`, the `unstable_grab` feature and
  its Linux-only deps (`evdev-rs`, `epoll`, `inotify`), and the two
  `unstable_grab`-gated leftovers: `Display::get_mouse_pos`
  (`src/linux/common.rs`) and `CGEventTapOption::Default`
  (`src/macos/common.rs`), the intercepting tap option. `grab` also called
  `get_name` in the same way patch 1 removes.
- Deleted the now-unreferenced `SimulateError`, `GrabError` and `GrabCallback`
  from `src/rdev.rs`, and their re-exports from `src/lib.rs`.
- Trimmed the crate-level rustdoc that documented `simulate` and `grab`.

Remaining public surface: `listen`, `display_size`, `Keyboard`,
`KeyboardState`, `Event`, `EventType`, `Key`, `Button`, `ListenError`,
`DisplayError`. The `serialize` feature is kept but unused by Sotone.

Also removed: upstream's `#[cfg(test)] mod tests` in `src/lib.rs`
(`test_keyboard_state`), which needed a real QWERTY layout and the OS keyboard
state: a device-dependent test, which this repo does not allow.

## Patch 3: the mouse hook is opt-in, and motion never enters our code

Date: 2026-08-12. Bug: with keyboard-only bindings (F4/F8) and
Sotone running, mouse input in a CPU-heavy fullscreen game went erratic: too
fast, then too slow, "as if the game skips ticks". Quitting Sotone fixed it
instantly (A/B tested against real gameplay, ground truth).

Cause is not our callback, which was already enqueue-only. It is that
`listen` installed `WH_MOUSE_LL` **unconditionally**, and the presence of that
hook makes Windows route every mouse input packet on the machine (~1000/s
from a gaming mouse) through `sotone-hook.exe`'s hook thread before the
foreground app sees it. Under full CPU load that thread gets starved and the
deltas arrive in bursts. With keyboard-only bindings the mouse hook served
nothing at all.

Changed (additive: `listen` keeps installing both hooks, so no existing
caller changes behaviour):

- `src/rdev.rs`: new `HookScope { Keyboard, KeyboardAndMouse }` with
  `includes_mouse()`.
- `src/lib.rs`: new `listen_scoped(callback, scope)`; `listen(cb)` is
  `listen_scoped(cb, KeyboardAndMouse)`.
- `src/windows/listen.rs`: `listen_scoped` calls `set_mouse_hook` only for
  `KeyboardAndMouse`. The raw callback now returns `CallNextHookEx`
  immediately for `WM_MOUSEMOVE` / `WM_MOUSEWHEEL` / `WM_MOUSEHWHEEL`: no
  `convert`, no `Event`, no user callback. That removes our per-move *work*;
  it does not remove the OS routing cost, which only skipping the hook does.
- `src/linux/listen.rs`: keyboard scope records `KeyPress..KeyRelease`
  instead of `KeyPress..MotionNotify`.
- `src/macos/{common,listen}.rs`: new `kCGEventMaskForKeyboardEvents`; the
  tap is created with the mask the scope asks for.

Public surface gains `HookScope` and `listen_scoped`.

## Patch 4: Windows listens with Raw Input, not with hooks

Date: 2026-08-16. Not a bug fix: this removes the *class* of bug
patches 1 and 3 were both instances of.

A low-level hook is **synchronous**. While `WH_KEYBOARD_LL` / `WH_MOUSE_LL` are
installed, Windows delivers every input event on the machine to the hooking
thread and waits for it to return before the foreground application sees the
event, with a timeout (`LowLevelHooksTimeout`, capped at 1000 ms since Windows
10 1709) after which the hook is dropped from the chain silently. That is the
mechanism behind patch 1 (a deadlocked callback timed out and keys vanished)
and behind patch 3's stutter (a starved hook thread released mouse deltas in
bursts). Microsoft's own `LowLevelKeyboardProc` remarks (rev. 2025-07)
recommend raw input instead for monitoring input targeted at other threads.

So `listen_scoped` no longer installs hooks. It follows the "Using Raw Input"
pattern from Microsoft's own documentation: create a message-only window
(`CreateWindowExW(.., HWND_MESSAGE, ..)`), call `RegisterRawInputDevices` with
`RIDEV_INPUTSINK` (which is what asks for input while the window is not
foreground, and is why `hwndTarget` must be non-NULL), then pump messages with
`GetMessageW`. `WM_INPUT` is an ordinary queued message, so a slow reader grows
its own message queue and **nothing outside this process waits on it**.

Changed:

- `src/windows/raw.rs`: new. The window class (registered once, `Once`), the
  sink window, the per-usage registration (page `0x01`, usage `0x06` keyboard /
  `0x02` mouse), the `GetRawInputData` two-call read, and the two **pure**
  decode functions.
- `src/windows/listen.rs`: reimplemented on the above. `WM_INPUT` → read →
  decode → `Event { time: SystemTime::now(), name: None }` → user callback;
  every message including `WM_INPUT` then goes to `DefWindowProcW`, which is
  not optional for `WM_INPUT` (that is where the system releases the packet).
  `GetMessageW` returning `-1` ends the listener with an error.
- `src/windows/common.rs`: **deleted**. Every item in it was hook plumbing:
  `SetWindowsHookExA`, the `HHOOK` global, and the
  `KBDLLHOOKSTRUCT`/`MSLLHOOKSTRUCT` decode. `listen.rs` was its only caller.
  A grep for `SetWindowsHookEx` over this tree now finds prose only.
- `src/windows/mod.rs`, `src/lib.rs`: wire `raw` in, and re-export the two
  decode functions.
- `Cargo.toml`: winapi gains the `libloaderapi` feature, for
  `GetModuleHandleW` (the module handle the window class is registered
  against). Everything else was already under `winuser`. **No new crate**, in
  this manifest or anywhere in the workspace.

Public surface gains `raw_keyboard_event` and `raw_mouse_events`. They are pure
(numbers in, `EventType` out, no OS state) and they are public so that
Sotone's own test suite can pin the vocabulary without a device, a window or a
thread. That matters more than API tidiness here: the OS now hands us different
numbers than it used to, and the config tokens users have already written must
keep naming the same physical controls.

### Behavioural deltas, including the uncomfortable ones

Strictly better:

- **Nothing in this crate sits on the synchronous input path in any scope.**
  No hook to time out, no silent unhook, no foreground application waiting on
  our thread.
- **Patch 3's residual is gone.** Registering the mouse usage does still deliver
  every movement packet (~1000/s from a gaming mouse; there is no buttons-only
  subscription), but asynchronously, into our own queue, after the foreground
  app already has it. `listen_scoped` still discards non-button mouse packets
  on a flags compare before decoding or allocating, and `HookScope::Keyboard`
  still never registers the mouse usage at all.

Unchanged:

- The UAC secure desktop delivers nothing to a raw input sink either.
- `Event::name` is still `None` on Windows (patch 1).
- Auto-repeat still arrives as repeated presses.
- Public API, `HookScope` semantics, and the blocking, spawn-free shape of
  `listen`/`listen_scoped`.

Changed in ways a caller could notice:

- **`EventType::MouseMove` and `EventType::Wheel` are never produced on
  Windows.** The raw path has no consumer for either, and building them would
  mean decoding motion for nothing. (Sotone has never had a consumer; another
  caller of this fork would.)
- **Sided modifiers are normalised by us, not by the OS.** `RAWKEYBOARD`
  reports the generic `VK_SHIFT`/`VK_CONTROL`/`VK_MENU` where the hook reported
  0xA0 to 0xA5. `raw.rs::sided_vkey` re-sides them with a hardcoded table
  rather than `MapVirtualKeyW`: shift by scan code (`0x2A` left, `0x36`
  right), control and menu by the `RI_KEY_E0` flag. A keyboard reporting
  neither shift scan code falls back to left, which is a guess, and the only
  guess on this path.
- **Error mapping.** No `ListenError` variant was added (the enum lives in
  `src/rdev.rs`, and no caller matches on it; Sotone formats it with `{:?}`).
  Registration failures map to the variant naming the class that failed:
  keyboard → `KeyHookError`, mouse → `MouseHookError`, which is why the two
  usages are registered in two calls rather than one two-element array.
  Window/class creation failure is reported as `KeyHookError` too (the window
  exists only to carry the keyboard registration) and the `GetLastError` code
  it carries is the actionable part.

Unverified, and deliberately not claimed either way:

- **Elevated foreground windows (UIPI).** A low-level hook in an unelevated
  process saw nothing while an elevated window had focus. Raw input is routed
  by the driver stack to each registered process, a different path, so it *may*
  cross that line. Nobody has tried. The old limitation stands in the
  docs until someone has.

Considered and rejected:

- **Filtering injected input via `hDevice == NULL`.** It reads like a free
  "ignore synthetic input" test and is not one: some touchpads report a NULL
  device handle for real hardware, which would deafen those users. This
  listener observes everything the OS gives it.
- **`RIDEV_NOLEGACY`.** It suppresses only the registering process's own legacy
  messages, which buys this listener nothing.
- **Unregistering on the way out.** Removal is `{RIDEV_REMOVE, hwndTarget:
  NULL}` and *fails* if `hwndTarget` is non-NULL, the opposite of what
  registration required. The scope is fixed for the life of the process and the
  sink dies with it, so nothing calls it. The fact is in a comment because it
  is an easy trap for whoever adds runtime rescoping.

Out of scope, recorded so it is not mistaken for an oversight:

- **The `sotone-hook.exe` helper process stays.** Raw input would probably
  survive in-process (our own queue; no starvation mechanism), but the split
  also buys lifetime isolation, and it was adopted after a real in-process
  starvation failure, so we do not re-litigate it on theory.
- **The open "capture dead in the empty phase" symptom is answered here as
  *not an input-API problem*.** That symptom is the shell's
  control-thread plumbing around the capture helper, not what the helper
  listens with; this patch changes nothing about it in either direction.
- **Linux and macOS listeners are untouched**; they still use X11 records and
  a Quartz event tap.
- The `HookScope` rustdoc in `src/rdev.rs` still explains itself in terms of
  patch 3's `WH_MOUSE_LL` tax. That text is accurate as history and stale as
  present tense; `src/rdev.rs` was outside this patch's scope.

## Verification status

Only the Windows target is compiled here. The Linux and macOS sources are
vendored for a later parity pass and are **not compile-checked** after the
strip or after patch 3's scope plumbing; they had no references to the deleted
`simulate`/`grab` items outside the deleted files, but treat them as
unverified until CI builds them.
