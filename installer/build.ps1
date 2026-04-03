# Chitty Workspace — Full Installer Build Script
# Assembles: Rust binary + embedded Python + sidecar venv + WebView2 bootstrapper
# Then packages into MSIX for Microsoft Store / sideloading
#
# Usage:  .\installer\build.ps1 [-SkipCargoBuild] [-SkipPythonDownload]
# Requires: Rust toolchain, Windows SDK (MakeAppx.exe), ImageMagick

param(
    [switch]$SkipCargoBuild,
    [switch]$SkipPythonDownload
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# ── Paths ─────────────────────────────────────────────────
$ProjectRoot  = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path "$ProjectRoot\Cargo.toml")) {
    $ProjectRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
}
$InstallerDir = Join-Path $ProjectRoot "installer"
$StagingDir   = Join-Path $InstallerDir "staging"
$SidecarSrc   = Join-Path $ProjectRoot "sidecar"
$OutputDir    = Join-Path $InstallerDir "output"

$PythonVersion   = "3.11.9"
$PythonMajorMinor = "311"
$PythonZipName   = "python-$PythonVersion-embed-amd64.zip"
$PythonUrl       = "https://www.python.org/ftp/python/$PythonVersion/$PythonZipName"
$GetPipUrl       = "https://bootstrap.pypa.io/get-pip.py"
$WebView2Url     = "https://go.microsoft.com/fwlink/p/?LinkId=2124703"

# ── Helpers ───────────────────────────────────────────────
function Write-Step($msg) { Write-Host "`n>> $msg" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host "   $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "   $msg" -ForegroundColor Yellow }

# ── Clean staging ─────────────────────────────────────────
Write-Step "Preparing staging directory"
if (Test-Path $StagingDir) {
    # Remove contents (more reliable than removing root on Windows - avoids lock issues)
    Get-ChildItem $StagingDir -Force | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
}
New-Item -ItemType Directory -Path $StagingDir -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $StagingDir "sidecar") -Force | Out-Null
New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null
Write-Ok "Staging: $StagingDir"

# ── Step 1: Cargo build ──────────────────────────────────
if (-not $SkipCargoBuild) {
    Write-Step "Step 1/8: Building Rust binary (cargo build --release)"
    Push-Location $ProjectRoot
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "Cargo build failed" }
    } finally {
        Pop-Location
    }
    Write-Ok "Binary: target\release\chitty-workspace.exe"
} else {
    Write-Step "Step 1/8: Skipping cargo build (flag set)"
    if (-not (Test-Path "$ProjectRoot\target\release\chitty-workspace.exe")) {
        throw "Release binary not found. Run without -SkipCargoBuild first."
    }
}

# ── Step 2: Download embedded Python ─────────────────────
$PythonDir = Join-Path $StagingDir "sidecar\python"

if (-not $SkipPythonDownload) {
    Write-Step "Step 2/8: Downloading Python $PythonVersion embeddable package"
    $PythonZipPath = Join-Path $InstallerDir $PythonZipName

    if (-not (Test-Path $PythonZipPath)) {
        Write-Host "   Downloading $PythonUrl ..."
        Invoke-WebRequest -Uri $PythonUrl -OutFile $PythonZipPath -UseBasicParsing
    } else {
        Write-Ok "Using cached $PythonZipName"
    }

    New-Item -ItemType Directory -Path $PythonDir -Force | Out-Null
    Write-Host "   Extracting to $PythonDir ..."
    Expand-Archive -Path $PythonZipPath -DestinationPath $PythonDir -Force
    Write-Ok "Python extracted"
} else {
    Write-Step "Step 2/8: Skipping Python download (flag set)"
    if (-not (Test-Path "$PythonDir\python.exe")) {
        throw "Embedded Python not found at $PythonDir. Run without -SkipPythonDownload first."
    }
}

# ── Step 3: Enable pip in embedded Python ─────────────────
Write-Step "Step 3/8: Bootstrapping pip into embedded Python"

# The embeddable distribution ships with python311._pth that blocks site-packages.
# We need to uncomment the "import site" line.
$PthFile = Join-Path $PythonDir "python$PythonMajorMinor._pth"
if (Test-Path $PthFile) {
    $content = Get-Content $PthFile -Raw
    $content = $content -replace '#import site', 'import site'
    Set-Content $PthFile $content -NoNewline
    Write-Ok "Enabled site-packages in $PthFile"
}

$GetPipPath = Join-Path $InstallerDir "get-pip.py"
if (-not (Test-Path $GetPipPath)) {
    Write-Host "   Downloading get-pip.py ..."
    Invoke-WebRequest -Uri $GetPipUrl -OutFile $GetPipPath -UseBasicParsing
}

$EmbeddedPython = Join-Path $PythonDir "python.exe"
& $EmbeddedPython $GetPipPath --no-warn-script-location 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "get-pip.py failed" }
Write-Ok "pip installed into embedded Python"

# ── Step 4: Create venv ──────────────────────────────────
Write-Step "Step 4/8: Creating virtual environment"
$VenvDir = Join-Path $StagingDir "sidecar\venv"

