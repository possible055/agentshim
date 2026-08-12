[CmdletBinding()]
param(
    [Parameter(Mandatory, Position = 0)]
    [string]$Version,
    [switch]$AllowDirty
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2.0

function ConvertTo-SemVer {
    param([Parameter(Mandatory)][string]$Value)

    $match = [regex]::Match(
        $Value,
        '^(?<major>0|[1-9][0-9]*)\.(?<minor>0|[1-9][0-9]*)\.(?<patch>0|[1-9][0-9]*)(?:-(?<prerelease>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$'
    )
    if (-not $match.Success) {
        throw "Version must be a valid SemVer value: $Value"
    }

    $preRelease = @()
    if ($match.Groups["prerelease"].Success) {
        $preRelease = @($match.Groups["prerelease"].Value -split '\.')
        foreach ($identifier in $preRelease) {
            if ($identifier -match '^0[0-9]+$') {
                throw "Numeric prerelease identifiers cannot contain leading zeroes: $Value"
            }
        }
    }

    [pscustomobject]@{
        Major = [long]$match.Groups["major"].Value
        Minor = [long]$match.Groups["minor"].Value
        Patch = [long]$match.Groups["patch"].Value
        PreRelease = $preRelease
    }
}

function Compare-SemVer {
    param(
        [Parameter(Mandatory)]$Left,
        [Parameter(Mandatory)]$Right
    )

    foreach ($part in @("Major", "Minor", "Patch")) {
        if ($Left.$part -lt $Right.$part) { return -1 }
        if ($Left.$part -gt $Right.$part) { return 1 }
    }

    $leftHasPreRelease = $Left.PreRelease.Count -gt 0
    $rightHasPreRelease = $Right.PreRelease.Count -gt 0
    if (-not $leftHasPreRelease -and -not $rightHasPreRelease) { return 0 }
    if (-not $leftHasPreRelease) { return 1 }
    if (-not $rightHasPreRelease) { return -1 }

    $count = [Math]::Min($Left.PreRelease.Count, $Right.PreRelease.Count)
    for ($index = 0; $index -lt $count; $index++) {
        $leftIdentifier = $Left.PreRelease[$index]
        $rightIdentifier = $Right.PreRelease[$index]
        $leftNumeric = $leftIdentifier -match '^[0-9]+$'
        $rightNumeric = $rightIdentifier -match '^[0-9]+$'
        if ($leftNumeric -and $rightNumeric) {
            $leftNumber = [long]$leftIdentifier
            $rightNumber = [long]$rightIdentifier
            if ($leftNumber -lt $rightNumber) { return -1 }
            if ($leftNumber -gt $rightNumber) { return 1 }
        } elseif ($leftNumeric -and -not $rightNumeric) {
            return -1
        } elseif (-not $leftNumeric -and $rightNumeric) {
            return 1
        } elseif ($leftIdentifier -cne $rightIdentifier) {
            return [string]::CompareOrdinal($leftIdentifier, $rightIdentifier)
        }
    }

    if ($Left.PreRelease.Count -lt $Right.PreRelease.Count) { return -1 }
    if ($Left.PreRelease.Count -gt $Right.PreRelease.Count) { return 1 }
    return 0
}

function Get-PackageVersion {
    param([Parameter(Mandatory)][string]$Content)

    $match = [regex]::Match($Content, '(?m)^version\s*=\s*"([^"]+)"\s*$')
    if (-not $match.Success) {
        throw "Cargo.toml does not contain a package version."
    }
    return $match.Groups[1].Value
}

function Set-PackageVersion {
    param(
        [Parameter(Mandatory)][string]$Content,
        [Parameter(Mandatory)][string]$NewVersion
    )

    $match = [regex]::Match($Content, '(?m)^version\s*=\s*"([^"]+)"\s*$')
    if (-not $match.Success) {
        throw "Cargo.toml does not contain a package version."
    }
    return $Content.Substring(0, $match.Groups[1].Index) +
        $NewVersion +
        $Content.Substring($match.Groups[1].Index + $match.Groups[1].Length)
}

function Normalize-LockDependencies {
    param([Parameter(Mandatory)][string]$Content)

    $normalized = [regex]::Replace($Content, '(?m)^(version|checksum)\s*=\s*"[^"]*"\r?\n', '')
    return $normalized.Replace("`r`n", "`n").Replace("`r", "`n")
}

$repository = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$manifestPath = Join-Path $repository "Cargo.toml"
$lockPath = Join-Path $repository "Cargo.lock"

if (-not $AllowDirty) {
    $status = @(& git -C $repository status --porcelain --untracked-files=all)
    if ($LASTEXITCODE -ne 0) {
        throw "Could not inspect the Git worktree."
    }
    if ($status.Count -gt 0) {
        throw "The Git worktree is dirty. Commit or stash changes first, or pass -AllowDirty explicitly."
    }
}

$manifestBefore = [IO.File]::ReadAllText($manifestPath)
$currentVersion = Get-PackageVersion -Content $manifestBefore
$currentSemVer = ConvertTo-SemVer -Value $currentVersion
$requestedSemVer = ConvertTo-SemVer -Value $Version
if ((Compare-SemVer -Left $requestedSemVer -Right $currentSemVer) -le 0) {
    throw "Version $Version must be greater than the current version $currentVersion."
}

$lockBefore = $null
if (Test-Path -LiteralPath $lockPath -PathType Leaf) {
    $lockBefore = [IO.File]::ReadAllText($lockPath)
}

$manifestAfter = Set-PackageVersion -Content $manifestBefore -NewVersion $Version
[IO.File]::WriteAllText($manifestPath, $manifestAfter, [Text.UTF8Encoding]::new($false))

Push-Location $repository
try {
    & cargo update --workspace
    if ($LASTEXITCODE -ne 0) {
        throw "cargo update --workspace failed."
    }
} finally {
    Pop-Location
}

if ($null -ne $lockBefore) {
    $lockAfter = [IO.File]::ReadAllText($lockPath)
    if ((Normalize-LockDependencies -Content $lockAfter) -cne (Normalize-LockDependencies -Content $lockBefore)) {
        throw "Cargo.lock changed beyond dependency versions and checksums. Inspect the working tree and resolve the unexpected lockfile changes."
    }
}

Push-Location $repository
try {
    $metadataJson = & cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata --locked failed."
    }
} finally {
    Pop-Location
}
$metadata = ($metadataJson -join "`n") | ConvertFrom-Json
$package = @($metadata.packages | Where-Object { $_.name -eq "codexshim" })
if ($package.Count -ne 1 -or $package[0].version -cne $Version) {
    throw "Cargo metadata reports an unexpected codexshim version. Expected $Version."
}

Write-Host "Prepared codexshim version $Version."
