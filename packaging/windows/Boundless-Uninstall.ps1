[CmdletBinding()]
param(
    [string]$InstallRoot = "",
    [string]$StartupFolderPath = "",
    [string]$StartMenuProgramsPath = "",
    [string]$DesktopFolderPath = "",
    [string]$UninstallRegistryKeyPath = "",
    [switch]$RemoveState,
    [string]$ConfigPath = "",
    [string]$DataRoot = "",
    [string]$SecurityRoot = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-LocalAppDataPath {
    return [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
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

function Get-DefaultConfigPath {
    return Join-Path (Join-Path (Get-LocalAppDataPath) "Boundless") "config.json"
}

function Get-DefaultDataRoot {
    return Join-Path (Get-LocalAppDataPath) "Boundless"
}

function Get-DefaultSecurityRoot {
    return Join-Path (Join-Path (Get-LocalAppDataPath) "Boundless") "security"
}

function Remove-IfExists {
    param([string]$Path)

    if (Test-Path -LiteralPath $Path) {
        Remove-Item -LiteralPath $Path -Recurse -Force
    }
}

function Schedule-InstallRootRemoval {
    param([string]$TargetPath)

    if (-not (Test-Path -LiteralPath $TargetPath)) {
        return
    }

    $cleanupScript = Join-Path ([System.IO.Path]::GetTempPath()) ("boundless-uninstall-" + [guid]::NewGuid().ToString("N") + ".cmd")
    $commandText = "@echo off`r`n" +
        "timeout /t 2 /nobreak >nul`r`n" +
        "rmdir /s /q `"$TargetPath`"`r`n" +
        "del /f /q `"%~f0`"`r`n"
    Set-Content -LiteralPath $cleanupScript -Value $commandText -Encoding ascii
    Start-Process -FilePath "cmd.exe" -ArgumentList "/c", $cleanupScript -WindowStyle Hidden | Out-Null
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
if ([string]::IsNullOrWhiteSpace($InstallRoot)) {
    $InstallRoot = $PSScriptRoot
}
if ([string]::IsNullOrWhiteSpace($UninstallRegistryKeyPath)) {
    $UninstallRegistryKeyPath = Get-DefaultUninstallRegistryKeyPath
}
if ([string]::IsNullOrWhiteSpace($ConfigPath)) {
    $ConfigPath = Get-DefaultConfigPath
}
if ([string]::IsNullOrWhiteSpace($DataRoot)) {
    $DataRoot = Get-DefaultDataRoot
}
if ([string]::IsNullOrWhiteSpace($SecurityRoot)) {
    $SecurityRoot = Get-DefaultSecurityRoot
}

Get-Process -Name "boundlesstray", "boundlessd" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

$shortcutPaths = @(
    (Join-Path $StartupFolderPath "Boundless.lnk"),
    (Join-Path $StartMenuProgramsPath "Boundless.lnk"),
    (Join-Path $DesktopFolderPath "Boundless.lnk")
)

foreach ($shortcutPath in $shortcutPaths) {
    if (Test-Path -LiteralPath $shortcutPath) {
        Remove-Item -LiteralPath $shortcutPath -Force
    }
}

if (Test-Path -LiteralPath $UninstallRegistryKeyPath) {
    Remove-Item -LiteralPath $UninstallRegistryKeyPath -Recurse -Force
}

if ($RemoveState) {
    if (Test-Path -LiteralPath $ConfigPath) {
        Remove-Item -LiteralPath $ConfigPath -Force
    }
    Remove-IfExists -Path $SecurityRoot
    Remove-IfExists -Path $DataRoot
}

$InstallRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallRoot)
Set-Location -LiteralPath ([System.IO.Path]::GetTempPath())
Schedule-InstallRootRemoval -TargetPath $InstallRoot

Write-Host "shortcuts_removed=$($shortcutPaths -join ';')"
Write-Host "uninstall_key_removed=$UninstallRegistryKeyPath"
Write-Host "install_root_removal_scheduled=$InstallRoot"
