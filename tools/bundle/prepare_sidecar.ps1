<#
.SYNOPSIS
    Builds sotone-hook.exe and stages it where the Tauri bundler expects a
    sidecar to already be sitting.

.DESCRIPTION
    `bundle.externalBin` does not build anything: the bundler copies a
    file that must already exist, named `<entry>-<target-triple>.exe`, and
    strips the triple as it copies so the app installs with `sotone-hook.exe`
    next to `sotone.exe` -- exactly where `shell.rs::helper_path()` looks. This
    script produces that file. `tauri.conf.json` runs it as
    `build.beforeBuildCommand`, and it is safe to run by hand.

    Two things about it are deliberate:

    1. The hook is built by its own `cargo build -p sotone-hook`, with no
       features. Whisper is optional in sotone-core precisely so the
       hook can link a backend-free copy of it (54 MB -> 0.4 MB). Building it in
       the same cargo invocation as the app would unify features across the
       graph and drag whisper.cpp back into a binary that never transcribes, so
       the two builds must stay separate invocations.
    2. Every path is derived from $PSScriptRoot, never from the working
       directory, so a script that quietly builds the wrong tree because it was
       launched from elsewhere is impossible -- the worst a wrong cwd can do is
       fail to find the script at all.

       That matters because the `-File` path in `tauri.conf.json` is relative,
       and JSON cannot hold the note explaining what it is relative to, so it
       lives here: tauri-cli runs beforeBuildCommand with its cwd set to the
       *frontend* directory, not `src-tauri/`. Frontend-directory discovery
       looks for a `package.json`; this repo has none anywhere (plain JS, no
       bundler), so it falls back to the parent of the tauri directory -- the
       repo root -- whichever directory `cargo tauri build` was invoked from.
       Add a `package.json` and that anchor moves.

    The size gate is the real check here. A hook over 5 MB means the
    backend feature-gating regressed and whisper.cpp is back inside it; that
    must fail the build rather than ship a 54 MB sidecar to users.

    Windows-only: PowerShell 5.1 compatible, and the staged
    file carries the `.exe` the bundler expects on this platform. The Linux and
    macOS equivalents land with their bundles.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File tools\bundle\prepare_sidecar.ps1

.OUTPUTS
    Exit 0 - `src-tauri/binaries/sotone-hook-<triple>.exe` is present and fresh.
    Exit 1 - the build failed, the hook was not produced, or it is too big to
             be the slim hook (see the message).
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# A fat hook means the feature-gating regressed. The slim one is ~0.4 MB, so
# this bar is loose enough never to fire on ordinary growth and tight enough to
# catch whisper.cpp (~50 MB) coming back.
$MAX_HOOK_BYTES = 5MB
# And a floor, because the failure this script exists to prevent is shipping the
# wrong bytes, and truncation is as wrong as bloat. Nothing that links
# sotone-core's hotkey wire comes out under 64 KB.
$MIN_HOOK_BYTES = 64KB

# Native tools report failure through the exit code. PowerShell 5.1 can turn an
# ordinary cargo progress line on stderr into a terminating error when stdout is
# captured, so the error preference is relaxed for the call itself and the exit
# code is what we judge.
function Invoke-Tool {
    param(
        [Parameter(Mandatory = $true)] [string] $Exe,
        [Parameter(Mandatory = $true)] [string[]] $Arguments,
        [switch] $Capture
    )

    $ErrorActionPreference = 'Continue'
    if ($Capture) {
        $output = & $Exe @Arguments
    }
    else {
        & $Exe @Arguments
        $output = $null
    }
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        throw "$Exe $($Arguments -join ' ') failed with exit code $code"
    }
    return $output
}

