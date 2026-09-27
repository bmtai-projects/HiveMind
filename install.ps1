# Installs the latest `hivemind` release binary for this machine.
#
#   irm https://hivemind.bmtai.in/install.ps1 | iex
#
# Override install location with $env:HIVEMIND_INSTALL_DIR (default: %LOCALAPPDATA%\hivemind).
$ErrorActionPreference = "Stop"
# Some Windows PowerShell 5.1 installs still default to TLS 1.0/1.1, which
# GitHub's endpoints reject -- force 1.2 before making any request.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Repo = "bmtai-projects/HiveMind"
$BinName = "hivemind"
# The only Windows target this repo publishes today (see release.yml).
$Target = "x86_64-pc-windows-msvc"
$InstallDir = if ($env:HIVEMIND_INSTALL_DIR) { $env:HIVEMIND_INSTALL_DIR } else { "$env:LOCALAPPDATA\hivemind" }

Write-Host "Looking up the latest release..."
$Latest = (Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest").tag_name
if (-not $Latest) {
    Write-Error "error: could not determine the latest release of $Repo"
    exit 1
}

$Archive = "$BinName-$Target.zip"
$Url = "https://github.com/$Repo/releases/download/$Latest/$Archive"

Write-Host "Installing $BinName $Latest for $Target..."

$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $TmpDir | Out-Null
try {
    $ArchivePath = Join-Path $TmpDir $Archive
    try {
        Invoke-WebRequest -Uri $Url -OutFile $ArchivePath -UseBasicParsing
    } catch {
        Write-Error "error: no prebuilt binary for $Target in release $Latest."
        exit 1
    }

    Expand-Archive -Path $ArchivePath -DestinationPath $TmpDir -Force

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Move-Item -Path (Join-Path $TmpDir "$BinName.exe") -Destination (Join-Path $InstallDir "$BinName.exe") -Force
} finally {
    Remove-Item -Recurse -Force $TmpDir
}

Write-Host "Installed to $InstallDir\$BinName.exe"

# Unlike the Unix installer (which just prints an `export PATH=...` line to
# add manually), Windows has no per-shell profile file every shell reads by
# default -- editing the User PATH via the environment-variable store is the
# actual equivalent. Only the *store* updates here; already-open terminals
# keep their old PATH until reopened, which is why the message below says so.
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (";$UserPath;" -notlike "*;$InstallDir;*") {
    $NewPath = if ([string]::IsNullOrEmpty($UserPath)) { $InstallDir } else { "$UserPath;$InstallDir" }
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    Write-Host ""
    Write-Host "Added $InstallDir to your PATH. Open a NEW PowerShell window for this to take effect."
}

Write-Host ""
Write-Host "hivemind is a command-line tool: run it from a terminal (this PowerShell window, once"
Write-Host "reopened) -- double-clicking hivemind.exe in File Explorer opens and immediately closes"
Write-Host "a console window, which just looks like a crash."
Write-Host ""
Write-Host "Next (in a new PowerShell window):"
Write-Host "  $BinName auth login    # sign in and pay as you go, no key needed"
Write-Host "  $BinName activate"
Write-Host ""
Write-Host "Or bring your own key:"
Write-Host '  $env:HIVEMIND_API_KEY = "..."'
Write-Host "  $BinName activate"
