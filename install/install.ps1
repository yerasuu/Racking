<#
.SYNOPSIS
  Installs the Racking clients (rack.exe, racking-mcp.exe) for the current user on Windows.
.DESCRIPTION
  Run from a checkout it uses the binaries in -From (default: target\release, or the folder of
  this script when the .exe files sit beside it), building them with cargo first when they are
  missing. Run without a checkout (piped from GitHub) or with -Version, it downloads that
  GitHub release and checks its SHA-256. Installs into %LOCALAPPDATA%\Racking\bin and adds that folder to the user PATH.
  No administrator rights needed. Safe to run again.
.EXAMPLE
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/yerasuu/Racking/main/install/install.ps1))) -Version 0.2.1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File install\install.ps1 -DryRun
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File install\install.ps1 -Uninstall
#>
param(
    [string]$From,
    [string]$Version,
    [string]$BinDir = (Join-Path $env:LOCALAPPDATA 'Racking\bin'),
    [switch]$Uninstall,
    [switch]$DryRun
)
$ErrorActionPreference = 'Stop'

$names = 'rack.exe', 'racking-mcp.exe'
$repo = 'yerasuu/Racking'
# Piped from GitHub there is no script file, hence no checkout.
$root = if ($PSScriptRoot) { Split-Path -Parent $PSScriptRoot } else { $null }
$checkout = $root -and (Test-Path (Join-Path $root 'Cargo.toml'))

function Step($text) { if ($DryRun) { Write-Host "[dry-run] $text" } }

function Update-UserPath([string]$dir, [bool]$add) {
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @($current -split ';' | Where-Object { $_ -ne '' })
    $has = $parts | Where-Object { $_.TrimEnd('\') -ieq $dir.TrimEnd('\') }
    if ($add -and -not $has) {
        Step "add $dir to the user PATH"
        if (-not $DryRun) { [Environment]::SetEnvironmentVariable('Path', (($parts + $dir) -join ';'), 'User') }
        Write-Host "Added $dir to your PATH: open a new terminal to use it."
    } elseif (-not $add -and $has) {
        Step "remove $dir from the user PATH"
        if (-not $DryRun) {
            $keep = $parts | Where-Object { $_.TrimEnd('\') -ine $dir.TrimEnd('\') }
            [Environment]::SetEnvironmentVariable('Path', ($keep -join ';'), 'User')
        }
    }
}

if ($Uninstall) {
    foreach ($n in $names) {
        foreach ($f in (Join-Path $BinDir $n), (Join-Path $BinDir "$n.old")) {
            if (Test-Path $f) {
                Step "remove $f"
                if (-not $DryRun) { Remove-Item -Force $f }
            }
        }
    }
    Update-UserPath $BinDir $false
    Write-Host "Removed rack and racking-mcp from $BinDir."
    Write-Host "The config ($env:APPDATA\rack\rack.conf) was left in place."
    exit 0
}

$explicit = [bool]$From
$tmp = $null
if ($Version -or (-not $explicit -and -not $checkout)) {
    if ($explicit) { throw '-From and -Version cannot be combined' }
    if ($Version) {
        $Version = $Version.TrimStart('v')
        if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "bad -Version (stable releases only, X.Y.Z): $Version" }
    }
    if (-not [Environment]::Is64BitOperatingSystem) { throw 'the prebuilt release is x86_64 only; build from source: cargo install --git https://github.com/yerasuu/Racking rack racking-mcp' }
    $asset = 'rack-x86_64-pc-windows-msvc.zip'
    $base = if ($env:RACKING_RELEASE_URL) { $env:RACKING_RELEASE_URL }   # for tests
            elseif ($Version) { "https://github.com/$repo/releases/download/v$Version" }
            else { "https://github.com/$repo/releases/latest/download" }
    Step "download $base/$asset and $base/SHA256SUMS, verify, unpack, install"
    if (-not $DryRun) {
        $tmp = Join-Path ([IO.Path]::GetTempPath()) ("racking-" + [Guid]::NewGuid())
        New-Item -ItemType Directory -Path $tmp | Out-Null
        Write-Host "Downloading $base/$asset"
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -UseBasicParsing "$base/$asset" -OutFile (Join-Path $tmp $asset)
        Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')
        $want = $null
        foreach ($line in Get-Content (Join-Path $tmp 'SHA256SUMS')) {
            $f = $line -split '\s+', 2
            if ($f.Count -eq 2 -and $f[1].TrimStart('*') -eq $asset) { $want = $f[0] }
        }
        $have = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash
        if (-not $want -or $want -ine $have) { throw "SHA-256 mismatch for ${asset}: nothing installed" }
        Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
        $From = $tmp
    } else { $From = '.' }
} else {
    if (-not $From) {
        # A release folder with the .exe files beside this script wins over a checkout.
        $beside = if ($PSCommandPath) { Split-Path -Parent $PSCommandPath } else { $null }
        if ($beside -and (Test-Path (Join-Path $beside 'rack.exe'))) { $From = $beside } else { $From = Join-Path $root 'target\release' }
    }
    $missing = $names | Where-Object { -not (Test-Path (Join-Path $From $_)) }
    if ($missing) {
        if ($explicit) { throw "not found in ${From}: $($missing -join ', ')" }
        if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
            throw "no binaries in $From and cargo is not installed (get Rust from https://rustup.rs, or pass -From DIR)"
        }
        Write-Host 'Building the clients with cargo (Rust 1.89 or newer).'
        Step 'cargo build --release --locked -p rack -p racking-mcp'
        if (-not $DryRun) {
            Push-Location $root
            try {
                cargo build --release --locked -p rack -p racking-mcp
                if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
            } finally { Pop-Location }
        }
    }
}

Step "create $BinDir"
if (-not $DryRun) { New-Item -ItemType Directory -Force -Path $BinDir | Out-Null }
foreach ($n in $names) {
    $dest = Join-Path $BinDir $n
    # Windows will not overwrite a running .exe but lets it be renamed: keep the old one as .old,
    # the same way `rack update` does.
    if (Test-Path $dest) {
        Step "move $dest to $n.old"
        if (-not $DryRun) { Move-Item -Force $dest "$dest.old" }
    }
    Step "copy $(Join-Path $From $n) to $dest"
    if (-not $DryRun) { Copy-Item -Force (Join-Path $From $n) $dest }
    Write-Host "Installed: $dest"
}
if ($tmp) { Remove-Item -Recurse -Force $tmp }
Update-UserPath $BinDir $true

Write-Host 'Next: rack join CODE --server URL   (see README, "Link a machine"). Later updates: rack update.'
