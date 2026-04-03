# Chitty Workspace - MSIX Package Builder
# Assembles: Rust binary + sidecar (embedded Python + venv) + icons into MSIX
# For Microsoft Store submission and direct sideloading
#
# MSIX handles upgrades automatically — same Identity Name with higher Version
# replaces the previous install cleanly (files, registry, shortcuts).
#
# Usage:  .\installer\build-msix.ps1 [-SkipCargoBuild] [-SkipSidecar]
# Requires: Windows SDK (MakeAppx.exe, MakePri.exe), Rust toolchain, ImageMagick

param(
    [switch]$SkipCargoBuild,
    [switch]$SkipSidecar
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# ── Paths ─────────────────────────────────────────────────
$ProjectRoot = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path "$ProjectRoot\Cargo.toml")) {
    $ProjectRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
}
$InstallerDir = Join-Path $ProjectRoot "installer"
$MsixDir      = Join-Path $InstallerDir "msix"
$LayoutDir    = Join-Path $InstallerDir "msix-layout"
$OutputDir    = Join-Path $InstallerDir "output"
$SidecarSrc   = Join-Path $ProjectRoot "sidecar"
$StagingDir   = Join-Path $InstallerDir "staging"

# ── Read version from Cargo.toml (single source of truth) ─
$CargoToml = Get-Content (Join-Path $ProjectRoot "Cargo.toml") -Raw
if ($CargoToml -match 'version\s*=\s*"(\d+\.\d+\.\d+)"') {
    $AppVersion = $Matches[1]
} else {
    throw "Could not parse version from Cargo.toml"
}
$MsixVersion = "$AppVersion.0"  # MSIX requires 4-part version (Major.Minor.Patch.Build)
Write-Host "Version: $AppVersion (MSIX: $MsixVersion)" -ForegroundColor Cyan

# ── Helpers ───────────────────────────────────────────────
function Write-Step($msg) { Write-Host "`n>> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host "   $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "   $msg" -ForegroundColor Yellow }

# ── Find Windows SDK tools ────────────────────────────────
function Find-SdkTool($name) {
    $sdkRoot = "C:\Program Files (x86)\Windows Kits\10\bin"
    if (Test-Path $sdkRoot) {
        $tool = Get-ChildItem -Path $sdkRoot -Recurse -Filter $name -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match "x64" } |
            Sort-Object { [version]($_.FullName -replace '.*\\(\d+\.\d+\.\d+\.\d+)\\.*', '$1') } -Descending |
            Select-Object -First 1
        if ($tool) { return $tool.FullName }
    }
    # Try PATH
    $cmd = Get-Command $name -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    return $null
}

$MakeAppx = Find-SdkTool "makeappx.exe"
$MakePri  = Find-SdkTool "makepri.exe"

if (-not $MakeAppx) { throw "MakeAppx.exe not found. Install Windows SDK: winget install Microsoft.WindowsSDK" }
if (-not $MakePri)  { throw "MakePri.exe not found. Install Windows SDK: winget install Microsoft.WindowsSDK" }

Write-Host "MakeAppx: $MakeAppx"
Write-Host "MakePri:  $MakePri"

# ── Clean layout ──────────────────────────────────────────
Write-Step "Step 1/7: Preparing MSIX layout directory"
if (Test-Path $LayoutDir) {
    Get-ChildItem $LayoutDir -Force | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
}
New-Item -ItemType Directory -Path $LayoutDir -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $LayoutDir "Images") -Force | Out-Null
New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null
Write-Ok "Layout: $LayoutDir"

# ── Step 2: Cargo build ──────────────────────────────────
if (-not $SkipCargoBuild) {
    Write-Step "Step 2/7: Building Rust binary (cargo build --release)"
    Push-Location $ProjectRoot
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "Cargo build failed" }
    } finally {
        Pop-Location
    }
    Write-Ok "Binary built"
} else {
    Write-Step "Step 2/7: Skipping cargo build"
    if (-not (Test-Path "$ProjectRoot\target\release\chitty-workspace.exe")) {
        throw "Release binary not found. Run without -SkipCargoBuild first."
    }
}

