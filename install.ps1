#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = "craft-build/craft"
$Binary = "craft"
$InstallDir = if ($env:CRAFT_INSTALL_DIR) {
    $env:CRAFT_INSTALL_DIR
} else {
    Join-Path $HOME ".cargo\bin"
}
$PlaywrightVersion = "1.60.0"

function Write-Err([string]$Message) {
    [Console]::Error.WriteLine("error: $Message")
    exit 1
}

function Test-Command([string]$Name) {
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

function Get-LatestTag {
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
    } catch {
        Write-Err "failed to determine latest release tag: $_"
    }
    $tag = $release.tag_name
    if (-not $tag) {
        Write-Err "failed to determine latest release tag"
    }
    return $tag
}

function Install-Craft([string]$Tag) {
    if (-not (Test-Command "cargo")) {
        Write-Err "cargo not found. Install Rust from https://rustup.rs and re-run this script."
    }

    Write-Host "looking up the latest release"
    if (-not $Tag) {
        $Tag = Get-LatestTag
    }

    Write-Host "building $Binary $Tag from source (this compiles all dependencies and can take several minutes)"
    cargo install --locked --force --git "https://github.com/$Repo.git" --tag $Tag $Binary

    if (-not (Test-Path -LiteralPath $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }
    Add-ToUserPath -Dir $InstallDir

    Write-Host ""
    if (Test-Command "npm") {
        Write-Host "Browser tooling (optional) needs the Playwright driver:"
        Write-Host "  npm install -g playwright@$PlaywrightVersion"
    } else {
        [Console]::Error.WriteLine("warning: Browser tooling needs the Playwright driver, but npm was not found.")
        [Console]::Error.WriteLine("warning: Install Node.js, then run: npm install -g playwright@$PlaywrightVersion")
    }

    Write-Host ""
    Write-Host "done. Run 'craft --version' to verify."
}

function Add-ToUserPath([string]$Dir) {
    $sep = [IO.Path]::PathSeparator
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($null -eq $userPath) {
        $userPath = ""
    }
    $entries = $userPath -split [regex]::Escape($sep) | Where-Object { $_ -ne "" }
    $already = $entries | Where-Object { $_.TrimEnd('\') -ieq $Dir.TrimEnd('\') }
    if ($already) {
        return
    }

    $newPath = if ($userPath.Trim()) { "$userPath$sep$Dir" } else { $Dir }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    $env:Path = "$env:Path$sep$Dir"
    Write-Host "added $Dir to user PATH (restart terminal if $Binary is not found)"
}

$tag = if ($args.Count -ge 1) { $args[0] } else { $null }
Install-Craft -Tag $tag
