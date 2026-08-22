# Contributing

Sotone is local-first, and the six invariants below are law, not
guidelines. A PR that violates one is closed regardless of how good the
rest of it is. Code comments refer to them by number.

1. No synthetic input, ever. Nothing in this codebase may generate
   keystrokes or mouse events.
2. Never steal focus. No window activation, no raise-to-front during
   capture or transcription.
3. Nothing leaves the machine. No network calls except explicit,
   user-initiated model downloads.
4. Never destroy user notes. Deletes go to `.trash/`, saves are atomic.
5. Never block the input hook. The hook callback enqueues and returns,
   nothing more.
6. No bundled model weights. Not in the repo, not in the installer.

## Pull requests

- Keep a PR small and scoped to one thing.
- Say out loud what you deviated from and why, if anything. Reviewing
  is much easier when the reasoning is visible.
- Comments explain *why*, not what. The interesting comments in this
  codebase are the ones about ordering, focus, and threading; keep them
  true when you change the code around them.
- Conventional commits (`feat:`, `fix:`, `refactor:`, `docs:`, `chore:`).
- No new dependency without a written line in the PR justifying it.
- No `unwrap()` or `expect()` on a runtime path. `anyhow` at boundaries,
  `thiserror` for library errors.

## Verify bar

Before opening a PR:

```
cargo fmt --all -- --check
cargo clippy --features vulkan -- -D warnings
cargo clippy --all-targets --features vulkan -- -D warnings
cargo test --features vulkan
```

Before anything that builds `src-tauri` (including `cargo test`), run
`tools/bundle/prepare_sidecar.ps1` (Windows) or
`tools/bundle/prepare_sidecar.sh` (Linux/macOS) once. The build expects
the staged sidecar in place. `BUILD.md` lists the toolchain prerequisites.

Tests must not require an audio device, a GPU, a real model file, or a
window. Anything that does belongs in an example or harness, not
`cargo test`. `vendor/rdev` is excluded from the workspace on purpose so
`fmt` and `clippy` apply to Sotone's own code only.

## Hard no

PRs are closed on sight, regardless of quality, if they:

- Add any code path that can synthesize keyboard or mouse input.
- Add any network call that isn't an explicit, user-initiated model
  download.
- Bundle model weights into the repo or the installer.

These map to invariants 1, 3, and 6 above.

## Testing by hand

Anything that needs ears, eyes, or a real game running (audio quality,
latency feel, whether a hotkey reaches a fullscreen window) can only be
verified by a person. If your change touches that surface, say in the PR
what you tested by hand, on what, and what you did not.