# ── Step 3: Generate MSIX icons from SVG ──────────────────
Write-Step "Step 3/7: Generating MSIX icon assets from SVG"

$SvgPath = Join-Path $ProjectRoot "assets\chitty-icon.svg"
$ImagesDir = Join-Path $LayoutDir "Images"

# Find ImageMagick
$Magick = $null
$MagickPaths = @(
    "C:\Program Files\ImageMagick-7.1.2-Q16-HDRI\magick.exe"
)
# Search common install locations
Get-ChildItem "C:\Program Files\ImageMagick*" -Directory -ErrorAction SilentlyContinue | ForEach-Object {
    $candidate = Join-Path $_.FullName "magick.exe"
    if (Test-Path $candidate) { $MagickPaths += $candidate }
}
$MagickCmd = Get-Command "magick" -ErrorAction SilentlyContinue
if ($MagickCmd) { $MagickPaths += $MagickCmd.Source }
foreach ($p in $MagickPaths) {
    if ($p -and (Test-Path $p)) { $Magick = $p; break }
}

if (-not $Magick) { throw "ImageMagick (magick.exe) not found. Install: winget install ImageMagick.ImageMagick" }
Write-Ok "ImageMagick: $Magick"

# Required MSIX icon sizes: name -> [width, height]
$iconSizes = @(
    @{ Name = "Square44x44Logo.png"; W = 44; H = 44 },
    @{ Name = "Square44x44Logo.targetsize-44_altform-unplated.png"; W = 44; H = 44 },
    @{ Name = "Square150x150Logo.png"; W = 150; H = 150 },
    @{ Name = "StoreLogo.png"; W = 50; H = 50 },
    @{ Name = "Square310x310Logo.png"; W = 310; H = 310 },
    @{ Name = "Wide310x150Logo.png"; W = 310; H = 150 }
)

foreach ($icon in $iconSizes) {
    $outPath = Join-Path $ImagesDir $icon.Name
    $size = "$($icon.W)x$($icon.H)"

    if ($icon.W -ne $icon.H) {
        # Wide logo: render square then extend canvas
        $squareSize = $icon.H
        & $Magick -background transparent -density 300 $SvgPath -resize "${squareSize}x${squareSize}" -gravity center -extent $size $outPath 2>$null
    } else {
        & $Magick -background transparent -density 300 $SvgPath -resize $size $outPath 2>$null
    }
    Write-Ok "$($icon.Name) ($size)"
}

# ── Step 4: Assemble layout ──────────────────────────────
Write-Step "Step 4/7: Assembling MSIX layout"

# Copy main binary
Copy-Item "$ProjectRoot\target\release\chitty-workspace.exe" (Join-Path $LayoutDir "ChittyWorkspace.exe") -Force
Write-Ok "ChittyWorkspace.exe"

# Copy manifest
Copy-Item (Join-Path $MsixDir "AppxManifest.xml") $LayoutDir -Force
Write-Ok "AppxManifest.xml"

# Copy sidecar if staging exists (from build.ps1) and not skipped
if (-not $SkipSidecar) {
    $SidecarStaging = Join-Path $StagingDir "sidecar"
    if (Test-Path $SidecarStaging) {
        $SidecarDest = Join-Path $LayoutDir "sidecar"
        Write-Host "   Copying sidecar from staging..."
        Copy-Item $SidecarStaging $SidecarDest -Recurse -Force
        Write-Ok "sidecar/ (Python + venv + inference_server.py + media_engine.py)"
    } else {
        # No staging — copy sidecar scripts directly (user will need Python installed)
        Write-Warn "Sidecar staging not found. Copying scripts only (no embedded Python)."
        $SidecarDest = Join-Path $LayoutDir "sidecar"
        New-Item -ItemType Directory -Path $SidecarDest -Force | Out-Null
        Copy-Item (Join-Path $SidecarSrc "inference_server.py") $SidecarDest -Force
        Copy-Item (Join-Path $SidecarSrc "media_engine.py") $SidecarDest -Force
        Copy-Item (Join-Path $SidecarSrc "requirements.txt") $SidecarDest -Force
        Copy-Item (Join-Path $SidecarSrc "requirements-full.txt") $SidecarDest -Force
        Write-Ok "sidecar/ (scripts only, no embedded Python)"
    }
} else {
    Write-Step "   Skipping sidecar (flag set)"
}

