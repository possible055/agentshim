[CmdletBinding()]
param(
    [string]$ReleaseDirectory,
    [string]$BinaryPath,
    [Parameter(Mandatory)]
    [string]$ExpectedVersion
)

$ErrorActionPreference = "Stop"
$testDirectory = Join-Path ([IO.Path]::GetTempPath()) ("codexshim-installer-test-" + [guid]::NewGuid().ToString("N"))
$fixtureDirectory = $null

try {
    $target = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
        X64 { "x86_64-pc-windows-msvc"; break }
        default { throw "Unsupported Windows architecture: $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)" }
    }
    if (-not $ReleaseDirectory) {
        if (-not $BinaryPath) {
            throw "ReleaseDirectory or BinaryPath is required."
        }
        $fixtureDirectory = Join-Path ([IO.Path]::GetTempPath()) ("codexshim-installer-fixture-" + [guid]::NewGuid().ToString("N"))
        $stage = Join-Path $fixtureDirectory "codexshim-$ExpectedVersion-$target"
        New-Item -ItemType Directory -Path $stage -Force | Out-Null
        Copy-Item -LiteralPath $BinaryPath -Destination (Join-Path $stage "codexshim.exe")
        $archive = Join-Path $fixtureDirectory "codexshim-$ExpectedVersion-$target.zip"
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [IO.Compression.ZipFile]::CreateFromDirectory($stage, $archive)
        $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $(Split-Path -Leaf $archive)" | Out-File -FilePath "$archive.sha256" -Encoding ascii
        $ReleaseDirectory = $fixtureDirectory
    }

    $binary = Join-Path $testDirectory "codexshim.exe"
    $expectedBinaryVersion = "codexshim $ExpectedVersion"
    $expectedPath = [IO.Path]::GetFullPath($binary).Replace('/', '\')

    for ($attempt = 1; $attempt -le 2; $attempt++) {
        $installerOutput = (& "$PSScriptRoot\install.ps1" -ReleaseDirectory $ReleaseDirectory -InstallDir $testDirectory 6>&1 | Out-String).Trim()
        if (-not $installerOutput.Contains("Installed codexshim at $expectedPath")) {
            throw "Installer did not report the expected Windows path on attempt $attempt."
        }
        if (-not $installerOutput.Contains("Installed version: $expectedBinaryVersion")) {
            throw "Installer did not report the expected version on attempt $attempt."
        }
        if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
            throw "Installer did not create $binary"
        }
        if (Test-Path -LiteralPath "$binary.old") {
            throw "Installer left a replacement backup at $binary.old"
        }
        $versionOutput = (& $binary --version | Out-String).Trim()
        if ($LASTEXITCODE -ne 0 -or $versionOutput -cne $expectedBinaryVersion) {
            throw "Installed executable reported '$versionOutput'; expected '$expectedBinaryVersion'."
        }
    }
} finally {
    Remove-Item -LiteralPath $testDirectory -Recurse -Force -ErrorAction SilentlyContinue
    if ($fixtureDirectory) {
        Remove-Item -LiteralPath $fixtureDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
}
