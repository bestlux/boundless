[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string[]]$Files = @(),

    [string]$CertificatePath = $env:WINDOWS_SIGN_CERT_PATH,

    [string]$CertificateBase64 = $env:WINDOWS_SIGN_CERT_BASE64,

    [string]$CertificatePassword = $env:WINDOWS_SIGN_CERT_PASSWORD,

    [string]$TimestampUrl = $env:WINDOWS_SIGN_TIMESTAMP_URL,

    [string]$Description = $env:WINDOWS_SIGN_DESCRIPTION,

    [string]$DescriptionUrl = $env:WINDOWS_SIGN_DESCRIPTION_URL,

    [string]$DigestAlgorithm = $(if ($env:WINDOWS_SIGN_DIGEST_ALGORITHM) { $env:WINDOWS_SIGN_DIGEST_ALGORITHM } else { "SHA256" }),

    [string]$TimestampDigestAlgorithm = $(if ($env:WINDOWS_SIGN_TIMESTAMP_DIGEST_ALGORITHM) { $env:WINDOWS_SIGN_TIMESTAMP_DIGEST_ALGORITHM } else { "SHA256" })
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-SignToolPath {
    $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $kitsRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    if (Test-Path -LiteralPath $kitsRoot) {
        $candidate = Get-ChildItem -Path $kitsRoot -Filter signtool.exe -Recurse -File |
            Where-Object { $_.FullName -match "\\x64\\" } |
            Sort-Object FullName -Descending |
            Select-Object -First 1
        if ($candidate) {
            return $candidate.FullName
        }
    }

    throw "signtool.exe was not found. Install the Windows SDK or add signtool.exe to PATH."
}

function Resolve-FilesToSign {
    param(
        [string[]]$InputFiles
    )

    $resolvedFiles = @()
    foreach ($file in $InputFiles) {
        if ([string]::IsNullOrWhiteSpace($file)) {
            continue
        }

        if (-not (Test-Path -LiteralPath $file)) {
            throw "File to sign was not found: $file"
        }

        $resolvedFiles += (Resolve-Path -LiteralPath $file).Path
    }

    return $resolvedFiles
}

function New-CertificateFile {
    param(
        [string]$ConfiguredCertificatePath,
        [string]$ConfiguredCertificateBase64,
        [string]$WorkingDirectory
    )

    if (-not [string]::IsNullOrWhiteSpace($ConfiguredCertificatePath)) {
        if (-not (Test-Path -LiteralPath $ConfiguredCertificatePath)) {
            throw "Configured certificate path was not found: $ConfiguredCertificatePath"
        }

        return (Resolve-Path -LiteralPath $ConfiguredCertificatePath).Path
    }

    if ([string]::IsNullOrWhiteSpace($ConfiguredCertificateBase64)) {
        return $null
    }

    $certificateBytes = $null
    try {
        $certificateBytes = [Convert]::FromBase64String($ConfiguredCertificateBase64)
    }
    catch {
        throw "WINDOWS_SIGN_CERT_BASE64 is not valid base64."
    }

    $certificateFile = Join-Path $WorkingDirectory "codesign.pfx"
    [System.IO.File]::WriteAllBytes($certificateFile, $certificateBytes)
    return $certificateFile
}

$filesToSign = @(Resolve-FilesToSign -InputFiles $Files)
if ($filesToSign.Count -eq 0) {
    Write-Host "No Windows artifacts were provided for signing; skipping."
    return
}

$tempDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("boundless-sign-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tempDirectory | Out-Null

try {
    $certificateFile = New-CertificateFile `
        -ConfiguredCertificatePath $CertificatePath `
        -ConfiguredCertificateBase64 $CertificateBase64 `
        -WorkingDirectory $tempDirectory

    if ([string]::IsNullOrWhiteSpace($certificateFile)) {
        Write-Host "Windows code signing is not configured; skipping."
        return
    }

    $signToolPath = Get-SignToolPath

    foreach ($file in $filesToSign) {
        $arguments = @(
            "sign"
            "/fd"
            $DigestAlgorithm
            "/f"
            $certificateFile
        )

        if ($CertificatePassword -ne $null) {
            $arguments += @("/p", $CertificatePassword)
        }

        if (-not [string]::IsNullOrWhiteSpace($Description)) {
            $arguments += @("/d", $Description)
        }

        if (-not [string]::IsNullOrWhiteSpace($DescriptionUrl)) {
            $arguments += @("/du", $DescriptionUrl)
        }

        if (-not [string]::IsNullOrWhiteSpace($TimestampUrl)) {
            $arguments += @("/tr", $TimestampUrl, "/td", $TimestampDigestAlgorithm)
        }

        $arguments += $file

        Write-Host "Signing $file"
        & $signToolPath @arguments
        if ($LASTEXITCODE -ne 0) {
            throw "signtool.exe failed for $file with exit code $LASTEXITCODE"
        }
    }
}
finally {
    Remove-Item -LiteralPath $tempDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
