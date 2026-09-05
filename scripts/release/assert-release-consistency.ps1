[CmdletBinding()]
param(
    [string]$RepoRoot = "",
    [string]$Tag = "",
    [string[]]$AssetPaths = @(),
    [switch]$CheckWindowsInstallerVersion
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-WorkspaceVersion {
    param([string]$CargoTomlPath)

    $cargoToml = Get-Content -LiteralPath $CargoTomlPath -Raw
    $match = [regex]::Match($cargoToml, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"(?<version>[^"]+)"')
    if (-not $match.Success) {
        throw "Could not find [workspace.package].version in $CargoTomlPath"
    }

    return $match.Groups["version"].Value
}

function Get-ChangelogVersion {
    param([string]$ChangeLogPath)

    $match = Select-String -LiteralPath $ChangeLogPath -Pattern '^## \[(?<version>[^\]]+)\]' | Select-Object -First 1
    if ($null -eq $match) {
        throw "Could not find a top changelog heading in $ChangeLogPath"
    }

    return $match.Matches[0].Groups["version"].Value
}

function Get-MsiProductVersion {
    param([string]$InstallerPath)

    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $installer.GetType().InvokeMember(
        "OpenDatabase",
        [System.Reflection.BindingFlags]::InvokeMethod,
        $null,
        $installer,
        @($InstallerPath, 0)
    )
    $view = $database.GetType().InvokeMember(
        "OpenView",
        [System.Reflection.BindingFlags]::InvokeMethod,
        $null,
        $database,
        @("SELECT `Value` FROM `Property` WHERE `Property`='ProductVersion'")
    )
    $view.GetType().InvokeMember("Execute", [System.Reflection.BindingFlags]::InvokeMethod, $null, $view, $null) | Out-Null
    $record = $view.GetType().InvokeMember("Fetch", [System.Reflection.BindingFlags]::InvokeMethod, $null, $view, $null)
    if ($null -eq $record) {
        throw "ProductVersion was not found in MSI: $InstallerPath"
    }

    return $record.StringData(1)
}