try {
    $repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
    $manifest = Join-Path $repoRoot 'Cargo.toml'
    if (-not (Test-Path -LiteralPath $manifest)) {
        throw "expected the workspace manifest at $manifest -- is this script still in tools/bundle/?"
    }

    # Never hardcode the target directory: this machine redirects it out of the
    # tree (Windows MAX_PATH), and cargo is the only thing that knows where it
    # ended up. `--no-deps` keeps the output to the three workspace members --
    # both because it needs no dependency resolution and because PowerShell
    # 5.1's ConvertFrom-Json chokes on the full graph.
    $metadata = (Invoke-Tool -Exe 'cargo' -Arguments @(
            'metadata', '--format-version', '1', '--no-deps', '--manifest-path', $manifest
        ) -Capture) -join ''
    $targetDir = ($metadata | ConvertFrom-Json).target_directory
    if ([string]::IsNullOrWhiteSpace($targetDir)) {
        throw 'cargo metadata did not report a target_directory'
    }
    $built = Join-Path (Join-Path $targetDir 'release') 'sotone-hook.exe'

    # Deleted before the build, and this is the subtle part. That path is not
    # cargo's private property: `<target>/release/sotone-hook.exe` is where cargo
    # uplifts the hook AND where tauri-build drops its own copy of the staged
    # sidecar, which it does by removing the file (breaking cargo's hardlink to
    # `deps/`) and copying over it. Cargo's fingerprints hash inputs, not
    # outputs, so a fresh build leaves whatever is sitting there alone -- and
    # anything reading it would then stage bytes cargo never produced. Cargo
    # does re-create an output that is *missing*, so deleting first makes cargo
    # the only possible author of what we copy.
    if (Test-Path -LiteralPath $built) {
        Remove-Item -LiteralPath $built -Force
    }

    Write-Host "prepare_sidecar: building the hotkey helper from $repoRoot"
    Invoke-Tool -Exe 'cargo' -Arguments @(
        'build', '-p', 'sotone-hook', '--release', '--manifest-path', $manifest
    )

    # The bundler names the file after the *host* triple, because that is what
    # it will be looking for when it copies it (no cross-compilation here; a
    # `--target` build would need this to follow suit).
    $hostLine = (Invoke-Tool -Exe 'rustc' -Arguments @('-vV') -Capture) |
        Where-Object { $_ -like 'host:*' } |
        Select-Object -First 1
    if (-not $hostLine) {
        throw 'could not read the host triple from `rustc -vV`'
    }
    # Parsed out here rather than with a capture group inside the Where-Object:
    # $Matches would be set in that block's own scope and be empty by now.
    $triple = $hostLine.Substring('host:'.Length).Trim()

    if (-not (Test-Path -LiteralPath $built)) {
        throw "the build reported success but $built is not there"
    }

    $size = (Get-Item -LiteralPath $built).Length
    if ($size -lt $MIN_HOOK_BYTES) {
        throw "$built is only $size bytes, which cannot be a real helper -- refusing to stage it"
    }
    if ($size -gt $MAX_HOOK_BYTES) {
        $tooBig = '{0} is {1:N1} MB, over the {2:N0} MB ceiling.' -f `
            $built, ($size / 1MB), ($MAX_HOOK_BYTES / 1MB)
        $tooBig += ' The hook links no transcription backend by design;'
        $tooBig += ' a hook this size means whisper.cpp is back inside it, and the'
        $tooBig += ' bundle must not ship it.'
        throw $tooBig
    }

    # Overwritten unconditionally, never reused: the bundler happily copies a
    # stale sidecar and says nothing (tauri-apps/tauri#15134), which would ship
    # an installer whose helper predates the build it came from.
    $stageDir = Join-Path (Join-Path $repoRoot 'src-tauri') 'binaries'
    if (-not (Test-Path -LiteralPath $stageDir)) {
        New-Item -ItemType Directory -Path $stageDir | Out-Null
    }
    $staged = Join-Path $stageDir "sotone-hook-$triple.exe"
    Copy-Item -LiteralPath $built -Destination $staged -Force

    Write-Host ("prepare_sidecar: staged {0} ({1:N0} bytes, {2:N2} MB)" -f `
            $staged, $size, ($size / 1MB))
    exit 0
}
catch {
    Write-Host "prepare_sidecar: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}
