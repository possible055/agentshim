[CmdletBinding()]
param(
    [string]$Version,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "codexshim\bin"),
    [string]$ReleaseDirectory
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2.0

# Injected by the release workflow from the release tag; empty in source.
$DefaultVersion = "" # @codexshim:default-version

function Get-ReplacementFailureMessage {
    param(
        [Parameter(Mandatory)]
        [string]$Destination,
        [Parameter(Mandatory)]
        [System.Management.Automation.ErrorRecord]$ErrorRecord
    )

    $exception = $ErrorRecord.Exception
    $errorCode = $null
    while ($null -ne $exception) {
        $candidate = $exception.HResult -band 0xFFFF
        if ($candidate -in 5, 32, 33, 183) {
            $errorCode = $candidate
            break
        }
        $exception = $exception.InnerException
    }
    $detail = $ErrorRecord.Exception.Message
    $reason = switch ($errorCode) {
        5 { "Access was denied while replacing the destination." }
        32 { "The destination is in use by another process. Stop Codex and any active codexshim process, then retry." }
        33 { "The destination is locked by another process. Stop Codex and any active codexshim process, then retry." }
        183 { "A file already exists at the replacement path." }
        default { "The replacement operation failed." }
    }

    if ($errorCode -in 5, 32, 33, 183) {
        return "Could not replace $Destination. $reason Windows error $($errorCode): $detail"
    }
    return "Could not replace $Destination. $reason $detail"
}

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "codexshim requires 64-bit Windows."
}
if (-not $InstallDir) {
    throw "InstallDir is required."
}

[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$target = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    X64 { "x86_64-pc-windows-msvc"; break }
    default { throw "Unsupported Windows architecture: $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)" }
}
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
        $resolvedVersion = $Version
        if (-not $resolvedVersion) { $resolvedVersion = $DefaultVersion }
        if ($resolvedVersion) {
            $tag = if ($resolvedVersion.StartsWith("v")) { $resolvedVersion } else { "v$resolvedVersion" }
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
    $installPath = (Resolve-Path -LiteralPath $InstallDir).Path
    $destination = [IO.Path]::GetFullPath((Join-Path $installPath "codexshim.exe"))
    $staged = [IO.Path]::GetFullPath((Join-Path $installPath (".codexshim-" + [guid]::NewGuid().ToString("N") + ".exe")))
    Copy-Item -LiteralPath $binary[0].FullName -Destination $staged
    $versionOutput = (& $staged --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "The downloaded codexshim executable failed verification."
    }
    if (-not $versionOutput) {
        throw "The downloaded codexshim executable did not report a version."
    }

    $backup = "$destination.old"
    Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
    $replacementCompleted = $false
    try {
        if (Test-Path -LiteralPath $destination -PathType Leaf) {
            [IO.File]::Replace($staged, $destination, $backup, $true)
        } else {
            Move-Item -LiteralPath $staged -Destination $destination
        }
        $replacementCompleted = $true
        Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
    } catch {
        Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
        if (-not $replacementCompleted -and (Test-Path -LiteralPath $backup -PathType Leaf) -and -not (Test-Path -LiteralPath $destination)) {
            Move-Item -LiteralPath $backup -Destination $destination
        }
        throw (Get-ReplacementFailureMessage -Destination $destination -ErrorRecord $_)
    }

    $displayDestination = $destination.Replace('/', '\')
    Write-Host "Installed codexshim at $displayDestination"
    Write-Host "Installed version: $versionOutput"
    Write-Host "Set command = '$displayDestination' in your Codex MCP configuration."
} finally {
    Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
