# The owner's first contact with tether, on Windows.
#
#   & ([scriptblock]::Create((irm https://nexus.tony-xy-liu.com/tether/win))) 7 anchor kettle
#
# Served by the relay at /win under the mount, because Windows has no shell that
# can run the script served at the mount itself. It fetches the client for this
# machine, runs it with whatever followed the call, and stops. Nothing is
# installed: no PATH entry, no startup item, no scheduled task, no service. What
# is left behind is this directory: the client, and the record of the session
# written beside it. The directory is named below before anything runs, and the
# record is named again in the prompt before the owner answers it.
#
# # Why the line is shaped like that
#
# A script arriving through a pipe cannot be given arguments, and the invite
# code is arguments. Building a script block from the download and calling it is
# the shape that keeps the code on the same line as the address. TETHER_CODE is
# the second route for anyone who finds that line hard to read out:
#
#   $env:TETHER_CODE = '7 anchor kettle'
#   irm https://nexus.tony-xy-liu.com/tether/win | iex
#
# # Why there is no checksum here
#
# The binary and this script are served by the same host over the same TLS
# connection, so anybody able to alter one can alter the other, and would simply
# write a matching checksum. A hash here would look like a defence and be none.
# What the owner is actually trusting is the address they were read, which is
# why that address is shown again by the client before it asks them anything.

$ErrorActionPreference = 'Stop'

function Die($message) {
    Write-Error "tether: $message"
    exit 1
}

$base = if ($env:TETHER_RELAY) { $env:TETHER_RELAY } else { 'https://nexus.tony-xy-liu.com/tether' }

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

$code = $args
if (-not $code -and $env:TETHER_CODE) { $code = $env:TETHER_CODE.Split(' ') }
if (-not $code) { Die 'no invite code: pass the slot and the words you were read' }

$dir = Join-Path $env:TEMP ("tether." + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $dir | Out-Null
$bin = Join-Path $dir 'tether.exe'

try {
    Invoke-WebRequest -UseBasicParsing -Uri "$base/bin/$asset" -OutFile $bin
} catch {
    Die "could not download the client for this machine ($asset) from $base"
}
if (-not (Test-Path $bin) -or (Get-Item $bin).Length -eq 0) {
    Die "the download was empty; the relay may not be serving a client for $asset"
}

# Windows marks a downloaded file as having come from elsewhere, and refuses to
# run it with a dialog the owner cannot answer over the phone. Clearing our own
# download is the same decision the owner would make in that dialog, made where
# they can see it.
try { Unblock-File -Path $bin } catch {}

Write-Host "tether: running $bin"
Write-Host "tether: delete $dir when you are done - the client and its log are in it."

& $bin @code
exit $LASTEXITCODE