# Copy browser extension
$ExtensionSrc = Join-Path $ProjectRoot "extension"
if (Test-Path $ExtensionSrc) {
    $ExtensionDest = Join-Path $LayoutDir "extension"
    Copy-Item $ExtensionSrc $ExtensionDest -Recurse -Force
    Write-Ok "extension/ (Chitty Browser Extension)"
} else {
    Write-Warn "Browser extension not found at $ExtensionSrc"
}

# ── Step 5: Generate PRI (Package Resource Index) ─────────
Write-Step "Step 5/7: Generating resources.pri"

Push-Location $LayoutDir
try {
    & $MakePri createconfig /cf priconfig.xml /dq en-US /o 2>&1 | Out-Null
    & $MakePri new /pr . /cf priconfig.xml /of resources.pri /o 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "MakePri failed" }
    # Clean up config file
    Remove-Item priconfig.xml -Force -ErrorAction SilentlyContinue
} finally {
    Pop-Location
}
Write-Ok "resources.pri generated"

# ── Step 6: Pack & Sign MSIX ──────────────────────────────
Write-Step "Step 6/7: Creating MSIX package"

# Stamp the version into AppxManifest.xml before packaging
$ManifestPath = Join-Path $LayoutDir "AppxManifest.xml"
$ManifestContent = Get-Content $ManifestPath -Raw
$ManifestContent = $ManifestContent -replace '(<Identity[^>]*?)Version="\d+\.\d+\.\d+\.\d+"', "`$1Version=`"$MsixVersion`""
Set-Content $ManifestPath $ManifestContent -NoNewline
Write-Ok "AppxManifest.xml stamped with version $MsixVersion"

$MsixOutput = Join-Path $OutputDir "ChittyWorkspace-$AppVersion-x64.msix"
& $MakeAppx pack /d $LayoutDir /p $MsixOutput /o 2>&1 | ForEach-Object {
    if ($_ -match "(Created|Package)") { Write-Host "   $_" }
}
if ($LASTEXITCODE -ne 0) { throw "MakeAppx pack failed" }

Write-Ok "MSIX packed"

# -- Step 7: Sign MSIX -----------------------------------------
Write-Step "Step 7/7: Signing MSIX package"

$PfxPath = Join-Path $InstallerDir "dev-signing.pfx"
if (Test-Path $PfxPath) {
    # Find SignTool
    $SignTool = Find-SdkTool "signtool.exe"
    if ($SignTool) {
        & $SignTool sign /fd SHA256 /f $PfxPath /p chittydev $MsixOutput 2>&1 | ForEach-Object {
            if ($_ -match "(Successfully|Error)") { Write-Host "   $_" }
        }
        if ($LASTEXITCODE -ne 0) { throw "SignTool signing failed" }
        Write-Ok "MSIX signed with dev-signing.pfx"
    } else {
        Write-Warn "SignTool.exe not found - MSIX is unsigned. Install Windows SDK."
    }
} else {
    Write-Warn "dev-signing.pfx not found - MSIX is unsigned. Run create-dev-cert.ps1 first."
}

$MsixSize = [math]::Round((Get-Item $MsixOutput).Length / 1MB, 1)
Write-Ok "Build complete!"
Write-Host ""
Write-Host ("   Output: " + $MsixOutput) -ForegroundColor Green
Write-Host ("   Size:   " + $MsixSize + " MB") -ForegroundColor Green
Write-Host ""
Write-Host "Next steps:" -ForegroundColor Cyan
Write-Host "   1. Upload to Partner Center: msstore publish" -ForegroundColor White
Write-Host "   2. Or test locally: Add-AppxPackage -Path '$MsixOutput'" -ForegroundColor White
Write-Host ""
