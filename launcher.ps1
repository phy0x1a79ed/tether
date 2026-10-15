# The owner's first contact with tether, on Windows.
#
#   & ([scriptblock]::Create((irm https://nexus.tony-xy-liu.com/tether/win))) acre anchor kettle
#
# Served by the relay at /win under the mount, because Windows has no shell that
# can run the script served at the mount itself. It fetches the client for this
# machine, checks that the bytes are the ones this release published, runs it
# with whatever followed the call, and stops. Nothing is installed: no PATH
# entry, no startup item, no scheduled task, no service. What is left behind is
# this directory: the client, and the record of the session written beside it.
# The directory is named below before anything runs, and the record is named
# again in the prompt before the owner answers it.
#
# # Why the line is shaped like that
#
# A script arriving through a pipe cannot be given arguments, and the invite
# code is arguments. Building a script block from the download and calling it is
# the shape that keeps the code on the same line as the address. TETHER_CODE is
# the second route for anyone who finds that line hard to read out:
#
#   $env:TETHER_CODE = 'acre anchor kettle'
#   irm https://nexus.tony-xy-liu.com/tether/win | iex
#
# # Why there is a checksum here now
#
# The client no longer comes from the same host as this script. A release
# publishes it to GitHub, this launcher tries GitHub first, and the relay is
# only the fallback for a machine that cannot reach GitHub. Two hosts means the
# old reasoning — alter one, alter the other — no longer holds, so the sha256 of
# every client is baked into this file when the release is cut, and a download
# whose hash does not match is refused rather than run.

$ErrorActionPreference = 'Stop'

function Die($message) {
    Write-Error "tether: $message"
    exit 1
}

# --- baked by release.sh (do not edit between the markers) ---
# Empty here means this is the source copy, not a released launcher. The source
# refuses to run: a raw checkout has no checksums, so it could only fetch
# unverified. release.sh fills these in when it cuts a release.
$TetherRepo = 'phy0x1a79ed/tether'
$TetherTag = ''
$TetherSums = @'
'@
# --- end baked ---

if (-not $TetherTag) {
    Die 'this is the launcher source, which ships no checksums. Fetch a released launcher from the relay or the repository''s releases.'
}

# GitHub is the first source; the relay is the fallback a machine behind an
# allow-list that blocks GitHub still reaches. TETHER_RELAY moves the fallback,
# TETHER_GITHUB_BASE moves the first source, and TETHER_NO_GITHUB=1 skips GitHub.
$base = if ($env:TETHER_RELAY) { $env:TETHER_RELAY } else { 'https://nexus.tony-xy-liu.com/tether' }
$githubBase = if ($env:TETHER_GITHUB_BASE) { $env:TETHER_GITHUB_BASE } else { "https://github.com/$TetherRepo/releases/download" }

# Windows PowerShell 5.1 still negotiates TLS 1.0 on a default install, and the
# relay does not answer it. Saying this costs nothing where it is already true.
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch {}

# The 32-bit variable reports x86 when a 32-bit PowerShell runs on a 64-bit
# machine, and the second variable is how that machine says what it really is.
$machine = $env:PROCESSOR_ARCHITEW6432
if (-not $machine) { $machine = $env:PROCESSOR_ARCHITECTURE }

switch ($machine) {
    # One client covers both. Windows on ARM runs an x64 binary through its own
    # emulation, and a native ARM64 build would buy nothing an owner could
    # notice.
    'AMD64' { $asset = 'tether-windows-x86_64.exe' }
    'ARM64' { $asset = 'tether-windows-x86_64.exe' }
    default { Die "this machine is $machine, which tether has no client for" }
}

# The hash this release published for the asset being fetched.
$want = $null
foreach ($line in $TetherSums -split "`n") {
    $parts = $line.Trim() -split '\s+'
    if ($parts.Count -ge 2 -and $parts[1] -eq $asset) { $want = $parts[0].ToLower(); break }
}
if (-not $want) { Die "this release published no client called $asset" }

$code = $args
if (-not $code -and $env:TETHER_CODE) { $code = $env:TETHER_CODE.Split(' ') }
if (-not $code) { Die 'no invite code: pass the three words you were read' }

$dir = Join-Path $env:TEMP ("tether." + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $dir | Out-Null
$bin = Join-Path $dir 'tether.exe'

# GitHub first, then the relay. A download that fails falls through to the next
# source; a download that succeeds but whose hash is wrong is refused here and
# does not fall through, because a wrong hash is tampering or corruption.
$sources = @()
if (-not $env:TETHER_NO_GITHUB) { $sources += "$githubBase/$TetherTag/$asset" }
$sources += "$base/bin/$asset"

$got = $null
foreach ($url in $sources) {
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $bin
    } catch {
        continue
    }
    if (-not (Test-Path $bin) -or (Get-Item $bin).Length -eq 0) { continue }
    $have = (Get-FileHash -Algorithm SHA256 -Path $bin).Hash.ToLower()
    if ($have -ne $want) {
        Write-Error "tether: $url does not match the checksum this release published."
        Write-Error "tether: expected $want"
        Write-Error "tether: received $have"
        Die 'the download was altered or corrupted; nothing was run.'
    }
    $got = $url
    break
}
if (-not $got) { Die "could not download the client for this machine ($asset) from GitHub or $base" }
Write-Host "tether: fetched $got"

# Windows marks a downloaded file as having come from elsewhere, and refuses to
# run it with a dialog the owner cannot answer over the phone. Clearing our own
# download is the same decision the owner would make in that dialog, made where
# they can see it.
try { Unblock-File -Path $bin } catch {}

Write-Host "tether: running $bin"
Write-Host "tether: delete $dir when you are done - the client and its log are in it."

& $bin @code
exit $LASTEXITCODE
