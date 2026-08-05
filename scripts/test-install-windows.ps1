[CmdletBinding()]
param(
    [string]$ReleaseDirectory,
    [string]$BinaryPath
)

$ErrorActionPreference = "Stop"
$testDirectory = Join-Path ([IO.Path]::GetTempPath()) ("codexshim-installer-test-" + [guid]::NewGuid().ToString("N"))
$fixtureDirectory = $null

try {
    if (-not $ReleaseDirectory) {
        if (-not $BinaryPath) {
            throw "ReleaseDirectory or BinaryPath is required."
        }
        $fixtureDirectory = Join-Path ([IO.Path]::GetTempPath()) ("codexshim-installer-fixture-" + [guid]::NewGuid().ToString("N"))
        $stage = Join-Path $fixtureDirectory "codexshim-0.0.0-x86_64-pc-windows-msvc"
        New-Item -ItemType Directory -Path $stage -Force | Out-Null
        Copy-Item -LiteralPath $BinaryPath -Destination (Join-Path $stage "codexshim.exe")
        $archive = Join-Path $fixtureDirectory "codexshim-0.0.0-x86_64-pc-windows-msvc.zip"
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [IO.Compression.ZipFile]::CreateFromDirectory($stage, $archive)
        $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $(Split-Path -Leaf $archive)" | Out-File -FilePath "$archive.sha256" -Encoding ascii
        $ReleaseDirectory = $fixtureDirectory
    }

    & "$PSScriptRoot\install.ps1" -ReleaseDirectory $ReleaseDirectory -InstallDir $testDirectory
    & "$PSScriptRoot\install.ps1" -ReleaseDirectory $ReleaseDirectory -InstallDir $testDirectory
    $binary = Join-Path $testDirectory "codexshim.exe"
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "Installer did not create $binary"
    }
    & $binary --version | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Installed executable failed its version check."
    }
} finally {
    Remove-Item -LiteralPath $testDirectory -Recurse -Force -ErrorAction SilentlyContinue
    if ($fixtureDirectory) {
        Remove-Item -LiteralPath $fixtureDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
}
