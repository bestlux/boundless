[CmdletBinding()]
param([string]$RepoRoot = '')

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($RepoRoot)) { $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path }
$root = Join-Path ([IO.Path]::GetTempPath()) ('BoundlessMsiArguments-' + [guid]::NewGuid().ToString('N'))
$savedExit = $env:BOUNDLESS_MSI_ARGUMENTS_FIXTURE_EXIT
try {
    New-Item -ItemType Directory -Path $root | Out-Null
    $recorder = Join-Path $root 'argv recorder.exe'
    $recorded = Join-Path $root 'arguments.txt'
    $source = Join-Path $root 'Recorder.cs'
    [IO.File]::WriteAllText($source, @'
using System;
using System.Text;
class Recorder {
    static int Main(string[] args) {
        foreach (string arg in args) Console.WriteLine(Convert.ToBase64String(Encoding.UTF8.GetBytes(arg)));
        return int.Parse(Environment.GetEnvironmentVariable("BOUNDLESS_MSI_ARGUMENTS_FIXTURE_EXIT"));
    }
}
'@)
    $compiler = Join-Path $env:WINDIR 'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
    & $compiler /nologo /target:exe "/out:$recorder" $source
    if ($LASTEXITCODE -ne 0) { throw 'Could not compile the harmless native argument recorder.' }

    $tokens = $null
    $parseErrors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $RepoRoot 'scripts/dev/installer-smoke.ps1'), [ref]$tokens, [ref]$parseErrors)
    if ($parseErrors.Count -ne 0) { throw 'installer-smoke.ps1 has syntax errors.' }
    foreach ($name in @('ConvertTo-MsiProcessArgument', 'Invoke-MsiExec')) {
        $definition = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $true)
        if ($null -ne $definition) { . ([scriptblock]::Create($definition.Extent.Text)) }
    }

    # Replace only the executable. The real Start-Process still serializes the
    # production wrapper's ArgumentList, and a native process parses the result.
    function Start-Process {
        param([string]$FilePath, [string[]]$ArgumentList, [switch]$Wait, [switch]$PassThru, [string]$WindowStyle)
        if ($FilePath -ne 'msiexec.exe' -or -not $Wait -or -not $PassThru -or $WindowStyle -ne 'Hidden') {
            throw 'Unexpected MSI process invocation.'
        }
        Microsoft.PowerShell.Management\Start-Process -FilePath $recorder -ArgumentList $ArgumentList -Wait -PassThru -WindowStyle Hidden -RedirectStandardOutput $recorded
    }

    $msi = 'C:\qualification bundle & files\Boundless.msi'
    $log = 'C:\qualification output & logs\repair.log'
    $cases = @(
        @{ Name = 'install'; Arguments = @('/i', $msi, '/qn', '/norestart', 'BOUNDLESS_ALLOWED_USER_SID=S-1-5-21-1-2-3-1001'); Log = $log },
        @{ Name = 'repair'; Arguments = @('/i', $msi, '/qn', '/norestart', 'REINSTALL=ALL', 'REINSTALLMODE=vomus', 'BOUNDLESS_ALLOWED_USER_SID=S-1-5-21-1-2-3-1001'); Log = $log },
        @{ Name = 'uninstall'; Arguments = @('/x', $msi, '/qn', '/norestart'); Log = $log },
        @{ Name = 'no-log'; Arguments = @('/i', 'C:\plain\Boundless.msi', '/qn'); Log = '' },
        @{ Name = 'quoting-edges'; Arguments = @('', 'a"b', 'C:\trailing space\', 'C:\embedded\"quote', 'PROPERTY=two words'); Log = '' }
    )
    foreach ($case in $cases) {
        $env:BOUNDLESS_MSI_ARGUMENTS_FIXTURE_EXIT = '0'
        $exitCode = Invoke-MsiExec -ArgumentList $case.Arguments -LogPath $case.Log
        if ($exitCode -ne 0) { throw "Unexpected exit for $($case.Name)." }
        $expected = @($case.Arguments)
        if ($case.Log) { $expected += @('/l*v', $case.Log) }
        $actual = @([IO.File]::ReadAllLines($recorded) | ForEach-Object { [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($_)) })
        if ($actual.Count -ne $expected.Count) {
            throw "$($case.Name): expected $($expected.Count) native arguments, got $($actual.Count): $($actual -join ' | ')"
        }
        for ($index = 0; $index -lt $expected.Count; $index++) {
            if ($actual[$index] -cne $expected[$index]) { throw "$($case.Name): native argument $index changed." }
        }
    }
    $env:BOUNDLESS_MSI_ARGUMENTS_FIXTURE_EXIT = '3010'
    if ((Invoke-MsiExec -ArgumentList @('/x', $msi)) -ne 3010) { throw 'Reboot-required success was lost.' }
    $env:BOUNDLESS_MSI_ARGUMENTS_FIXTURE_EXIT = '1603'
    $failure = $null
    try { Invoke-MsiExec -ArgumentList @('/x', $msi) | Out-Null } catch { $failure = $_ }
    if ($null -eq $failure -or $failure.Exception.Message -notmatch 'failed with exit code 1603') { throw 'MSI failure was not propagated.' }
    Write-Host 'installer_msi_native_arguments_fixtures=passed'
}
finally {
    $env:BOUNDLESS_MSI_ARGUMENTS_FIXTURE_EXIT = $savedExit
    $resolvedRoot = [IO.Path]::GetFullPath($root)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    if (-not $resolvedRoot.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($resolvedRoot) -notlike 'BoundlessMsiArguments-*') { throw 'Refusing fixture cleanup outside its temporary directory.' }
    if (Test-Path -LiteralPath $resolvedRoot) { Remove-Item -LiteralPath $resolvedRoot -Recurse -Force }
}
