# Building Sotone

Sotone is a Rust workspace with three crates and a Tauri v2 shell. The
transcription backend is chosen at compile time with a Cargo feature
(`vulkan`, `cpu` or `metal`). The speech model is a runtime choice.

| Feature | Platform | Backend | Status |
|---|---|---|---|
| `vulkan` | Windows, Linux | whisper.cpp on any Vulkan GPU (AMD, NVIDIA, Intel, including integrated) | **Verified** on Windows |
| `cpu` | Windows, Linux | whisper.cpp on OpenBLAS + OpenMP | Compiles in CI, unproven at runtime |
| `metal` | macOS | whisper.cpp on Metal + CoreML | Compiles in CI, unproven at runtime |

Exactly one feature must be enabled.

## Windows, Vulkan (the verified path)

### Toolchain

1. **Rust** (stable, `x86_64-pc-windows-msvc`).
2. **Visual Studio 2022 Build Tools** with the C++ workload.
3. **CMake** and **Ninja**. whisper.cpp's nested Vulkan build only configures
   under the Ninja generator. The Visual Studio generator fails with
   "No CMAKE_C_COMPILER could be found" on `vulkan-shaders-gen`. So set
   `CMAKE_GENERATOR=Ninja`.
4. **LLVM**. `bindgen` needs `libclang` to generate the whisper.cpp bindings.
   If LLVM's `bin` folder isn't on your PATH, set `LIBCLANG_PATH` to it.
5. The **LunarG Vulkan SDK**. Its installer sets `VULKAN_SDK`; the build needs
   the headers, the loader and `glslc`.
6. **tauri-cli** for bundling: `cargo install tauri-cli --version 2.11.4 --locked`.
   Not needed for `cargo build` / `cargo test`.

### Build

1. Open a developer command prompt (or run `vcvars64.bat` first) so `cl.exe`
   is on PATH for the C build. Once whisper.cpp is compiled and cached,
   Rust-only rebuilds work from any shell.
2. Mind `MAX_PATH`. The whisper.cpp build adds ~155 characters to every object
   path; from a deep checkout that exceeds Windows' 260-character limit and
   `cl.exe` dies with `C1041`. Keep the checkout shallow, or point cargo's
   target directory somewhere short with a `.cargo/config.toml` in the
   checkout. It is gitignored, and it is also a convenient home for the two
   environment variables above:

   ```toml
   [build]
   target-dir = "C:/sotone-target"

   [env]
   LIBCLANG_PATH = "C:/Program Files/LLVM/bin"
   CMAKE_GENERATOR = "Ninja"
   ```

3. Stage the hook sidecar once before the first build:

   ```powershell
   powershell -ExecutionPolicy Bypass -File tools\bundle\prepare_sidecar.ps1
   ```

   This builds `sotone-hook.exe` and copies it to where the Tauri bundler
   expects a sidecar to already exist (`src-tauri/binaries/`). It is a
   prerequisite of `cargo test` as well as of bundling. Without it
   `tauri-build` fails with "resource path ... doesn't exist". `cargo tauri
   build` runs it again by itself.

4. Build:

   ```powershell
   cargo build --features vulkan          # compile, no bundle; day-to-day development
   cargo tauri build -f vulkan            # release bundle; NSIS installer lands under <target-dir>/release/bundle/nsis/
   ```

### The lockfile pin

Build with the committed `Cargo.lock`. `cpal` 0.18 declares compatibility with
`windows-core` 0.62 but does not compile against it; the lockfile pins 0.61.2.
If you ever regenerate the lockfile, re-pin with:

```
cargo update -p windows-core --precise 0.61.2
```

## Linux (`cpu`) and macOS (`metal`)

Both compile in CI and have never been run. Treat them as unverified until
someone has, and expect rough edges in the input hook. The vendored `rdev`
sources for X11 and Quartz have been compiled but not exercised.

The exact package lists CI installs before building are in
[`.github/workflows/ci.yml`](.github/workflows/ci.yml). Refer to that rather
than a copy here that can drift. The sidecar step on these platforms is
`tools/bundle/prepare_sidecar.sh`.

## Verifying a change

The bar every change is held to (CI runs the same commands):

```
cargo fmt --all -- --check
cargo clippy --features vulkan -- -D warnings
cargo clippy --all-targets --features vulkan -- -D warnings
cargo test --features vulkan
```

Tests need no audio device, no GPU, no model file and no window, so they run
on a headless runner. Anything that does need one lives in
`crates/sotone-core/examples/` (`mic_probe`, `hotkey_probe`, `transcribe_probe`,
`dictate`) rather than in `cargo test`.

`vendor/rdev` is excluded from the workspace on purpose, so `fmt` and
`clippy` apply to Sotone's own code only.