# Install virtualenv into embedded python, then create venv
& $EmbeddedPython -m pip install virtualenv --no-warn-script-location 2>&1 | Out-Null
& $EmbeddedPython -m virtualenv $VenvDir 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "Failed to create venv" }
Write-Ok "Venv created: $VenvDir"

# ── Step 5: Install sidecar dependencies ─────────────────
Write-Step "Step 5/8: Installing sidecar dependencies into venv"
$VenvPip = Join-Path $VenvDir "Scripts\pip.exe"
$RequirementsFile = Join-Path $InstallerDir "requirements-installer.txt"

& $VenvPip install -r $RequirementsFile --no-warn-script-location 2>&1 | ForEach-Object {
    if ($_ -match "^(Successfully|Installing|Collecting)") { Write-Host "   $_" }
}
if ($LASTEXITCODE -ne 0) { throw "pip install failed" }
Write-Ok "Dependencies installed"

# ── Step 6: Copy sidecar scripts ─────────────────────────
Write-Step "Step 6/8: Copying sidecar scripts"
Copy-Item (Join-Path $SidecarSrc "inference_server.py") (Join-Path $StagingDir "sidecar\") -Force
Copy-Item (Join-Path $SidecarSrc "media_engine.py") (Join-Path $StagingDir "sidecar\") -Force
Copy-Item $RequirementsFile (Join-Path $StagingDir "sidecar\requirements.txt") -Force
Write-Ok "Sidecar files staged (inference_server.py, media_engine.py)"

# ── Step 7: Clean up build-only bloat ─────────────────────
Write-Step "Step 7/8: Cleaning build-only artifacts from staging"

# Remove pip, setuptools, virtualenv, wheel from embedded Python (only needed during build)
$EmbeddedSitePackages = Join-Path $PythonDir "Lib\site-packages"
$BuildOnlyPackages = @("pip", "pip-*", "setuptools", "setuptools-*", "virtualenv", "virtualenv-*",
                        "wheel", "wheel-*", "distlib", "distlib-*", "filelock", "filelock-*",
                        "platformdirs", "platformdirs-*", "packaging", "packaging-*",
                        "python_discovery", "python_discovery-*", "_distutils_hack",
                        "distutils-precedence.pth")
foreach ($pkg in $BuildOnlyPackages) {
    Get-ChildItem -Path $EmbeddedSitePackages -Filter $pkg -ErrorAction SilentlyContinue | ForEach-Object {
        Remove-Item $_.FullName -Recurse -Force -ErrorAction SilentlyContinue
    }
}
Write-Ok "Removed build-only packages from embedded Python"

# Remove pip from venv (not needed at runtime)
$VenvSitePackages = Join-Path $VenvDir "Lib\site-packages"
foreach ($pkg in @("pip", "pip-*", "setuptools", "setuptools-*", "_distutils_hack", "distutils-precedence.pth")) {
    Get-ChildItem -Path $VenvSitePackages -Filter $pkg -ErrorAction SilentlyContinue | ForEach-Object {
        Remove-Item $_.FullName -Recurse -Force -ErrorAction SilentlyContinue
    }
}
Write-Ok "Removed pip/setuptools from venv"

# Remove __pycache__ directories everywhere
Get-ChildItem -Path $StagingDir -Directory -Recurse -Filter "__pycache__" | ForEach-Object {
    Remove-Item $_.FullName -Recurse -Force
}
Write-Ok "Removed all __pycache__ directories"

# Remove test directories from site-packages
foreach ($sp in @($EmbeddedSitePackages, $VenvSitePackages)) {
    Get-ChildItem -Path $sp -Directory -Recurse -Filter "tests" -ErrorAction SilentlyContinue | ForEach-Object {
        Remove-Item $_.FullName -Recurse -Force -ErrorAction SilentlyContinue
    }
    Get-ChildItem -Path $sp -Directory -Recurse -Filter "test" -ErrorAction SilentlyContinue | ForEach-Object {
        Remove-Item $_.FullName -Recurse -Force -ErrorAction SilentlyContinue
    }
}
Write-Ok "Removed test directories"

# Report cleaned size
$PythonSize = [math]::Round((Get-ChildItem $PythonDir -Recurse -File | Measure-Object -Property Length -Sum).Sum / 1MB, 1)
$VenvSize = [math]::Round((Get-ChildItem $VenvDir -Recurse -File | Measure-Object -Property Length -Sum).Sum / 1MB, 1)
Write-Ok "Embedded Python: ${PythonSize} MB, Venv: ${VenvSize} MB"

# ── Step 8: Download WebView2 bootstrapper ────────────────
Write-Step "Step 8/8: Downloading WebView2 bootstrapper"
$WebView2Path = Join-Path $StagingDir "MicrosoftEdgeWebview2Setup.exe"
if (-not (Test-Path $WebView2Path)) {
    Invoke-WebRequest -Uri $WebView2Url -OutFile $WebView2Path -UseBasicParsing
}
Write-Ok "WebView2 bootstrapper staged"

# ── Build MSIX Package ────────────────────────────────────
Write-Step "Building MSIX package"

$MsixScript = Join-Path $InstallerDir "build-msix.ps1"
& $MsixScript -SkipCargoBuild -SkipSidecar:$false

Write-Host ""
Write-Host "Build complete." -ForegroundColor Cyan
Write-Host ""
