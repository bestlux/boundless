[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Set-MsiSingleRow {
    param(
        [__ComObject]$Database,
        [string]$Sql,
        [int]$ColumnCount,
        [string]$Label,
        [string]$VariableName,
        [switch]$AllowMissing
    )

    $view = $Database.OpenView($Sql)
    try {
        $view.Execute()
        $record = $view.Fetch()
        if ($null -eq $record) {
            if ($AllowMissing) {
                Set-Variable -Name $VariableName -Value $null -Scope Script
                return
            }
            throw "Expected one $Label row, found 0."
        }

        $values = @()
        for ($index = 1; $index -le $ColumnCount; $index++) {
            $values += $record.StringData($index)
        }

        $extraRecord = $view.Fetch()
        if ($null -ne $extraRecord) {
            throw "Expected exactly one $Label row, found more than one."
        }

        Set-Variable -Name $VariableName -Value $values -Scope Script
    }
    finally {
        $view.Close()
    }
}

function Get-MsiColumnValues {
    param(
        [__ComObject]$Database,
        [string]$Sql,
        [int]$Column
    )

    $view = $Database.OpenView($Sql)
    try {
        $view.Execute()
        $values = @()
        while ($true) {
            $record = $view.Fetch()
            if ($null -eq $record) {
                break
            }
            $values += $record.StringData($Column)
        }
        return $values
    }
    finally {
        $view.Close()
    }
}

function Assert-Equals {
    param(
        [string]$Actual,
        [string]$Expected,
        [string]$Label
    )

    if ($Actual -ne $Expected) {
        throw "$Label was unexpected. Expected '$Expected', got '$Actual'."
    }
}

function Test-AllowedUserSidShape {
    param([string]$Sid)

    return $Sid -cmatch '^S-1-\d+(?:-\d+)+$'
}

function Assert-AllowedUserSidShapeExamples {
    if (-not (Test-AllowedUserSidShape -Sid "S-1-5-21-1-2-3-1001")) {
        throw "SID shape validator rejected a valid user SID example."
    }
    foreach ($sid in @(
        "",
        " S-1-5-21-1",
        "S-1-5-21-1 ",
        "S-1-not-a-sid",
        "S-1-5--21",
        "S-1-5-21-",
        "S-1-5-21-abc",
        "S-1-5-21-1);(A;;GA;;;WD",
        "S-2-5-21-1"
    )) {
        if (Test-AllowedUserSidShape -Sid $sid) {
            throw "SID shape validator accepted malformed example: $sid"
        }
    }
}

function Assert-LaunchConditionContains {
    param(
        [string[]]$Conditions,
        [string]$Needle
    )

    $match = $Conditions | Where-Object { $_ -like "*$Needle*" } | Select-Object -First 1
    if ($null -eq $match) {
        throw "LaunchCondition table did not contain expected SID validation fragment: $Needle"
    }
}

$InstallerPath = (Resolve-Path -LiteralPath $InstallerPath).Path
$installer = New-Object -ComObject WindowsInstaller.Installer
$database = $installer.OpenDatabase($InstallerPath, 0)

Set-MsiSingleRow -Database $database -Sql "SELECT Property, Value FROM Property WHERE Property = 'SecureCustomProperties'" -ColumnCount 2 -Label "SecureCustomProperties" -VariableName "secureRow"
$secureCustomProperties = [string]@($secureRow)[1]
if (($secureCustomProperties -split ';') -notcontains "BOUNDLESS_ALLOWED_USER_SID") {
    throw "SecureCustomProperties does not include BOUNDLESS_ALLOWED_USER_SID. Value=$secureCustomProperties"
}

Assert-AllowedUserSidShapeExamples
$launchConditions = Get-MsiColumnValues -Database $database -Sql "SELECT Condition FROM LaunchCondition" -Column 1
Assert-LaunchConditionContains -Conditions $launchConditions -Needle 'BOUNDLESS_ALLOWED_USER_SID << "S-1-"'
Assert-LaunchConditionContains -Conditions $launchConditions -Needle 'BOUNDLESS_ALLOWED_USER_SID >< " "'
Assert-LaunchConditionContains -Conditions $launchConditions -Needle 'BOUNDLESS_ALLOWED_USER_SID >< "--"'
Assert-LaunchConditionContains -Conditions $launchConditions -Needle 'BOUNDLESS_ALLOWED_USER_SID >> "-"'
Assert-LaunchConditionContains -Conditions $launchConditions -Needle 'BOUNDLESS_ALLOWED_USER_SID >< "n"'
Assert-LaunchConditionContains -Conditions $launchConditions -Needle 'BOUNDLESS_ALLOWED_USER_SID >< "-S"'

Set-MsiSingleRow -Database $database -Sql "SELECT Name, DisplayName, ServiceType, StartType, ErrorControl, StartName, Arguments, Component_, Description FROM ServiceInstall WHERE ServiceInstall = 'BoundlessServiceInstall'" -ColumnCount 9 -Label "ServiceInstall" -VariableName "serviceInstall"
Assert-Equals -Actual $serviceInstall[0] -Expected "BoundlessService" -Label "ServiceInstall.Name"
Assert-Equals -Actual $serviceInstall[1] -Expected "Boundless Service" -Label "ServiceInstall.DisplayName"
Assert-Equals -Actual $serviceInstall[2] -Expected "16" -Label "ServiceInstall.ServiceType"
Assert-Equals -Actual $serviceInstall[3] -Expected "2" -Label "ServiceInstall.StartType"
Assert-Equals -Actual $serviceInstall[4] -Expected "1" -Label "ServiceInstall.ErrorControl"
if ($serviceInstall[5] -notin @("", "LocalSystem")) {
    throw "ServiceInstall.StartName should be empty or LocalSystem for LocalSystem account, got '$($serviceInstall[5])'."
}
Assert-Equals -Actual $serviceInstall[6] -Expected "--allowed-user-sid=[BOUNDLESS_ALLOWED_USER_SID]" -Label "ServiceInstall.Arguments"
Assert-Equals -Actual $serviceInstall[7] -Expected "BoundlessServicePayloadComponent" -Label "ServiceInstall.Component_"
Assert-Equals -Actual $serviceInstall[8] -Expected "Boundless service-mode daemon host." -Label "ServiceInstall.Description"

Set-MsiSingleRow -Database $database -Sql "SELECT Name, Event, Wait, Component_ FROM ServiceControl WHERE ServiceControl = 'BoundlessServiceControl'" -ColumnCount 4 -Label "ServiceControl" -VariableName "serviceControl"
Assert-Equals -Actual $serviceControl[0] -Expected "BoundlessService" -Label "ServiceControl.Name"
if ((([int]$serviceControl[1]) -band 1) -eq 0 -or (([int]$serviceControl[1]) -band 2) -eq 0 -or (([int]$serviceControl[1]) -band 32) -eq 0 -or (([int]$serviceControl[1]) -band 128) -eq 0) {
    throw "ServiceControl.Event does not include start-on-install, stop-on-install, stop-on-uninstall, and remove-on-uninstall bits: $($serviceControl[1])"
}
Assert-Equals -Actual $serviceControl[2] -Expected "1" -Label "ServiceControl.Wait"
Assert-Equals -Actual $serviceControl[3] -Expected "BoundlessServicePayloadComponent" -Label "ServiceControl.Component_"

Set-MsiSingleRow -Database $database -Sql "SELECT Directory_, KeyPath FROM Component WHERE Component = 'BoundlessServicePayloadComponent'" -ColumnCount 2 -Label "service Component" -VariableName "component"
Assert-Equals -Actual $component[0] -Expected "INSTALLDIR" -Label "Component.Directory_"
Assert-Equals -Actual $component[1] -Expected "ServiceBinaryFile" -Label "Component.KeyPath"

Set-MsiSingleRow -Database $database -Sql "SELECT FileName, Component_ FROM File WHERE File = 'ServiceBinaryFile'" -ColumnCount 2 -Label "service File" -VariableName "file"
if ($file[0] -notmatch '(^|[|])boundless-service\.exe$') {
    throw "ServiceBinaryFile does not install boundless-service.exe: $($file[0])"
}
Assert-Equals -Actual $file[1] -Expected "BoundlessServicePayloadComponent" -Label "File.Component_"

Set-MsiSingleRow -Database $database -Sql "SELECT FileName, Component_ FROM File WHERE File = 'InstallHelperScriptFile'" -ColumnCount 2 -Label "install helper File" -VariableName "installHelperFile"
if ($installHelperFile[0] -notmatch '(^|[|])Boundless-Install\.ps1$') {
    throw "InstallHelperScriptFile does not install Boundless-Install.ps1: $($installHelperFile[0])"
}
Assert-Equals -Actual $installHelperFile[1] -Expected "BoundlessPayloadComponent" -Label "InstallHelperScriptFile.Component_"

Set-MsiSingleRow -Database $database -Sql "SELECT Directory_Parent, DefaultDir FROM Directory WHERE Directory = 'INSTALLDIR'" -ColumnCount 2 -Label "INSTALLDIR Directory" -VariableName "installDir"
Assert-Equals -Actual $installDir[0] -Expected "ProgramFiles64Folder" -Label "INSTALLDIR.Directory_Parent"
if ($installDir[1] -notmatch '(^|[|])Boundless$') {
    throw "INSTALLDIR.DefaultDir was unexpected. Expected long name Boundless, got '$($installDir[1])'."
}

$summary = [ordered]@{
    installer_path = $InstallerPath
    secure_custom_properties = $secureCustomProperties
    service_name = $serviceInstall[0]
    service_arguments = $serviceInstall[6]
    service_start_type = "auto"
    service_component = $serviceInstall[7]
    service_control_event = [int]$serviceControl[1]
    service_binary_file = $file[0]
    install_helper_file = $installHelperFile[0]
    install_directory_parent = $installDir[0]
    invalid_sid_examples_rejected = $true
    sid_launch_condition_count = @($launchConditions | Where-Object { $_ -like "*BOUNDLESS_ALLOWED_USER_SID*" }).Count
    status = "passed"
}

$summary | ConvertTo-Json -Depth 4
