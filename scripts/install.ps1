# Install the Cormex capture CLI on Windows.
#
#   irm https://raw.githubusercontent.com/weironz/cormex/main/scripts/install.ps1 | iex
#
# # Why the checksum is verified
#
# TLS protects the transit, not the file at rest. `SHA256SUMS` comes from the
# same Release, so it does not defend against a compromised release — what it
# defends against is a truncated or corrupted download, which is the failure
# that actually happens.
#
# # Why `$env:LOCALAPPDATA\cormex\bin` and not `Program Files`
#
# Installing into `Program Files` needs elevation, and a capture client has no
# business asking for it: it runs as the user, reads the user's files, and
# stores the user's key. A per-user directory also means `uninstall` can remove
# it without an administrator.

$ErrorActionPreference = 'Stop'

$Repo = 'weironz/cormex'
$Bin = 'cormex'

function Fail($Message) {
    Write-Error $Message
    exit 1
}

# ── Platform ──────────────────────────────────────────────
$Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default { Fail "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}
$Target = "$Arch-pc-windows-msvc"

# ── Version ───────────────────────────────────────────────
$Version = $env:CORMEX_VERSION
if (-not $Version) {
    # The redirect's final path segment is the tag
    $resp = Invoke-WebRequest -Uri "https://github.com/$Repo/releases/latest" `
        -MaximumRedirection 0 -ErrorAction SilentlyContinue -SkipHttpErrorCheck
    $location = $resp.Headers.Location
    if (-not $location) { Fail 'could not work out the latest version; set $env:CORMEX_VERSION' }
    $Version = ($location -split '/')[-1]
}
if (-not $Version) { Fail 'could not work out the latest version; set $env:CORMEX_VERSION' }

Write-Host "installing $Bin $Version for $Target"

# ── Download ──────────────────────────────────────────────
$Tmp = Join-Path $env:TEMP ("cormex-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    $Base = "https://github.com/$Repo/releases/download/$Version"
    $Archive = "$Bin-$($Version.TrimStart('v'))-$Target.zip"
    $ArchivePath = Join-Path $Tmp $Archive
    $SumsPath = Join-Path $Tmp 'SHA256SUMS'

    Invoke-WebRequest -Uri "$Base/$Archive" -OutFile $ArchivePath
    Invoke-WebRequest -Uri "$Base/SHA256SUMS" -OutFile $SumsPath

    # ── Verify ────────────────────────────────────────────
    $line = Get-Content $SumsPath | Where-Object { $_ -match " $([regex]::Escape($Archive))$" }
    if (-not $line) { Fail "SHA256SUMS does not list $Archive" }
    $expected = ($line -split '\s+')[0]
    $actual = (Get-FileHash -Algorithm SHA256 -Path $ArchivePath).Hash.ToLower()
    if ($expected -ne $actual) {
        Fail "checksum mismatch for $Archive`n  expected $expected`n  actual   $actual`nThe download is corrupt. Try again."
    }

    # ── Install ───────────────────────────────────────────
    Expand-Archive -Path $ArchivePath -DestinationPath $Tmp -Force
    $Dir = Join-Path $env:LOCALAPPDATA "cormex\bin"
    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    $exe = Join-Path $Tmp "$Bin-$($Version.TrimStart('v'))-$Target\$Bin.exe"
    Copy-Item $exe (Join-Path $Dir "$Bin.exe") -Force

    # Add to the user's PATH if it is not there. `User` scope, so no elevation
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -notlike "*$Dir*") {
        [Environment]::SetEnvironmentVariable('Path', "$userPath;$Dir", 'User')
        Write-Host ''
        Write-Host "added $Dir to your user PATH; open a new terminal to pick it up"
    }

    Write-Host ''
    Write-Host "installed to $Dir\$Bin.exe"
    Write-Host ''
    Write-Host "next: $Bin login --server <your cormex server>"
    Write-Host "then: $Bin init"
}
finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
