[CmdletBinding()]
param(
    [string]$PackageRoot = "",
    [string]$InstallRoot = "",
    [string]$StartupFolderPath = "",
    [string]$StartMenuProgramsPath = "",
    [string]$DesktopFolderPath = "",
    [string]$UninstallRegistryKeyPath = "",
    [switch]$NoLaunch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-LocalAppDataPath {
    return [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
}

function Get-DefaultInstallRoot {
    return Join-Path (Join-Path (Get-LocalAppDataPath) "Programs") "Boundless"
}

function Get-DefaultStartupFolderPath {
    return [Environment]::GetFolderPath([Environment+SpecialFolder]::Startup)
}

function Get-DefaultStartMenuProgramsPath {
    return [Environment]::GetFolderPath([Environment+SpecialFolder]::Programs)
}

function Get-DefaultDesktopFolderPath {
    return [Environment]::GetFolderPath([Environment+SpecialFolder]::DesktopDirectory)
}

function Get-DefaultUninstallRegistryKeyPath {
    return "Registry::HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Uninstall\Boundless"
}

function Resolve-RequiredPackagePath {
    param(
        [string]$Root,
        [string]$Name
    )

    $path = Join-Path $Root $Name
    if (-not (Test-Path -LiteralPath $path)) {
        throw "Required package asset is missing: $path"
    }

    return (Resolve-Path -LiteralPath $path).Path
}

function Ensure-Directory {
    param([string]$Path)

    New-Item -ItemType Directory -Force -Path $Path | Out-Null
}

function New-Shortcut {
    param(
        [string]$ShortcutPath,
        [string]$TargetPath,
        [string]$WorkingDirectory,
        [string]$Description,
        [string]$IconLocation
    )

    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($ShortcutPath)
    $shortcut.TargetPath = $TargetPath
    $shortcut.WorkingDirectory = $WorkingDirectory
    if (-not [string]::IsNullOrWhiteSpace($IconLocation)) {
        $shortcut.IconLocation = $IconLocation
    } else {
        $shortcut.IconLocation = $TargetPath
    }
    $shortcut.Description = $Description
    $shortcut.Save()
}

if ([string]::IsNullOrWhiteSpace($InstallRoot)) {
    $InstallRoot = Get-DefaultInstallRoot
}
if ([string]::IsNullOrWhiteSpace($PackageRoot)) {
    $PackageRoot = $PSScriptRoot
}
if ([string]::IsNullOrWhiteSpace($StartupFolderPath)) {
    $StartupFolderPath = Get-DefaultStartupFolderPath
}
if ([string]::IsNullOrWhiteSpace($StartMenuProgramsPath)) {
    $StartMenuProgramsPath = Get-DefaultStartMenuProgramsPath
}
if ([string]::IsNullOrWhiteSpace($DesktopFolderPath)) {
    $DesktopFolderPath = Get-DefaultDesktopFolderPath
}
if ([string]::IsNullOrWhiteSpace($UninstallRegistryKeyPath)) {
    $UninstallRegistryKeyPath = Get-DefaultUninstallRegistryKeyPath
}

$PackageRoot = (Resolve-Path -LiteralPath $PackageRoot).Path
$manifestPath = Resolve-RequiredPackagePath -Root $PackageRoot -Name "package-manifest.json"
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json

$payloadFiles = @(
    "boundlessd.exe"
    "boundlessctl.exe"
    "boundlesstray.exe"
    "Boundless.ico"
    "Boundless-Install.ps1"
    "Boundless-Uninstall.ps1"
    "Boundless-Reset.ps1"
    "README.txt"
    "LICENSE.txt"
    "CHANGELOG.md"
    "package-manifest.json"
)

Ensure-Directory -Path $InstallRoot
foreach ($file in $payloadFiles) {
    $source = Resolve-RequiredPackagePath -Root $PackageRoot -Name $file
    Copy-Item -LiteralPath $source -Destination (Join-Path $InstallRoot $file) -Force
}

Ensure-Directory -Path $StartupFolderPath
$trayPath = Join-Path $InstallRoot "boundlesstray.exe"
$iconPath = Join-Path $InstallRoot "Boundless.ico"
$startupShortcutPath = Join-Path $StartupFolderPath "Boundless.lnk"
New-Shortcut `
    -ShortcutPath $startupShortcutPath `
    -TargetPath $trayPath `
    -WorkingDirectory $InstallRoot `
    -Description "Launch Boundless tray at sign-in" `
    -IconLocation $iconPath

Ensure-Directory -Path $StartMenuProgramsPath
$startMenuShortcutPath = Join-Path $StartMenuProgramsPath "Boundless.lnk"
New-Shortcut `
    -ShortcutPath $startMenuShortcutPath `
    -TargetPath $trayPath `
    -WorkingDirectory $InstallRoot `
    -Description "Launch Boundless" `
    -IconLocation $iconPath

Ensure-Directory -Path $DesktopFolderPath
$desktopShortcutPath = Join-Path $DesktopFolderPath "Boundless.lnk"
New-Shortcut `
    -ShortcutPath $desktopShortcutPath `
    -TargetPath $trayPath `
    -WorkingDirectory $InstallRoot `
    -Description "Launch Boundless" `
    -IconLocation $iconPath

$uninstallCommand = 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File "{0}"' -f (Join-Path $InstallRoot "Boundless-Uninstall.ps1")
New-Item -Path $UninstallRegistryKeyPath -Force | Out-Null
Set-ItemProperty -Path $UninstallRegistryKeyPath -Name DisplayName -Value "Boundless"
Set-ItemProperty -Path $UninstallRegistryKeyPath -Name DisplayVersion -Value $manifest.version
Set-ItemProperty -Path $UninstallRegistryKeyPath -Name Publisher -Value $manifest.publisher
Set-ItemProperty -Path $UninstallRegistryKeyPath -Name InstallLocation -Value $InstallRoot
Set-ItemProperty -Path $UninstallRegistryKeyPath -Name DisplayIcon -Value $iconPath
Set-ItemProperty -Path $UninstallRegistryKeyPath -Name UninstallString -Value $uninstallCommand
Set-ItemProperty -Path $UninstallRegistryKeyPath -Name QuietUninstallString -Value $uninstallCommand
Set-ItemProperty -Path $UninstallRegistryKeyPath -Name NoModify -Value 1 -Type DWord
Set-ItemProperty -Path $UninstallRegistryKeyPath -Name NoRepair -Value 1 -Type DWord

if (-not $NoLaunch) {
    Start-Process -FilePath $trayPath -WorkingDirectory $InstallRoot | Out-Null
}

Write-Host "install_root=$InstallRoot"
Write-Host "startup_shortcut=$startupShortcutPath"
Write-Host "start_menu_shortcut=$startMenuShortcutPath"
Write-Host "desktop_shortcut=$desktopShortcutPath"
Write-Host "uninstall_key=$UninstallRegistryKeyPath"
