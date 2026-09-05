[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ReportPath,
    [string]$OutputPath = '',
    [ValidateRange(1, 100)][int]$MinimumSamples = 20,
    [string]$ExpectedDaemonSha256 = '',
    [string]$ExpectedSourceRevision = '',
    [switch]$RequireRealPaired,
    [ValidateRange(1, 8760)][int]$MaxEvidenceAgeHours = 168
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'functional-evidence.ps1')
$inputFile = Get-Item -LiteralPath $ReportPath
if ($inputFile.Length -gt 1MB) { throw 'Paired-test report exceeds the 1 MiB input bound' }
$inputBytes = [IO.File]::ReadAllBytes($inputFile.FullName)
if ($inputBytes.Length -gt 1MB) { throw 'Paired-test report exceeds the 1 MiB input bound' }
# Keep the hash tied to the exact bytes parsed. BOM detection also accepts
# Windows PowerShell 5.1's UTF-16 stdout redirection.
$stream = New-Object IO.MemoryStream(,$inputBytes)
$reader = New-Object IO.StreamReader($stream, [Text.Encoding]::UTF8, $true)
try { $report = $reader.ReadToEnd() | ConvertFrom-Json }
finally { $reader.Dispose(); $stream.Dispose() }
$hasher = [Security.Cryptography.SHA256]::Create()
try { $inputHash = ([BitConverter]::ToString($hasher.ComputeHash($inputBytes))).Replace('-', '').ToLowerInvariant() }
finally { $hasher.Dispose() }
$parameters = @{
    Report = $report
    MinimumSamples = $MinimumSamples
    ExpectedDaemonSha256 = $ExpectedDaemonSha256
    ExpectedSourceRevision = $ExpectedSourceRevision
    RequireRealPaired = $RequireRealPaired
    MaxEvidenceAgeHours = $MaxEvidenceAgeHours
}
$validation = Assert-PairedTestReport @parameters
$validation | Add-Member -NotePropertyName report_sha256 -NotePropertyValue $inputHash
if ($OutputPath) {
    $fullOutputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
    if ($fullOutputPath -eq $inputFile.FullName) { throw 'Validation output must not replace its source report' }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $fullOutputPath) | Out-Null
    $validation | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $fullOutputPath -Encoding utf8
}
$validation | ConvertTo-Json -Depth 12
