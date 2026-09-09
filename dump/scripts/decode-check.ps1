<#
.SYNOPSIS
    Checks that this machine's video decoder works, without a window or a renderer.

.DESCRIPTION
    A new decoder backend fails in one of two ways: it refuses the stream, or it produces
    nothing anybody can see. Judged by the picture on screen those are the same bug — and the
    screen needs a renderer, which on a new platform is the other half of the work.

    So this answers the decoder's half on its own. It builds the headless command line, finds a
    recorded bitstream, and decodes every frame in it, reporting how many went in, how many
    pictures came out, and what size they are.

    Where the recording comes from:

      1. -Dump, if you pass one.
      2. Any .h264, .hevc or .bin sitting in dump\, newest first.
      3. One recorded here and now, by running a host and a client against each other on the
         loopback. That needs an encoder this machine has — which on Windows means NVENC, so
         it works on an NVIDIA machine and not otherwise. When it cannot, it says so and tells
         you to copy a recording across.

.PARAMETER Dump
    A recorded Annex B bitstream, as written by PRISM_DUMP_BITSTREAM.

.PARAMETER Codec
    auto, h264 or hevc. Defaults to auto, which reads it out of the recording — being told the
    wrong one is silent, because the stream simply yields no frames.

.PARAMETER Verify
    Read every picture back and say whether it is a flat colour. Costs a copy out of GPU memory
    per frame, which is what the live path exists to avoid, so it is off unless asked for.

.PARAMETER Frames
    How many frames to record, when recording one here. Defaults to 120.

.EXAMPLE
    .\dump\scripts\decode-check.ps1

.EXAMPLE
    .\dump\scripts\decode-check.ps1 -Dump C:\Users\me\frames.hevc -Verify
#>

[CmdletBinding()]
param(
    [string]$Dump,
    [ValidateSet('auto', 'h264', 'hevc')]
    [string]$Codec = 'auto',
    [switch]$Verify,
    [int]$Frames = 120
)

$ErrorActionPreference = 'Stop'

# Native commands are run with this relaxed and their exit code read instead. Cargo writes its
# progress to standard error, and a shell told to stop on the first error treats that as one —
# so a build that is going perfectly well would end the script on its first line of output.
function Invoke-Native {
    param([scriptblock]$Command)

    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'

    try {
        & $Command
        return $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previous
    }
}

# The repository is two directories above this script, wherever it was run from.
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$dumpDir = Join-Path $root 'dump'

function Write-Step { param([string]$Text) Write-Host "`n== $Text" -ForegroundColor Cyan }
function Write-Good { param([string]$Text) Write-Host "   $Text" -ForegroundColor Green }
function Write-Warn { param([string]$Text) Write-Host "   $Text" -ForegroundColor Yellow }

Write-Step "Where"
Write-Host "   repository $root"
Write-Host "   PowerShell $($PSVersionTable.PSVersion)"
Write-Host "   $env:PROCESSOR_ARCHITECTURE"

# ── The toolchain ─────────────────────────────────────────────────────────────────────────
Write-Step "Toolchain"

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo is not on PATH. Install Rust from https://rustup.rs and open a new shell."
}

Write-Host "   $(cargo --version)"
Write-Host "   $(rustc --version)"
Write-Host "   host triple $((rustc -vV | Select-String '^host:').ToString().Split(' ')[1])"

# ── Build ─────────────────────────────────────────────────────────────────────────────────
# The whole binary, not just the decoder: it is one crate, and its other halves are what pull
# in SDL. If the build stops here on a machine where SDL will not compile, that is worth
# knowing plainly rather than as a puzzle.
Write-Step "Building prism-cli (release)"

Push-Location $root
try {
    $built = Invoke-Native { cargo build --release -p prism-cli }

    if ($built -ne 0) {
        throw "the build failed. If it stopped inside SDL, say so — the decoder does not need it and it can be moved behind a feature."
    }
}
finally {
    Pop-Location
}

$cli = Join-Path $root 'target\release\prism-cli.exe'
if (-not (Test-Path $cli)) { throw "built, but $cli is not there" }
Write-Good "built $cli"