function Assert-ReleasePleaseExtraFiles {
    param(
        [string]$RepoRoot,
        [string[]]$CrateCargoTomlPaths,
        [string[]]$CrateNames
    )

    $configPath = Join-Path $RepoRoot "release-please-config.json"
    $config = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
    $package = $config.packages."."
    if ($null -eq $package) {
        throw "release-please config does not contain package entry '.'."
    }

    $configuredPaths = @($package."extra-files" | ForEach-Object { $_.path })
    $expectedPaths = @(
        "Cargo.toml",
        "Cargo.lock",
        "packaging/windows/package-manifest.json"
    )
    $resolvedRepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path.TrimEnd('\', '/')
    foreach ($crateCargoTomlPath in $CrateCargoTomlPaths) {
        $resolvedPath = (Resolve-Path -LiteralPath $crateCargoTomlPath).Path
        if (-not $resolvedPath.StartsWith($resolvedRepoRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "Crate manifest path is outside repo root: $resolvedPath"
        }
        $relativePath = $resolvedPath.Substring($resolvedRepoRoot.Length).TrimStart('\', '/')
        $expectedPaths += ($relativePath -replace '\\', '/')
    }

    foreach ($expectedPath in $expectedPaths | Sort-Object -Unique) {
        $windowsPath = $expectedPath -replace '/', '\'
        $found = $configuredPaths | Where-Object { $_ -eq $expectedPath -or $_ -eq $windowsPath } | Select-Object -First 1
        if ($null -eq $found) {
            throw "release-please config is missing extra-files entry for '$expectedPath'."
        }
    }

    # GenericToml in the pinned release-please action wraps scalars in tagged
    # objects, so filters use name.value while the update target remains version.
    # Require the canonical selector to catch newly added crates and prevent
    # dependency versions (including same-name registry packages) from changing.
    $nameFilters = @($CrateNames | Sort-Object -Unique | ForEach-Object { "@.name.value == '$_'" })
    $expectedLockJsonPath = '$.package[?(!@.source && (' + ($nameFilters -join ' || ') + '))].version'
    $lockEntries = @($package."extra-files" | Where-Object { $_.path -eq "Cargo.lock" })
    if ($lockEntries.Count -ne 1 -or $lockEntries[0].type -ne "toml" -or $lockEntries[0].jsonpath -cne $expectedLockJsonPath) {
        throw "release-please Cargo.lock entry must use type 'toml' and select exactly the workspace package versions. Expected jsonpath: $expectedLockJsonPath"
    }
}

function Assert-WorkspaceLockVersions {
    param(
        [string]$CargoLockPath,
        [string[]]$CrateNames,
        [string]$WorkspaceVersion
    )

    $cargoLock = Get-Content -LiteralPath $CargoLockPath -Raw
    $lockVersions = @{}
    foreach ($package in [regex]::Matches($cargoLock, '(?ms)^\[\[package\]\]\s*(?<body>.*?)(?=^\[|\z)')) {
        $body = $package.Groups["body"].Value
        $nameMatch = [regex]::Match($body, '(?m)^name\s*=\s*"(?<name>[^"]+)"\s*$')
        $name = $nameMatch.Groups["name"].Value
        if ($CrateNames -cnotcontains $name -or $body -match '(?m)^source\s*=') {
            continue
        }
        if ($lockVersions.ContainsKey($name)) {
            throw "Cargo.lock contains multiple local package entries for '$name'."
        }
        $versionMatch = [regex]::Match($body, '(?m)^version\s*=\s*"(?<version>[^"]+)"\s*$')
        $lockVersions[$name] = $versionMatch.Groups["version"].Value
    }

    foreach ($name in $CrateNames) {
        if (-not $lockVersions.ContainsKey($name)) {
            throw "Cargo.lock is missing workspace package '$name'."
        }
        if ($lockVersions[$name] -ne $WorkspaceVersion) {
            throw "Cargo.lock package '$name' version '$($lockVersions[$name])' does not match workspace version '$WorkspaceVersion'."
        }
    }
}

$repoRoot = if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
}
else {
    (Resolve-Path -LiteralPath $RepoRoot).Path
}
$workspaceVersion = Get-WorkspaceVersion -CargoTomlPath (Join-Path $repoRoot "Cargo.toml")
$changelogVersion = Get-ChangelogVersion -ChangeLogPath (Join-Path $repoRoot "CHANGELOG.md")
$manifestVersion = (Get-Content -LiteralPath (Join-Path $repoRoot "packaging\windows\package-manifest.json") -Raw | ConvertFrom-Json).version
$releasePleaseVersion = (Get-Content -LiteralPath (Join-Path $repoRoot ".release-please-manifest.json") -Raw | ConvertFrom-Json)."." 

if ($workspaceVersion -ne $changelogVersion) {
    throw "Workspace version '$workspaceVersion' does not match top changelog version '$changelogVersion'."
}
if ($workspaceVersion -ne $manifestVersion) {
    throw "Workspace version '$workspaceVersion' does not match packaging manifest version '$manifestVersion'."
}
if ($workspaceVersion -ne $releasePleaseVersion) {
    throw "Workspace version '$workspaceVersion' does not match release-please manifest version '$releasePleaseVersion'."
}

$crateCargoTomls = Get-ChildItem -LiteralPath (Join-Path $repoRoot "crates") -Recurse -Filter Cargo.toml -File
$crateNames = @()
foreach ($crateCargoToml in $crateCargoTomls) {
    $crateManifest = Get-Content -LiteralPath $crateCargoToml.FullName -Raw
    $packageSection = [regex]::Match($crateManifest, '(?ms)^\[package\]\s*(?<body>.*?)(?=^\[|\z)').Groups["body"].Value
    $crateNameMatch = [regex]::Match($packageSection, '(?m)^name\s*=\s*"(?<name>[A-Za-z0-9_-]+)"\s*$')
    if (-not $crateNameMatch.Success) {
        throw "Crate manifest must declare a literal package name: $($crateCargoToml.FullName)"
    }
    $crateNames += $crateNameMatch.Groups["name"].Value
    $crateVersionMatch = [regex]::Match($packageSection, '(?m)^version\s*=\s*"(?<version>[^"]+)"\s*$')
    if (-not $crateVersionMatch.Success) {
        throw "Crate manifest must declare a literal package version: $($crateCargoToml.FullName)"
    }

    $crateVersion = $crateVersionMatch.Groups["version"].Value
    if ($crateVersion -ne $workspaceVersion) {
        throw "Crate manifest version '$crateVersion' does not match workspace version '$workspaceVersion': $($crateCargoToml.FullName)"
    }
}
Assert-WorkspaceLockVersions -CargoLockPath (Join-Path $repoRoot "Cargo.lock") -CrateNames $crateNames -WorkspaceVersion $workspaceVersion
Assert-ReleasePleaseExtraFiles -RepoRoot $repoRoot -CrateCargoTomlPaths @($crateCargoTomls.FullName) -CrateNames $crateNames

if (-not [string]::IsNullOrWhiteSpace($Tag)) {
    $expectedTag = "v$workspaceVersion"
    if ($Tag -ne $expectedTag) {
        throw "Tag '$Tag' does not match workspace version '$workspaceVersion' (expected '$expectedTag')."
    }
}

foreach ($assetPath in $AssetPaths) {
    if ([string]::IsNullOrWhiteSpace($assetPath)) {
        continue
    }

    $assetName = [System.IO.Path]::GetFileName($assetPath)
    if ($assetName -eq "SHA256SUMS.txt") {
        continue
    }

    $expectedNames = @(
        "Boundless-$workspaceVersion-windows-x64.msi",
        "Boundless-$workspaceVersion-windows-x64.zip",
        "Boundless-$workspaceVersion-windows-x64-install.ps1",
        "boundless-$workspaceVersion-linux-x64.tar.gz"
    )

    if ($expectedNames -notcontains $assetName) {
        throw "Unexpected release asset name '$assetName'."
    }

    if (
        $CheckWindowsInstallerVersion -and
        $assetName -like "Boundless-*-windows-x64.msi" -and
        (((Get-Variable -Name IsWindows -ErrorAction SilentlyContinue) -and $IsWindows) -or $env:OS -eq "Windows_NT")
    ) {
        $msiVersion = Get-MsiProductVersion -InstallerPath $assetPath
        if ($msiVersion -ne $workspaceVersion) {
            throw "MSI ProductVersion '$msiVersion' does not match workspace version '$workspaceVersion'."
        }
    }
}

Write-Host "release_version=$workspaceVersion"
Write-Host "release_consistency=passed"
