[CmdletBinding()]
param(
    [string]$Version,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "codexshim\bin"),
    [string]$ReleaseDirectory
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2.0

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "codexshim requires 64-bit Windows."
}
if (-not $InstallDir) {
    throw "InstallDir is required."
}

[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$target = "x86_64-pc-windows-msvc"
$temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ("codexshim-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null

try {
    if ($ReleaseDirectory) {
        $archive = @(Get-ChildItem -LiteralPath $ReleaseDirectory -Filter "codexshim-*-$target.zip" -File)
        if ($archive.Count -ne 1) {
            throw "Expected exactly one Windows release archive in $ReleaseDirectory."
        }
        $archivePath = $archive.FullName
        $checksumPath = "$archivePath.sha256"
        if (-not (Test-Path -LiteralPath $checksumPath -PathType Leaf)) {
            throw "Missing checksum file: $checksumPath"
        }
    } else {
        if ($Version) {
            $tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
            $releaseUrl = "https://api.github.com/repos/possible055/codexshim/releases/tags/$tag"
        } else {
            $releaseUrl = "https://api.github.com/repos/possible055/codexshim/releases/latest"
        }

        $headers = @{ Accept = "application/vnd.github+json" }
        $release = Invoke-RestMethod -Uri $releaseUrl -Headers $headers
        $archiveAsset = @($release.assets | Where-Object { $_.name -like "codexshim-*-$target.zip" })
        if ($archiveAsset.Count -ne 1) {
            throw "The release does not contain exactly one Windows archive."
        }
        $checksumAsset = @($release.assets | Where-Object { $_.name -eq ($archiveAsset[0].name + ".sha256") })
        if ($checksumAsset.Count -ne 1) {
            throw "The release does not contain the archive checksum."
        }

        $archivePath = Join-Path $temporaryDirectory $archiveAsset[0].name
        $checksumPath = "$archivePath.sha256"
        Invoke-WebRequest -Uri $archiveAsset[0].browser_download_url -OutFile $archivePath
        Invoke-WebRequest -Uri $checksumAsset[0].browser_download_url -OutFile $checksumPath
    }

    $checksumLine = Get-Content -LiteralPath $checksumPath | Select-Object -First 1
    if ($checksumLine -notmatch '^([0-9a-fA-F]{64})\s+\*?(.+)$') {
        throw "Invalid checksum file: $checksumPath"
    }
    if ($Matches[2] -ne (Split-Path -Leaf $archivePath)) {
        throw "Checksum filename does not match the release archive."
    }
    $expectedHash = $Matches[1].ToLowerInvariant()
    $actualHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $expectedHash) {
        throw "Checksum verification failed for $(Split-Path -Leaf $archivePath)."
    }

    $extractDirectory = Join-Path $temporaryDirectory "extract"
    New-Item -ItemType Directory -Path $extractDirectory | Out-Null
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::ExtractToDirectory($archivePath, $extractDirectory)
    $binary = @(Get-ChildItem -LiteralPath $extractDirectory -Filter "codexshim.exe" -File -Recurse)
    if ($binary.Count -ne 1) {
        throw "The release archive does not contain exactly one codexshim.exe."
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $destination = Join-Path (Resolve-Path -LiteralPath $InstallDir).Path "codexshim.exe"
    $staged = Join-Path $InstallDir (".codexshim-" + [guid]::NewGuid().ToString("N") + ".exe")
    Copy-Item -LiteralPath $binary[0].FullName -Destination $staged
    & $staged --version | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "The downloaded codexshim executable failed verification."
    }

    $backup = "$destination.old"
    Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
    try {
        if (Test-Path -LiteralPath $destination -PathType Leaf) {
            Move-Item -LiteralPath $destination -Destination $backup
        }
        Move-Item -LiteralPath $staged -Destination $destination
        Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
    } catch {
        Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
        if ((Test-Path -LiteralPath $backup -PathType Leaf) -and -not (Test-Path -LiteralPath $destination)) {
            Move-Item -LiteralPath $backup -Destination $destination
        }
        throw "Could not replace $destination. Stop Codex and any active codexshim process, then retry. $($_.Exception.Message)"
    }

    Write-Host "Installed codexshim at $destination"
    Write-Host "Set command = '$destination' in your Codex MCP configuration."
} finally {
    Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