# ── The recording ─────────────────────────────────────────────────────────────────────────
Write-Step "Recording"

if (-not $Dump) {
    $found = Get-ChildItem -Path $dumpDir -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Extension -in '.h264', '.hevc', '.bin' } |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1

    if ($found) {
        $Dump = $found.FullName
        Write-Good "using $($found.Name), $([math]::Round($found.Length / 1MB, 1)) MB"
    }
}

if (-not $Dump) {
    Write-Warn "nothing in dump\, so recording one here"

    # Two identities in a scratch directory that trust each other, so nothing this machine
    # already trusts is touched. The peers file lives beside the key, which is why each gets a
    # directory of its own.
    $scratch = Join-Path ([System.IO.Path]::GetTempPath()) "prism-decode-check"
    $hostDir = Join-Path $scratch 'host'
    $clientDir = Join-Path $scratch 'client'

    New-Item -ItemType Directory -Force -Path $hostDir, $clientDir | Out-Null

    # The key goes to standard output and a note about where it was kept to standard error, so
    # only the first is captured.
    $hostKey = (Invoke-Native { & $cli keygen --identity (Join-Path $hostDir 'identity.key') 2>$null })
    $hostKey = $hostKey | Select-Object -Last 1
    $clientKey = (Invoke-Native { & $cli keygen --identity (Join-Path $clientDir 'identity.key') 2>$null })
    $clientKey = $clientKey | Select-Object -Last 1

    $Dump = Join-Path $dumpDir 'recorded.bin'
    New-Item -ItemType Directory -Force -Path $dumpDir | Out-Null
    Remove-Item $Dump -ErrorAction SilentlyContinue

    $hostLog = Join-Path $scratch 'host.log'

    $server = Start-Process -FilePath $cli -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $hostLog -RedirectStandardError (Join-Path $scratch 'host.err') `
        -ArgumentList @(
            'host',
            '--identity', (Join-Path $hostDir 'identity.key'),
            '--peer-key', $clientKey,
            '--bind', '127.0.0.1:47210',
            '--fps', '60', '--encode',
            '--width', '1280', '--height', '720',
            '--frames', "$Frames"
        )

    Start-Sleep -Seconds 2

    $env:PRISM_DUMP_BITSTREAM = $Dump
    try {
        Invoke-Native {
            & $cli client `
                --identity (Join-Path $clientDir 'identity.key') `
                --host '127.0.0.1:47210' --peer-key $hostKey `
                --decode --idle-timeout-ms 4000
        } | Out-Null
    }
    finally {
        Remove-Item Env:\PRISM_DUMP_BITSTREAM -ErrorAction SilentlyContinue
        if ($server -and -not $server.HasExited) { $server | Stop-Process -Force }
    }

    if (-not (Test-Path $Dump) -or (Get-Item $Dump).Length -eq 0) {
        Get-Content $hostLog -Tail 5 -ErrorAction SilentlyContinue | ForEach-Object { Write-Warn $_ }
        throw "could not record here — this machine has no encoder the host could use. Copy a recording from a machine that does and pass it with -Dump."
    }

    Write-Good "recorded $([math]::Round((Get-Item $Dump).Length / 1MB, 1)) MB to $Dump"
}

if (-not (Test-Path $Dump)) { throw "no such recording: $Dump" }

# ── Decode ────────────────────────────────────────────────────────────────────────────────
Write-Step "Decoding"

$decodeArgs = @('decode', '--file', $Dump, '--codec', $Codec)
if ($Verify) { $decodeArgs += '--verify' }

$outcome = Invoke-Native { & $cli @decodeArgs }

Write-Step "Result"

if ($outcome -eq 0) {
    Write-Good "the decoder on this machine reads the stream."
    Write-Host  "   Compare 'submitted' against 'decoded' above: they should be equal."
}
else {
    Write-Host "   the decoder did not produce pictures." -ForegroundColor Red
    Write-Host "   The lines above say which of the two it was: frames refused outright, or"
    Write-Host "   frames accepted that yielded nothing."
}

exit $outcome
