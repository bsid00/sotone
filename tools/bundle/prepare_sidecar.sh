#!/usr/bin/env bash
#
# Builds sotone-hook and stages it where the Tauri bundler expects a sidecar to
# already be sitting. Linux/macOS counterpart of prepare_sidecar.ps1;
# the two are the same script in two languages and must stay that way.
#
# `bundle.externalBin` does not build anything: the bundler copies a file that
# must already exist, named `<entry>-<target-triple>` (no extension on these
# platforms -- the `.exe` in the ps1 is Windows's, not the bundler's), and
# strips the triple as it copies so the app installs with `sotone-hook` next to
# the app binary -- exactly where `shell.rs::helper_path()` looks.
#
# Two things about it are deliberate:
#
#   1. The hook is built by its own `cargo build -p sotone-hook`, with no
#      features. Whisper is optional in sotone-core precisely so the
#      hook can link a backend-free copy of it (54 MB -> 0.4 MB). Building it in
#      the same cargo invocation as the app would unify features across the
#      graph and drag whisper.cpp back into a binary that never transcribes, so
#      the two builds must stay separate invocations.
#   2. Every path is derived from the script's own location, never from the
#      working directory, so a script that quietly builds the wrong tree because
#      it was launched from elsewhere is impossible -- the worst a wrong cwd can
#      do is fail to find the script at all.
#
# The size gate is the real check here. A hook over 5 MB means the
# backend feature-gating regressed and whisper.cpp is back inside it; that must
# fail the build rather than ship a 54 MB sidecar to users.
#
# `tauri.conf.json`'s beforeBuildCommand still names the PowerShell script,
# because only the Windows release job bundles today. A future Linux/macOS
# bundle job needs a platform-conf override pointing at this file (see the
# comment in .github/workflows/release.yml).
#
# Usage:   bash tools/bundle/prepare_sidecar.sh
# Exit 0 - `src-tauri/binaries/sotone-hook-<triple>` is present and fresh.
# Exit 1 - the build failed, the hook was not produced, or it is too big to be
#          the slim hook (see the message).

set -euo pipefail

# A fat hook means the feature-gating regressed. The slim one is ~0.4 MB, so
# this bar is loose enough never to fire on ordinary growth and tight enough to
# catch whisper.cpp (~50 MB) coming back.
MAX_HOOK_BYTES=$((5 * 1024 * 1024))
# And a floor, because the failure this script exists to prevent is shipping the
# wrong bytes, and truncation is as wrong as bloat. Nothing that links
# sotone-core's hotkey wire comes out under 64 KB.
MIN_HOOK_BYTES=$((64 * 1024))

# Failures go to stderr and exit non-zero, so a CI step that runs this fails
# loudly rather than continuing with a stale or absent sidecar.
die() {
    printf 'prepare_sidecar: %s\n' "$1" >&2
    exit 1
}

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
manifest="$repo_root/Cargo.toml"
[ -f "$manifest" ] || die "expected the workspace manifest at $manifest -- is this script still in tools/bundle/?"

# Never hardcode the target directory: a developer machine may redirect it out
# of the tree (Windows MAX_PATH), and cargo is the only thing that knows where
# it ended up. `--no-deps` keeps the output to the three workspace members --
# it needs no dependency resolution, and the full graph is megabytes of JSON.
metadata=$(cargo metadata --format-version 1 --no-deps --manifest-path "$manifest") ||
    die 'cargo metadata failed'

# jq is preinstalled on GitHub runners and is the right tool; python3 is the
# fallback for a developer machine without it. Both read the same one field.
if command -v jq >/dev/null 2>&1; then
    target_dir=$(printf '%s' "$metadata" | jq -r '.target_directory')
elif command -v python3 >/dev/null 2>&1; then
    target_dir=$(printf '%s' "$metadata" |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
else
    die 'need jq or python3 to read target_directory out of `cargo metadata`'
fi
case "$target_dir" in
    '' | null) die 'cargo metadata did not report a target_directory' ;;
esac

built="$target_dir/release/sotone-hook"

# Deleted before the build, and this is the subtle part. That path is not
# cargo's private property: `<target>/release/sotone-hook` is where cargo uplifts
# the hook AND where tauri-build drops its own copy of the staged sidecar, which
# it does by removing the file (breaking cargo's hardlink to `deps/`) and
# copying over it. Cargo's fingerprints hash inputs, not outputs, so a fresh
# build leaves whatever is sitting there alone -- and anything reading it would
# then stage bytes cargo never produced. Cargo does re-create an output that is
# *missing*, so deleting first makes cargo the only possible author of what we
# copy.
rm -f "$built"

printf 'prepare_sidecar: building the hotkey helper from %s\n' "$repo_root"
cargo build -p sotone-hook --release --manifest-path "$manifest" ||
    die 'cargo build -p sotone-hook --release failed'

# The bundler names the file after the *host* triple, because that is what it
# will be looking for when it copies it (no cross-compilation here; a `--target`
# build would need this to follow suit).
host_line=$(rustc -vV | grep '^host:' || true)
[ -n "$host_line" ] || die 'could not read the host triple from `rustc -vV`'
triple=${host_line#host:}
# Trim surrounding whitespace without leaning on a subshell's word splitting.
triple=$(printf '%s' "$triple" | tr -d '[:space:]')
[ -n "$triple" ] || die 'could not read the host triple from `rustc -vV`'

[ -f "$built" ] || die "the build reported success but $built is not there"

# `wc -c` rather than `stat`, whose flags differ between GNU and BSD/macOS.
size=$(wc -c <"$built" | tr -d '[:space:]')
if [ "$size" -lt "$MIN_HOOK_BYTES" ]; then
    die "$built is only $size bytes, which cannot be a real helper -- refusing to stage it"
fi
if [ "$size" -gt "$MAX_HOOK_BYTES" ]; then
    mb=$(awk -v b="$size" 'BEGIN { printf "%.1f", b / 1048576 }')
    ceiling=$(awk -v b="$MAX_HOOK_BYTES" 'BEGIN { printf "%.0f", b / 1048576 }')
    die "$built is $mb MB, over the $ceiling MB ceiling. The hook links no transcription backend by design; a hook this size means whisper.cpp is back inside it, and the bundle must not ship it."
fi

# Overwritten unconditionally, never reused: the bundler happily copies a stale
# sidecar and says nothing (tauri-apps/tauri#15134), which would ship an
# installer whose helper predates the build it came from.
stage_dir="$repo_root/src-tauri/binaries"
mkdir -p "$stage_dir"
staged="$stage_dir/sotone-hook-$triple"
cp -f "$built" "$staged"
chmod +x "$staged"

mb=$(awk -v b="$size" 'BEGIN { printf "%.2f", b / 1048576 }')
printf 'prepare_sidecar: staged %s (%s bytes, %s MB)\n' "$staged" "$size" "$mb"
exit 0
