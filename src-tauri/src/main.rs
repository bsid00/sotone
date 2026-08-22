// Release builds run without a console window. Diagnostics go through tracing,
// so nothing is lost except the stdout nobody was reading.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod shell;
mod tray;

// The app is the process that loads a model, so it needs a backend. sotone-core
// stopped asserting this: a backend-free build of it is now a real
// configuration — sotone-hook.exe links exactly that, and skips ~50 MB of
// whisper.cpp for it. So the "none was enabled" half of the arbitration lives
// here, at the one binary that cannot do without one; "more than one" stays in
// sotone-core, next to the features themselves. Without this check a plain
// `cargo build -p sotone` would fail in shell.rs with a page of unresolved-module
// errors, and a backend-free `sotone.exe` would report `backend_name() == "none"`
// and only admit it when the transcriber failed to load.
#[cfg(not(any(feature = "vulkan", feature = "cpu", feature = "metal")))]
compile_error!(
    "Sotone needs exactly one backend feature, and none was enabled. \
     Build with --features vulkan, --features cpu, or --features metal (macOS only)."
);

use tauri::{Manager, WindowEvent};
use tracing_subscriber::EnvFilter;

use shell::ShellState;
use tray::{MenuAction, TrayInput};

fn main() {
    // Before anything else, so even startup failures are structured.
    init_tracing();

    tracing::info!(
        backend = sotone_core::backend_name(),
        version = env!("CARGO_PKG_VERSION"),
        "sotone starting"
    );

    tauri::Builder::default()
        // FIRST, before every other plugin and before our own `setup` — the
        // plugin's own documented rule, and here it is the whole safety story.
        // Its setup claims a named mutex keyed on the bundle identifier; a
        // second launch finds the mutex taken, sends one local `WM_COPYDATA`
        // to the running instance's hidden message window and calls
        // `process::exit(0)` **from inside this setup**, so the doomed process
        // dies before `shell::start` — before any window, tray icon, audio
        // stream or draft store exists. That ordering is what makes "Sotone
        // runs once" true rather than merely tidy: two live copies would both
        // record every press (Raw Input broadcasts) and both append to the
        // same store, which is invariant 4's corruption race.
        //
        // Invariant 1: a private window message between two of our own
        // processes is not keyboard or mouse synthesis — nothing here can
        // generate input. Invariant 3: `CreateMutexW` plus a local
        // `SendMessageW`; no socket is opened and nothing leaves the machine.
        // UNVERIFIED, stated rather than hidden: an *elevated* first instance
        // signalled by a non-elevated second — named mutexes and window
        // messages both cross session/integrity boundaries badly, and this is
        // the same untested territory as every other elevation behaviour in
        // this app.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // This runs synchronously on the running instance's window-message
            // thread, with the second process blocked in `SendMessageW` — the
            // tao thread's rule wearing another costume (invariant 5). So it
            // enqueues and returns: no window call, no I/O, no lock held for
            // longer than the send. The open itself happens on the tray
            // thread, in `act()` → `open_window` — the existing focus
            // carve-out that "Open Sotone" and the icon's double-click already
            // use, because relaunching the exe is the user asking for the
            // window. No new focus call is added anywhere by this task.
            //
            // `_args` and `_cwd` are dropped deliberately, not forgotten:
            // Sotone has no command-line surface, so there is nothing a
            // forwarded argument could mean.
            let Some(state) = app.try_state::<ShellState>() else {
                // Before `shell::start` managed the state, i.e. a second
                // launch landing in the first launch's first few
                // milliseconds. Nothing to route it through, and nothing
                // worth building a queue for.
                tracing::warn!("a second launch arrived before the shell was up; ignoring it");
                return;
            };
            tracing::info!("a second launch asked for the window");
            // If the tray thread never spawned (`tray::spawn` returned false),
            // this send goes nowhere and the second launch opens no window.
            // That machine is already in the warned, degraded state the tray's
            // own failure path describes; no fallback path is built for it.
            state.tray(TrayInput::Menu(MenuAction::Open));
        }))
        // These two, like the single-instance plugin above, are used from Rust
        // only: the frontend never
        // invokes them, so they need no entry in `capabilities/default.json` —
        // the ACL gates the IPC path, and Rust-side calls do not take it.
        //
        // `dialog` is the native folder picker, the one surface in Sotone that
        // takes focus, and it opens only from a click in our own UI.
        // `opener` is used for exactly one call, `reveal_item_in_dir`, on paths
        // Sotone resolved itself. Neither reaches the network (invariant 3).
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // App-defined commands are not ACL-gated; the capability file exists
        // for the frontend's `listen()`, which is.
        .invoke_handler(tauri::generate_handler![
            shell::set_armed,
            shell::shell_status,
            shell::draft_new,
            // The move chooser's "New note". Async, and the one
            // command that creates a draft off the store rather than through
            // the worker: it must **not** open what it makes, or the note the
            // selection is leaving would be displaced mid-batch. It answers
            // with the new id.
            shell::draft_create_detached,
            shell::draft_open,
            shell::draft_discard,
            // Saving. Send-only: the render, the guard and the
            // atomic write are all blocking I/O on the worker thread.
            shell::draft_save,
            // Save all: the same worker path, once per dirty note
            // of the active project, and Guarded every time — there is no
            // batch overwrite anywhere in this app.
            shell::draft_save_all,
            // Renaming a note. Send-only like the rest: a note's
            // name is its file's name, so this is an `fs::rename` plus a
            // metadata write on the worker or the control thread — never on
            // the thread that services IPC.
            shell::draft_rename,
            // Moving a note between projects. Send-only like the
            // rest: one atomic `meta.json` write carrying the project **and**
            // the re-derived binding, on the worker or the control thread. No
            // file on disk moves.
            shell::draft_set_project,
            // Line editing. All send-only except `line_audio`,
            // which reads a file and is therefore async — no filesystem work
            // ever happens on the thread that services IPC.
            shell::line_edit,
            shell::line_set_deleted,
            // Reordering: one appended move record per drag.
            shell::line_move,
            // Move to note… — the selection toolbar's third action.
            // Send-only like the rest, one call per line, and append-only on
            // both sides: the destination gains records and a byte copy of the
            // wav, the source gains the ordinary soft delete. Nothing is ever
            // in neither note — the worker writes the destination first.
            // It also carries `first`: the batch boundary, which
            // only the window knows, and which puts one session divider above
            // the arrivals instead of one above each.
            shell::line_move_to,
            shell::line_retranscribe,
            shell::line_audio,
            // Projects. All async: they write the config file or
            // open a native dialog, and neither may happen on the thread that
            // services IPC.
            shell::project_create,
            shell::project_set_active,
            shell::project_update,
            // The project's lifecycle. `project_rename` moves the
            // name, the folder when that is safe, and every referencing
            // draft's `meta.project`; `project_delete` removes the entry from
            // the config and touches **no file at all** — the folder and its
            // notes stay on disk.
            shell::project_rename,
            shell::project_delete,
            shell::project_pick_folder,
            shell::project_reveal,
            shell::filename_preview,
            // Search. Async for the same reason, and one more: it
            // walks every draft directory on disk, and a scan on the thread
            // that services IPC is jank in the one thread that must never
            // block. Read-only — it opens no writable path and answers with a
            // value rather than an event.
            shell::search_notes,
            // Settings. Async for the same reason the project
            // commands are — they write the config file, scan the models
            // folder or open a native dialog — except the two capture
            // commands, which are send-only: the control thread owns the
            // hook helper, and the capture is its lifecycle to drive.
            shell::settings_devices,
            shell::hotkey_capture_start,
            shell::hotkey_capture_cancel,
            shell::set_mode_enabled,
            // The microphone, the model and the language all apply live:
            // `set_mic` reconnects the engine, `model_set_active` loads
            // off the event loop and hands the worker a warmed transcriber, and
            // `set_language` is a decoding parameter on the running model.
            // Nothing in Settings needs a restart any more.
            shell::set_mic,
            shell::set_language,
            shell::set_cues,
            shell::set_overlay,
            // What the X does. It writes one config key and touches
            // nothing else: `shell::on_close` reads that key when the X is
            // actually pressed, so there is no running copy to update here.
            shell::set_close_quits,
            // The overlay pill's two knobs. Both write the config
            // and then touch one window: the corner moves the pill now, the
            // reveal duration is read when the next line lands.
            shell::set_overlay_corner,
            shell::set_reveal_seconds,
            // The palette and the deleted-lines filter.
            // The two settings mutations accepted while a recording is live —
            // both are view preferences that reach the config file and the
            // window, and nothing else.
            shell::set_theme,
            shell::set_hide_deleted,
            shell::model_set_active,
            // Models are managed in the folder now, by design: the two
            // commands behind the in-app "Add
            // model…" and "Remove" are deleted, not merely unregistered, and
            // what replaces them is this plus the models-folder reveal
            // (`project_reveal`, already registered above). A rescan is a
            // directory read that re-emits `sotone://settings` — it touches no
            // file, no engine and no config, so it is not refused while a
            // recording is live.
            shell::models_rescan,
            // First run. The way out of the empty phase, and now the
            // *only* restart left in the product: with no model there is no
            // session at all — no engine, no worker, no helper — so there is
            // nothing for a live change to reach, and starting one is a new
            // process. Async like the other config-adjacent commands, and it
            // relaunches this same local executable — no network, no focus, no
            // input.
            shell::app_restart,
            // The onboarding wizard. Three commands and no new
            // machinery: the finish writes one config key and — on a machine
            // that launched ready — puts the window back to its declared size,
            // while the other two answer questions the window must not answer
            // itself. `project_slug_preview` keeps the filename sanitizer in
            // Rust (one sanitizer), and `onboarding_notes_root` is the
            // suggested notes folder, which only Rust can resolve. Both are
            // synchronous: string work and one directory lookup, which is all
            // the thread servicing IPC may ever do.
            shell::onboarding_finish,
            shell::project_slug_preview,
            shell::onboarding_notes_root,
            // Error states. The manual lever behind the hotkey-dead
            // pane: it respawns the helper once, which is also how the
            // supervisor's restart counter gets reset. Send-only, like the two
            // capture commands, because the control thread owns the helper.
            shell::hook_recheck,
            // The glass tray menu. All three send-only into the
            // tray thread's channel: the page renders and measures, the
            // thread decides. `tray_menu_action` re-uses the same id
            // vocabulary the native menu had, so the click routing is
            // unchanged.
            tray::tray_menu_action,
            tray::tray_menu_ready,
            tray::tray_menu_closed
        ])
        .setup(|app| {
            // Returns as soon as the control thread is spawned: whisper alone
            // takes half a second to load and the event loop must not wait for
            // it. Startup failures after this point are reported into the
            // window as a fatal status rather than by dying.
            shell::start(app.handle())?;
            Ok(())
        })
        // Focus is deliberately not handled. Capture works whichever window is
        // focused, including Sotone's own, so nothing here
        // observes focus and nothing anywhere takes it (invariant 2).
        .on_window_event(|window, event| {
            match event {
                // This **hides** the window rather than quitting:
                // the tray is where Sotone lives while the window is closed, and
                // the tray's Exit is the one true way out. `shell::on_close` has
                // the whole rule, including what happens when there is no tray.
                WindowEvent::CloseRequested { api, .. } => shell::on_close(window, api),
                // The overlay pill's size and position are both derived from
                // its monitor's scale factor, so a DPI change makes
                // every one of those numbers stale at once. Re-placing is one
                // message on the overlay thread's queue and cannot show,
                // activate or otherwise disturb the window (invariant 2).
                WindowEvent::ScaleFactorChanged { .. } if window.label() == "overlay" => {
                    shell::on_overlay_scale_change(window.app_handle());
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        // Startup-fatal: there is no app without a webview, and no UI left in
        // which to show a friendlier error.
        .expect("failed to start the Tauri application")
        // Built and run in two steps rather than `Builder::run`, for exactly one
        // reason: the exit guard below needs the run-event callback.
        .run(|_app, event| {
            // **Belt and braces on top of `prevent_close`**. A
            // hidden window is still a window, so this should never fire — but
            // if anything ever does close the last one, an app whose whole job
            // is to keep listening while you play should not evaporate because
            // its UI went away. `code: None` is precisely "somebody other than
            // us asked": `AppHandle::exit`, which `shell::quit` calls, carries
            // `Some(0)` and is let through. A restart (`app_restart`) carries
            // `RESTART_EXIT_CODE` and `prevent_exit` ignores it by contract.
            if let tauri::RunEvent::ExitRequested {
                code: None, api, ..
            } = event
            {
                tracing::debug!("an unasked-for exit was prevented; Sotone lives in the tray");
                api.prevent_exit();
            }
        });
}

/// `RUST_LOG` if set, `info` otherwise. Timestamps and target names on, because
/// the interesting bugs in this codebase are ordering bugs across three threads.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}
