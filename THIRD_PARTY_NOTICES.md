# Third-party notices

Sotone itself is MIT licensed (see `LICENSE`). This file summarizes the
licenses of the code it ships with: its Rust dependency graph and the
pieces that are vendored or embedded rather than pulled from crates.io. It
is a summary, not a substitute for the licenses themselves; the full
dependency graph is machine-auditable at any time with `cargo metadata`.

Last audited 2026-08-22 against the full (non-workspace) dependency graph:
537 packages.

## License families in the dependency graph

Every one of the 537 packages carries a license expression; none are
unlicensed or license-unknown. The graph is overwhelmingly permissive:
MIT, Apache-2.0, BSD (2- and 3-clause), ISC, Zlib, Unicode-3.0, Unlicense,
and CC0-class licenses account for all but the two families called out
below.

## MPL-2.0 (5 crates)

`cssparser`, `cssparser-macros`, `dtoa-short`, and `selectors` (CSS
machinery pulled in via Tauri/wry) and `option-ext` (pulled in via `dirs`)
are licensed under the Mozilla Public License 2.0, a file-level weak
copyleft license, compatible with distributing the combined work under
MIT. Sotone uses all five **unmodified, straight from crates.io**; the
MPL's source-availability obligation for those files is satisfied by
pointing at the upstream crates, which is what this paragraph does.

## r-efi (5.3.0, 6.0.0)

Tri-licensed `MIT OR Apache-2.0 OR LGPL-2.1-or-later`. Sotone takes it
under MIT. No copyleft obligation attaches.

## Vendored and embedded code (not crates.io dependencies)

These are not pulled from crates.io at build time and are named here
explicitly rather than left to `cargo metadata`.

- **`vendor/rdev`**: a modified copy of rdev 0.5.3 (MIT, Nicolas Patry).
  The upstream MIT license text is kept at `vendor/rdev/LICENSE`, as MIT
  requires the notice to travel with the code. The local patches are
  documented in `vendor/rdev/README-SOTONE.md`.
- **whisper.cpp and ggml**: MIT (Georgi Gerganov and the ggml authors).
  Vendored inside the `whisper-rs-sys` crate and statically linked into
  `sotone.exe`.
- **WebView2 offline bootstrapper**: the Windows NSIS installer embeds
  Microsoft's WebView2 offline bootstrapper (`webviewInstallMode:
  offlineInstaller`). This is proprietary, Microsoft-owned code,
  redistributed under Microsoft's WebView2 distribution terms, not MIT,
  not open source. Stated here as fact, not legal advice.
