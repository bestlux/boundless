[CmdletBinding()]
param(
    [string]$InstallerPath = "",
    [string]$AllowedUserSid = "",
    [string]$AllowedUserName = "",
    [switch]$UseCurrentUserWhenElevated,
    [switch]$Quiet,
    [switch]$NoRestart,
    [string]$LogPath = "",
    [switch]$ResolveOnly,
    [switch]$SelfTest,
    [Parameter(DontShow = $true)]
    [switch]$ElevatedInstall,
    [Parameter(DontShow = $true)]
    [switch]$ElevatedBootstrapServiceRecovery,
    [Parameter(DontShow = $true)]
    [switch]$ElevatedBootstrapMsiIdleProof,
    [Parameter(DontShow = $true)]
    [switch]$ElevatedBootstrapMsiIdleServiceRecovery,
    [Parameter(DontShow = $true)]
    [string]$ElevatedBootstrapRecoveryJob = "",
    [Parameter(DontShow = $true)]
    [string]$ElevatedBootstrapRecoveryRevocationEvent = "",
    [Parameter(DontShow = $true)]
    [string]$ElevatedBootstrapRecoveryActionFence = "",
    [Parameter(DontShow = $true)]
    [string]$ElevatedBootstrapRecoveryActionCommittedEvent = "",
    [Parameter(DontShow = $true)]
    [string]$ExpectedInstallerSha256 = "",
    [Parameter(DontShow = $true)]
    [string]$ElevatedInstallCancelEvent = "",
    [Parameter(DontShow = $true)]
    [int]$ElevatedInstallCoordinatorProcessId = 0,
    [Parameter(DontShow = $true)]
    [long]$ElevatedInstallCoordinatorStartTicks = 0,
    [Parameter(DontShow = $true)]
    [string]$ElevatedInstallMonitorMutex = "",
    [Parameter(DontShow = $true)]
    [string]$ElevatedInstallStartGate = "",
    [Parameter(DontShow = $true)]
    [string]$ElevatedInstallResultPath = "",
    [Parameter(DontShow = $true)]
    [string]$ElevatedInstallServiceInitialRunningEvent = "",
    [Parameter(DontShow = $true)]
    [string]$ElevatedInstallMsiMayHaveStartedEvent = "",
    [Parameter(DontShow = $true)]
    [string]$ElevatedInstallMsiDefinitiveCompletionEvent = "",
    [Parameter(DontShow = $true)]
    [string]$ElevatedInstallMsiIdleProvenEvent = "",
    [Parameter(DontShow = $true)]
    [int]$ElevatedInstallTimeoutSeconds = 900
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# Anchor the exact helper PowerShell parsed before any install preflight or UAC
# handoff. Comparing the loaded AST text to disk closes the small load/startup
# replacement window; the byte hash and metadata are rechecked again immediately
# before elevation and the elevated copier accepts only this startup hash.
if ([string]::IsNullOrWhiteSpace($PSCommandPath)) {
    throw "Boundless install helper must run from a script file."
}
$startupHelperPath = (Resolve-Path -LiteralPath $PSCommandPath -ErrorAction Stop).Path
$startupLoadedText = $MyInvocation.MyCommand.ScriptBlock.Ast.Extent.Text
$startupDiskText = [IO.File]::ReadAllText($startupHelperPath)
if (-not [string]::Equals($startupLoadedText, $startupDiskText, [StringComparison]::Ordinal)) {
    throw "Boundless install helper changed between PowerShell load and startup."
}
$startupHelperItem = Get-Item -LiteralPath $startupHelperPath -Force -ErrorAction Stop
$script:BoundlessHelperStartupAnchor = [pscustomobject]@{
    path = $startupHelperPath
    sha256 = (Get-FileHash -LiteralPath $startupHelperPath -Algorithm SHA256).Hash
    length = [int64]$startupHelperItem.Length
    last_write_utc_ticks = [int64]$startupHelperItem.LastWriteTimeUtc.Ticks
}

function Assert-BoundlessHelperStartupAnchor {
    if ($null -eq $script:BoundlessHelperStartupAnchor) {
        throw "Boundless helper startup identity anchor was unavailable."
    }
    $anchor = $script:BoundlessHelperStartupAnchor
    $currentPath = (Resolve-Path -LiteralPath $PSCommandPath -ErrorAction Stop).Path
    if (-not $currentPath.Equals($anchor.path, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Boundless helper path changed after startup."
    }
    $currentItem = Get-Item -LiteralPath $currentPath -Force -ErrorAction Stop
    $currentHash = (Get-FileHash -LiteralPath $currentPath -Algorithm SHA256).Hash
    if (
        [int64]$currentItem.Length -ne [int64]$anchor.length -or
        [int64]$currentItem.LastWriteTimeUtc.Ticks -ne [int64]$anchor.last_write_utc_ticks -or
        -not $currentHash.Equals($anchor.sha256, [StringComparison]::OrdinalIgnoreCase)
    ) {
        throw "Boundless install helper changed after its startup identity was anchored."
    }
    return $anchor
}

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Assert-AllowedUserSid {
    param([string]$Sid)

    if ([string]::IsNullOrWhiteSpace($Sid)) {
        throw "Allowed user SID was empty."
    }

    if ($Sid -notmatch '^S-1-\d+(?:-\d+)+$') {
        throw "Allowed user SID must be a strict numeric SID such as S-1-5-21-... Got: $Sid"
    }
}

function Resolve-AccountSid {
    param([string]$AccountName)

    if ([string]::IsNullOrWhiteSpace($AccountName)) {
        throw "Allowed user name was empty."
    }

    try {
        $account = [Security.Principal.NTAccount]::new($AccountName)
        return $account.Translate([Security.Principal.SecurityIdentifier]).Value
    }
    catch {
        throw "Could not resolve Windows account '$AccountName' to a SID. Use DOMAIN\user format or pass -AllowedUserSid explicitly. $($_.Exception.Message)"
    }
}

function Resolve-AccountNameFromSid {
    param([string]$Sid)

    try {
        $securityIdentifier = [Security.Principal.SecurityIdentifier]::new($Sid)
        return $securityIdentifier.Translate([Security.Principal.NTAccount]).Value
    }
    catch {
        return ""
    }
}

function Resolve-AllowedUser {
    if (-not [string]::IsNullOrWhiteSpace($AllowedUserSid) -and -not [string]::IsNullOrWhiteSpace($AllowedUserName)) {
        throw "Pass either -AllowedUserSid or -AllowedUserName, not both."
    }

    if (-not [string]::IsNullOrWhiteSpace($AllowedUserSid)) {
        Assert-AllowedUserSid -Sid $AllowedUserSid
        return [pscustomobject]@{
            sid = $AllowedUserSid
            account = Resolve-AccountNameFromSid -Sid $AllowedUserSid
            source = "explicit_sid"
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($AllowedUserName)) {
        $sid = Resolve-AccountSid -AccountName $AllowedUserName
        Assert-AllowedUserSid -Sid $sid
        return [pscustomobject]@{
            sid = $sid
            account = $AllowedUserName
            source = "explicit_account"
        }
    }

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $isElevated = Test-IsAdministrator
    if ($isElevated -and -not $UseCurrentUserWhenElevated) {
        throw "Refusing to infer the allowed user from an already-elevated shell. Run this helper from the intended desktop user's normal PowerShell so it can capture that SID before UAC, or pass -AllowedUserSid for the intended user. Use -UseCurrentUserWhenElevated only when the elevated account is intentionally the desktop user to authorize."
    }

    $source = if ($isElevated) {
        "current_elevated_user_explicitly_allowed"
    }
    else {
        "current_unelevated_user"
    }

    return [pscustomobject]@{
        sid = $identity.User.Value
        account = $identity.Name
        source = $source
    }
}

function Resolve-InstallerPath {
    if (-not [string]::IsNullOrWhiteSpace($InstallerPath)) {
        if (-not (Test-Path -LiteralPath $InstallerPath)) {
            throw "InstallerPath was not found: $InstallerPath"
        }

        return (Resolve-Path -LiteralPath $InstallerPath).Path
    }

    $scriptRoot = if ([string]::IsNullOrWhiteSpace($PSScriptRoot)) {
        (Resolve-Path ".").Path
    }
    else {
        $PSScriptRoot
    }

    $candidates = @(Get-ChildItem -LiteralPath $scriptRoot -Filter "Boundless-*-windows-x64.msi" -File -ErrorAction SilentlyContinue)
    if ($candidates.Count -eq 0) {
        throw "No Boundless Windows MSI was found next to this helper. Pass -InstallerPath <path-to-msi>."
    }
    if ($candidates.Count -gt 1) {
        $names = @($candidates | Select-Object -ExpandProperty Name) -join ", "
        throw "Multiple Boundless Windows MSI files were found next to this helper. Pass -InstallerPath explicitly. Found: $names"
    }

    return $candidates[0].FullName
}

function ConvertTo-ProcessArgument {
    param([string]$Value)

    if ($Value -notmatch '[\s"]') {
        return $Value
    }

    return '"' + ($Value -replace '"', '\"') + '"'
}

function ConvertTo-BoundlessCompressedEncodedCommand {
    param([string]$Source)

    $sourceBytes = [Text.Encoding]::UTF8.GetBytes($Source)
    $buffer = [IO.MemoryStream]::new()
    try {
        $gzip = [IO.Compression.GZipStream]::new(
            $buffer,
            [IO.Compression.CompressionMode]::Compress,
            $true
        )
        try { $gzip.Write($sourceBytes, 0, $sourceBytes.Length) }
        finally { $gzip.Dispose() }
        $compressed = [Convert]::ToBase64String($buffer.ToArray())
    }
    finally {
        $buffer.Dispose()
    }
    $launcher = @'
$b=[Convert]::FromBase64String("__COMPRESSED_SOURCE__")
$i=[IO.MemoryStream]::new($b)
$g=[IO.Compression.GZipStream]::new($i,[IO.Compression.CompressionMode]::Decompress)
$r=[IO.StreamReader]::new($g,[Text.Encoding]::UTF8)
try{& ([scriptblock]::Create($r.ReadToEnd()))}
finally{$r.Dispose();$g.Dispose();$i.Dispose()}
'@.Replace("__COMPRESSED_SOURCE__", $compressed)
    return [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($launcher))
}

function Initialize-BoundlessProcessTreeNativeMethods {
    if ($null -ne ("BoundlessOwnedProcessBoundary" -as [type])) {
        return
    }

    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public sealed class BoundlessOwnedProcessBoundary : IDisposable
{
    private IntPtr jobHandle;
    private IntPtr processHandle;
    private readonly int processId;
    private bool disposed;

    internal BoundlessOwnedProcessBoundary(IntPtr jobHandle, IntPtr processHandle, int processId)
    {
        this.jobHandle = jobHandle;
        this.processHandle = processHandle;
        this.processId = processId;
    }

    public int Id { get { return processId; } }

    public bool HasExited
    {
        get
        {
            ThrowIfDisposed();
            return BoundlessProcessTreeNativeMethods.WaitForSingleObject(processHandle, 0) == 0;
        }
    }

    public int ExitCode
    {
        get
        {
            ThrowIfDisposed();
            uint exitCode;
            if (!BoundlessProcessTreeNativeMethods.GetExitCodeProcess(processHandle, out exitCode))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "GetExitCodeProcess failed");
            }
            return unchecked((int)exitCode);
        }
    }

    public int ActiveProcessCount
    {
        get
        {
            ThrowIfDisposed();
            return BoundlessProcessTreeNativeMethods.GetActiveProcessCount(jobHandle);
        }
    }

    public bool WaitForExit(int timeoutMilliseconds)
    {
        ThrowIfDisposed();
        uint timeout = timeoutMilliseconds < 0 ? 0xFFFFFFFFu : unchecked((uint)timeoutMilliseconds);
        uint result = BoundlessProcessTreeNativeMethods.WaitForSingleObject(processHandle, timeout);
        if (result == 0) { return true; }
        if (result == 258) { return false; }
        throw new Win32Exception(Marshal.GetLastWin32Error(), "WaitForSingleObject(process) failed");
    }

    public bool WaitForTreeExit(int timeoutMilliseconds)
    {
        ThrowIfDisposed();
        Stopwatch stopwatch = Stopwatch.StartNew();
        do
        {
            if (ActiveProcessCount == 0) { return true; }
            Thread.Sleep(20);
        }
        while (timeoutMilliseconds < 0 || stopwatch.ElapsedMilliseconds < timeoutMilliseconds);
        return ActiveProcessCount == 0;
    }

    public void Terminate(int exitCode)
    {
        ThrowIfDisposed();
        if (ActiveProcessCount == 0) { return; }
        if (!BoundlessProcessTreeNativeMethods.TerminateJobObject(jobHandle, unchecked((uint)exitCode)))
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "TerminateJobObject failed");
        }
    }

    public void Dispose()
    {
        if (disposed) { return; }
        disposed = true;
        if (jobHandle != IntPtr.Zero)
        {
            BoundlessProcessTreeNativeMethods.CloseHandle(jobHandle);
            jobHandle = IntPtr.Zero;
        }
        if (processHandle != IntPtr.Zero)
        {
            BoundlessProcessTreeNativeMethods.CloseHandle(processHandle);
            processHandle = IntPtr.Zero;
        }
        GC.SuppressFinalize(this);
    }

    ~BoundlessOwnedProcessBoundary()
    {
        Dispose();
    }

    private void ThrowIfDisposed()
    {
        if (disposed) { throw new ObjectDisposedException("BoundlessOwnedProcessBoundary"); }
    }
}

public static class BoundlessProcessTreeNativeMethods
{
    private const uint CREATE_SUSPENDED = 0x00000004;
    private const uint CREATE_NO_WINDOW = 0x08000000;
    private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
    private const int JobObjectBasicAccountingInformation = 1;
    private const int JobObjectExtendedLimitInformation = 9;
    private const uint JOB_OBJECT_QUERY = 0x0004;
    private const int ERROR_FILE_NOT_FOUND = 2;
    private const int ERROR_ALREADY_EXISTS = 183;

    [StructLayout(LayoutKind.Sequential)]
    private struct SECURITY_ATTRIBUTES
    {
        public int nLength;
        public IntPtr lpSecurityDescriptor;
        [MarshalAs(UnmanagedType.Bool)] public bool bInheritHandle;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct STARTUPINFO
    {
        public int cb;
        public IntPtr lpReserved;
        public IntPtr lpDesktop;
        public IntPtr lpTitle;
        public int dwX;
        public int dwY;
        public int dwXSize;
        public int dwYSize;
        public int dwXCountChars;
        public int dwYCountChars;
        public int dwFillAttribute;
        public int dwFlags;
        public short wShowWindow;
        public short cbReserved2;
        public IntPtr lpReserved2;
        public IntPtr hStdInput;
        public IntPtr hStdOutput;
        public IntPtr hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct PROCESS_INFORMATION
    {
        public IntPtr hProcess;
        public IntPtr hThread;
        public int dwProcessId;
        public int dwThreadId;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_BASIC_LIMIT_INFORMATION
    {
        public long PerProcessUserTimeLimit;
        public long PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize;
        public UIntPtr MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass;
        public uint SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct IO_COUNTERS
    {
        public ulong ReadOperationCount;
        public ulong WriteOperationCount;
        public ulong OtherOperationCount;
        public ulong ReadTransferCount;
        public ulong WriteTransferCount;
        public ulong OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
    {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public UIntPtr ProcessMemoryLimit;
        public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed;
        public UIntPtr PeakJobMemoryUsed;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_BASIC_ACCOUNTING_INFORMATION
    {
        public long TotalUserTime;
        public long TotalKernelTime;
        public long ThisPeriodTotalUserTime;
        public long ThisPeriodTotalKernelTime;
        public uint TotalPageFaultCount;
        public uint TotalProcesses;
        public uint ActiveProcesses;
        public uint TotalTerminatedProcesses;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr CreateJobObjectW(ref SECURITY_ATTRIBUTES attributes, string name);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr OpenJobObjectW(uint desiredAccess, bool inheritHandle, string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetInformationJobObject(
        IntPtr job,
        int informationClass,
        ref JOBOBJECT_EXTENDED_LIMIT_INFORMATION information,
        uint length);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool QueryInformationJobObject(
        IntPtr job,
        int informationClass,
        out JOBOBJECT_BASIC_ACCOUNTING_INFORMATION information,
        uint length,
        IntPtr returnLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool CreateProcessW(
        string applicationName,
        StringBuilder commandLine,
        IntPtr processAttributes,
        IntPtr threadAttributes,
        bool inheritHandles,
        uint creationFlags,
        IntPtr environment,
        string currentDirectory,
        ref STARTUPINFO startupInfo,
        out PROCESS_INFORMATION processInformation);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint ResumeThread(IntPtr thread);

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern bool TerminateJobObject(IntPtr job, uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateProcess(IntPtr process, uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern bool CloseHandle(IntPtr handle);

    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string descriptor,
        uint revision,
        out IntPtr securityDescriptor,
        IntPtr size);

    [DllImport("kernel32.dll")]
    private static extern IntPtr LocalFree(IntPtr memory);

    public static BoundlessOwnedProcessBoundary Start(
        string applicationName,
        string commandLine,
        string currentDirectory,
        bool createNoWindow,
        string jobName,
        string jobSddl)
    {
        IntPtr securityDescriptor = IntPtr.Zero;
        IntPtr job = IntPtr.Zero;
        PROCESS_INFORMATION process = new PROCESS_INFORMATION();
        try
        {
            SECURITY_ATTRIBUTES attributes = new SECURITY_ATTRIBUTES();
            attributes.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
            if (!String.IsNullOrEmpty(jobSddl))
            {
                if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    jobSddl,
                    1,
                    out securityDescriptor,
                    IntPtr.Zero))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "job SDDL conversion failed");
                }
                attributes.lpSecurityDescriptor = securityDescriptor;
            }
            job = CreateJobObjectW(ref attributes, String.IsNullOrEmpty(jobName) ? null : jobName);
            if (job == IntPtr.Zero)
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateJobObject failed");
            }
            if (!String.IsNullOrEmpty(jobName) && Marshal.GetLastWin32Error() == ERROR_ALREADY_EXISTS)
            {
                throw new InvalidOperationException("owned process job unexpectedly already existed");
            }

            JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if (!SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                ref limits,
                unchecked((uint)Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION)))))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "SetInformationJobObject failed");
            }

            STARTUPINFO startup = new STARTUPINFO();
            startup.cb = Marshal.SizeOf(typeof(STARTUPINFO));
            uint flags = CREATE_SUSPENDED | (createNoWindow ? CREATE_NO_WINDOW : 0u);
            if (!CreateProcessW(
                applicationName,
                new StringBuilder(commandLine),
                IntPtr.Zero,
                IntPtr.Zero,
                false,
                flags,
                IntPtr.Zero,
                String.IsNullOrEmpty(currentDirectory) ? null : currentDirectory,
                ref startup,
                out process))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateProcess failed");
            }
            if (!AssignProcessToJobObject(job, process.hProcess))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "AssignProcessToJobObject failed");
            }
            if (ResumeThread(process.hThread) == 0xFFFFFFFFu)
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "ResumeThread failed");
            }
            CloseHandle(process.hThread);
            process.hThread = IntPtr.Zero;
            BoundlessOwnedProcessBoundary result = new BoundlessOwnedProcessBoundary(
                job,
                process.hProcess,
                process.dwProcessId);
            job = IntPtr.Zero;
            process.hProcess = IntPtr.Zero;
            return result;
        }
        catch (Exception originalError)
        {
            if (job != IntPtr.Zero) { TerminateJobObject(job, 1); }
            Exception cleanupError = null;
            if (process.hProcess != IntPtr.Zero)
            {
                uint wait = WaitForSingleObject(process.hProcess, 0);
                if (wait == 258)
                {
                    if (!TerminateProcess(process.hProcess, 1))
                    {
                        cleanupError = new Win32Exception(
                            Marshal.GetLastWin32Error(),
                            "TerminateProcess(unassigned child) failed");
                    }
                    else if (WaitForSingleObject(process.hProcess, 5000) != 0)
                    {
                        cleanupError = new InvalidOperationException(
                            "Unassigned suspended child did not terminate within 5000 ms.");
                    }
                }
                else if (wait != 0)
                {
                    cleanupError = new Win32Exception(
                        Marshal.GetLastWin32Error(),
                        "WaitForSingleObject(unassigned child) failed");
                }
            }
            if (process.hThread != IntPtr.Zero) { CloseHandle(process.hThread); }
            if (process.hProcess != IntPtr.Zero) { CloseHandle(process.hProcess); }
            if (job != IntPtr.Zero) { CloseHandle(job); }
            if (cleanupError != null)
            {
                throw new InvalidOperationException(
                    "Owned process admission failed and child cleanup could not be proven.",
                    new AggregateException(originalError, cleanupError));
            }
            throw;
        }
        finally
        {
            if (securityDescriptor != IntPtr.Zero) { LocalFree(securityDescriptor); }
        }
    }

    internal static int GetActiveProcessCount(IntPtr job)
    {
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION accounting;
        if (!QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            out accounting,
            unchecked((uint)Marshal.SizeOf(typeof(JOBOBJECT_BASIC_ACCOUNTING_INFORMATION))),
            IntPtr.Zero))
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "QueryInformationJobObject failed");
        }
        return unchecked((int)accounting.ActiveProcesses);
    }

    public static int GetNamedJobActiveProcessCount(string name)
    {
        IntPtr job = OpenJobObjectW(JOB_OBJECT_QUERY, false, name);
        if (job == IntPtr.Zero)
        {
            int error = Marshal.GetLastWin32Error();
            if (error == ERROR_FILE_NOT_FOUND) { return -1; }
            throw new Win32Exception(error, "OpenJobObject failed");
        }
        try { return GetActiveProcessCount(job); }
        finally { CloseHandle(job); }
    }
}
'@
}

function Initialize-BoundlessRecoveryAuthorityNativeMethods {
    if ($null -ne ("BoundlessRecoveryAuthorityNativeMethodsV1" -as [type])) {
        return
    }

    # Keep recovery authority interop versioned and independent from the
    # process-tree native type. PowerShell hosts can retain an older Add-Type
    # definition across helper invocations during an upgrade.
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Threading;

public sealed class BoundlessRecoveryAuthorityJobV1 : IDisposable
{
    private IntPtr jobHandle;
    private bool disposed;

    internal BoundlessRecoveryAuthorityJobV1(IntPtr jobHandle)
    {
        this.jobHandle = jobHandle;
    }

    public int ActiveProcessCount
    {
        get
        {
            ThrowIfDisposed();
            return BoundlessRecoveryAuthorityNativeMethodsV1.GetActiveProcessCount(jobHandle);
        }
    }

    public void Terminate(int exitCode)
    {
        ThrowIfDisposed();
        if (ActiveProcessCount == 0) { return; }
        if (!BoundlessRecoveryAuthorityNativeMethodsV1.TerminateJobObject(
            jobHandle,
            unchecked((uint)exitCode)))
        {
            throw new Win32Exception(
                Marshal.GetLastWin32Error(),
                "TerminateJobObject(recovery authority) failed");
        }
    }

    public bool WaitForEmpty(int timeoutMilliseconds)
    {
        ThrowIfDisposed();
        Stopwatch stopwatch = Stopwatch.StartNew();
        do
        {
            if (ActiveProcessCount == 0) { return true; }
            Thread.Sleep(20);
        }
        while (timeoutMilliseconds < 0 || stopwatch.ElapsedMilliseconds < timeoutMilliseconds);
        return ActiveProcessCount == 0;
    }

    public void Dispose()
    {
        if (disposed) { return; }
        disposed = true;
        if (jobHandle != IntPtr.Zero)
        {
            BoundlessRecoveryAuthorityNativeMethodsV1.CloseHandle(jobHandle);
            jobHandle = IntPtr.Zero;
        }
        GC.SuppressFinalize(this);
    }

    ~BoundlessRecoveryAuthorityJobV1() { Dispose(); }

    private void ThrowIfDisposed()
    {
        if (disposed)
            throw new ObjectDisposedException("BoundlessRecoveryAuthorityJobV1");
    }
}

public static class BoundlessRecoveryAuthorityNativeMethodsV1
{
    private const uint JOB_OBJECT_ASSIGN_PROCESS = 0x0001;
    private const uint JOB_OBJECT_QUERY = 0x0004;
    private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
    private const int JobObjectBasicAccountingInformation = 1;
    private const int JobObjectExtendedLimitInformation = 9;
    private const int ERROR_ALREADY_EXISTS = 183;

    [StructLayout(LayoutKind.Sequential)]
    private struct SECURITY_ATTRIBUTES
    {
        public int nLength;
        public IntPtr lpSecurityDescriptor;
        [MarshalAs(UnmanagedType.Bool)] public bool bInheritHandle;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_BASIC_LIMIT_INFORMATION
    {
        public long PerProcessUserTimeLimit;
        public long PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize;
        public UIntPtr MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public IntPtr Affinity;
        public uint PriorityClass;
        public uint SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct IO_COUNTERS
    {
        public ulong ReadOperationCount;
        public ulong WriteOperationCount;
        public ulong OtherOperationCount;
        public ulong ReadTransferCount;
        public ulong WriteTransferCount;
        public ulong OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
    {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public UIntPtr ProcessMemoryLimit;
        public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed;
        public UIntPtr PeakJobMemoryUsed;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_BASIC_ACCOUNTING_INFORMATION
    {
        public long TotalUserTime;
        public long TotalKernelTime;
        public long ThisPeriodTotalUserTime;
        public long ThisPeriodTotalKernelTime;
        public uint TotalPageFaultCount;
        public uint TotalProcesses;
        public uint ActiveProcesses;
        public uint TotalTerminatedProcesses;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr CreateJobObjectW(
        ref SECURITY_ATTRIBUTES attributes,
        string name);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr OpenJobObjectW(
        uint desiredAccess,
        bool inheritHandle,
        string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetInformationJobObject(
        IntPtr job,
        int informationClass,
        ref JOBOBJECT_EXTENDED_LIMIT_INFORMATION information,
        uint length);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool QueryInformationJobObject(
        IntPtr job,
        int informationClass,
        out JOBOBJECT_BASIC_ACCOUNTING_INFORMATION information,
        uint length,
        IntPtr returnLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

    [DllImport("kernel32.dll")]
    private static extern IntPtr GetCurrentProcess();

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern bool TerminateJobObject(IntPtr job, uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern bool CloseHandle(IntPtr handle);

    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string descriptor,
        uint revision,
        out IntPtr securityDescriptor,
        IntPtr size);

    [DllImport("kernel32.dll")]
    private static extern IntPtr LocalFree(IntPtr memory);

    public static BoundlessRecoveryAuthorityJobV1 Create(string name, string jobSddl)
    {
        IntPtr securityDescriptor = IntPtr.Zero;
        IntPtr job = IntPtr.Zero;
        try
        {
            SECURITY_ATTRIBUTES attributes = new SECURITY_ATTRIBUTES();
            attributes.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
            if (!String.IsNullOrEmpty(jobSddl))
            {
                if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    jobSddl,
                    1,
                    out securityDescriptor,
                    IntPtr.Zero))
                {
                    throw new Win32Exception(
                        Marshal.GetLastWin32Error(),
                        "recovery authority job SDDL conversion failed");
                }
                attributes.lpSecurityDescriptor = securityDescriptor;
            }
            job = CreateJobObjectW(ref attributes, name);
            if (job == IntPtr.Zero)
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "CreateJobObject(recovery authority) failed");
            if (Marshal.GetLastWin32Error() == ERROR_ALREADY_EXISTS)
                throw new InvalidOperationException(
                    "Recovery authority job unexpectedly already existed.");

            JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits =
                new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if (!SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                ref limits,
                unchecked((uint)Marshal.SizeOf(
                    typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION)))))
            {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "SetInformationJobObject(recovery authority) failed");
            }
            BoundlessRecoveryAuthorityJobV1 result =
                new BoundlessRecoveryAuthorityJobV1(job);
            job = IntPtr.Zero;
            return result;
        }
        finally
        {
            if (job != IntPtr.Zero) { CloseHandle(job); }
            if (securityDescriptor != IntPtr.Zero) { LocalFree(securityDescriptor); }
        }
    }

    public static void Join(string name)
    {
        IntPtr job = OpenJobObjectW(
            JOB_OBJECT_ASSIGN_PROCESS | JOB_OBJECT_QUERY,
            false,
            name);
        if (job == IntPtr.Zero)
            throw new Win32Exception(
                Marshal.GetLastWin32Error(),
                "OpenJobObject(recovery authority) failed");
        try
        {
            if (!AssignProcessToJobObject(job, GetCurrentProcess()))
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "AssignProcessToJobObject(recovery authority) failed");
        }
        finally { CloseHandle(job); }
    }

    internal static int GetActiveProcessCount(IntPtr job)
    {
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION accounting;
        if (!QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            out accounting,
            unchecked((uint)Marshal.SizeOf(
                typeof(JOBOBJECT_BASIC_ACCOUNTING_INFORMATION))),
            IntPtr.Zero))
        {
            throw new Win32Exception(
                Marshal.GetLastWin32Error(),
                "QueryInformationJobObject(recovery authority) failed");
        }
        return unchecked((int)accounting.ActiveProcesses);
    }
}
'@
}

function Resolve-BoundlessProcessExecutable {
    param([string]$FilePath)

    if ([IO.Path]::IsPathRooted($FilePath)) {
        return (Resolve-Path -LiteralPath $FilePath -ErrorAction Stop).Path
    }
    $command = Get-Command $FilePath -CommandType Application -ErrorAction Stop |
        Select-Object -First 1
    return $command.Source
}

function Start-BoundlessOwnedProcessBoundary {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList = @(),
        [switch]$CreateNoWindow,
        [string]$JobName = "",
        [string]$JobSddl = ""
    )

    Initialize-BoundlessProcessTreeNativeMethods
    $resolvedFile = Resolve-BoundlessProcessExecutable -FilePath $FilePath
    $commandLine = @(
        ConvertTo-ProcessArgument -Value $resolvedFile
        @($ArgumentList | ForEach-Object { ConvertTo-ProcessArgument -Value $_ })
    ) -join " "
    return [BoundlessProcessTreeNativeMethods]::Start(
        $resolvedFile,
        $commandLine,
        (Get-Location).Path,
        $CreateNoWindow.IsPresent,
        $JobName,
        $JobSddl
    )
}

function New-BoundlessMsiArguments {
    param(
        [string]$ResolvedInstallerPath,
        [string]$Sid
    )

    $arguments = @(
        "/i",
        $ResolvedInstallerPath,
        "BOUNDLESS_ALLOWED_USER_SID=$Sid"
    )

    if ($Quiet) {
        $arguments += "/qn"
    }
    if ($NoRestart) {
        $arguments += "/norestart"
    }
    if (-not [string]::IsNullOrWhiteSpace($LogPath)) {
        $resolvedLogPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($LogPath)
        $logParent = Split-Path -Parent $resolvedLogPath
        if (-not [string]::IsNullOrWhiteSpace($logParent)) {
            New-Item -ItemType Directory -Force -Path $logParent | Out-Null
        }
        $arguments += @("/l*v", $resolvedLogPath)
    }

    return $arguments
}

function Open-BoundlessInstallerCancellationEvent {
    param([string]$Name)

    if ($Name -notmatch '^Local\\Boundless\.Installer\.Cancel\.v1\.[0-9a-f]{32}$') {
        throw "Installer cancellation event name was invalid."
    }
    try {
        return [Threading.EventWaitHandle]::OpenExisting($Name)
    }
    catch {
        throw "Installer cancellation event was unavailable. $($_.Exception.Message)"
    }
}

function Stop-BoundlessProcessBoundary {
    param(
        [object]$Process,
        [int]$TimeoutMilliseconds = 5000
    )

    if ($null -eq $Process) {
        return
    }
    if ($Process.GetType().FullName -eq "BoundlessOwnedProcessBoundary") {
        if ($Process.ActiveProcessCount -eq 0) {
            return
        }
        $Process.Terminate(1)
        if (-not $Process.WaitForTreeExit($TimeoutMilliseconds)) {
            throw "Owned installer process tree PID $($Process.Id) did not stop within $TimeoutMilliseconds ms; active=$($Process.ActiveProcessCount)."
        }
        if (-not $Process.WaitForExit($TimeoutMilliseconds)) {
            throw "Owned installer root PID $($Process.Id) was not signaled after its process tree emptied."
        }
        return
    }
    if ($Process.HasExited) {
        return
    }
    try {
        $Process.Kill()
    }
    catch {
        if (-not $Process.HasExited) {
            throw "Could not terminate installer process boundary PID $($Process.Id). $($_.Exception.Message)"
        }
    }
    if (-not $Process.WaitForExit($TimeoutMilliseconds)) {
        throw "Installer process boundary PID $($Process.Id) did not stop within $TimeoutMilliseconds ms."
    }
}

function Assert-BoundlessInputInjectorTargets {
    param(
        [object[]]$Processes,
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId,
        [string]$ExpectedPath
    )

    foreach ($process in @($Processes)) {
        if ($process.owner_sid -ne $ExpectedOwnerSid) {
            throw "Input injector PID $($process.id) belonged to unexpected SID $($process.owner_sid)."
        }
        if ([int]$process.session_id -ne $ExpectedSessionId) {
            throw "Input injector PID $($process.id) ran in unexpected session $($process.session_id); expected $ExpectedSessionId."
        }
        if (-not (Test-WindowsPathEqual -Left $process.path -Right $ExpectedPath)) {
            throw "Input injector PID $($process.id) did not run from the MSI-owned Program Files path."
        }
    }
    return @($Processes)
}

function Get-BoundlessInputInjectorTargets {
    param(
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId,
        [string]$ExpectedPath
    )

    $snapshot = @(
        Get-Process -Name "boundless-input-injector" -ErrorAction SilentlyContinue |
            ForEach-Object {
                $processPath = try { $_.Path } catch { "" }
                [pscustomobject]@{
                    id = $_.Id
                    session_id = $_.SessionId
                    owner_sid = Get-ProcessOwnerSid -ProcessId $_.Id
                    path = $processPath
                    process = $_
                }
            }
    )
    return @(
        Assert-BoundlessInputInjectorTargets `
            -Processes $snapshot `
            -ExpectedOwnerSid $ExpectedOwnerSid `
            -ExpectedSessionId $ExpectedSessionId `
            -ExpectedPath $ExpectedPath
    )
}

function Stop-BoundlessInputInjectorBeforeMsi {
    param(
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId,
        [int]$GracefulTimeoutMilliseconds = 3500
    )

    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    $expectedPath = Join-Path $env:ProgramFiles "Boundless\boundless-input-injector.exe"
    $targets = @(Get-BoundlessInputInjectorTargets `
        -ExpectedOwnerSid $ExpectedOwnerSid `
        -ExpectedSessionId $ExpectedSessionId `
        -ExpectedPath $expectedPath)
    if ($targets.Count -eq 0) {
        $finalTargets = @(Get-BoundlessInputInjectorTargets `
            -ExpectedOwnerSid $ExpectedOwnerSid `
            -ExpectedSessionId $ExpectedSessionId `
            -ExpectedPath $expectedPath)
        if ($finalTargets.Count -ne 0) {
            throw "Input injector appeared during the bounded shutdown preflight."
        }
        return [pscustomobject]@{
            initial_count = 0
            elapsed_milliseconds = 0
            force_kill_used = $false
        }
    }

    $deadline = (Get-Date).AddMilliseconds($GracefulTimeoutMilliseconds)
    do {
        $remaining = @(
            $targets |
                Where-Object { $null -ne (Get-Process -Id $_.id -ErrorAction SilentlyContinue) }
        )
        if ($remaining.Count -eq 0) {
            break
        }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)

    $forceKillUsed = $remaining.Count -gt 0
    foreach ($target in $remaining) {
        Stop-BoundlessProcessBoundary -Process $target.process -TimeoutMilliseconds 2000
    }
    $finalTargets = @(Get-BoundlessInputInjectorTargets `
        -ExpectedOwnerSid $ExpectedOwnerSid `
        -ExpectedSessionId $ExpectedSessionId `
        -ExpectedPath $expectedPath)
    if ($finalTargets.Count -ne 0) {
        throw "Input injector shutdown left $($finalTargets.Count) process(es) after the bounded stop."
    }
    $stopwatch.Stop()
    return [pscustomobject]@{
        initial_count = $targets.Count
        elapsed_milliseconds = $stopwatch.ElapsedMilliseconds
        force_kill_used = $forceKillUsed
    }
}

function Throw-BoundlessMsiFailure {
    param(
        [string]$Message,
        [ValidateSet("not_started", "definitive_failure", "uncertain")]
        [string]$CompletionState
    )

    $exception = [InvalidOperationException]::new($Message)
    $exception.Data["BoundlessMsiCompletionState"] = $CompletionState
    throw $exception
}

function Invoke-BoundlessMsiElevated {
    param(
        [string]$ResolvedInstallerPath,
        [string]$Sid,
        [string]$CancellationEventName,
        [int]$CoordinatorProcessId,
        [long]$CoordinatorStartTicks,
        [string]$MonitorMutexName,
        [Threading.EventWaitHandle]$MsiMayHaveStartedEvent,
        [Threading.EventWaitHandle]$MsiDefinitiveCompletionEvent,
        [int]$TimeoutSeconds
    )

    if (-not (Test-IsAdministrator)) {
        throw "The MSI phase must run elevated."
    }

    $cancellation = $null
    $process = $null
    $msiStarted = $false
    try {
        $cancellation = Open-BoundlessInstallerCancellationBoundary `
            -EventName $CancellationEventName `
            -CoordinatorProcessId $CoordinatorProcessId `
            -CoordinatorStartTicks $CoordinatorStartTicks `
            -MonitorMutexName $MonitorMutexName
        $initialCancellation = Get-BoundlessInstallerCancellationReason -Boundary $cancellation
        if (-not [string]::IsNullOrWhiteSpace($initialCancellation)) {
            Throw-BoundlessMsiFailure `
                -Message "msiexec.exe was not started because $initialCancellation." `
                -CompletionState "not_started"
        }
        $arguments = New-BoundlessMsiArguments -ResolvedInstallerPath $ResolvedInstallerPath -Sid $Sid
        # Publish the conservative boundary before CreateProcess. If this helper
        # is hard-killed after this Set(), the bootstrap must assume Windows
        # Installer may own work even when no client remains to report it.
        if (-not $MsiMayHaveStartedEvent.Set()) {
            throw "Could not publish the MSI may-have-started boundary."
        }
        try {
            $process = Start-BoundlessOwnedProcessBoundary `
                -FilePath "msiexec.exe" `
                -ArgumentList $arguments
            $msiStarted = $true
        }
        catch {
            if ($_.Exception.Message -match 'cleanup could not be proven') {
                Throw-BoundlessMsiFailure `
                    -Message "msiexec.exe launch failed without proven child cleanup. $($_.Exception.Message)" `
                    -CompletionState "uncertain"
            }
            # A returned CreateProcess failure is definitive. The pre-MSI
            # service recovery path can safely restore an originally-running
            # service while the bootstrap observes a non-uncertain boundary.
            [void]$MsiMayHaveStartedEvent.Reset()
            [void]$MsiDefinitiveCompletionEvent.Set()
            Throw-BoundlessMsiFailure `
                -Message "msiexec.exe could not be started. $($_.Exception.Message)" `
                -CompletionState "not_started"
        }
        $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
        while (-not $process.WaitForExit(100)) {
            $cancellationReason = Get-BoundlessInstallerCancellationReason -Boundary $cancellation
            if (-not [string]::IsNullOrWhiteSpace($cancellationReason)) {
                Stop-BoundlessProcessBoundary -Process $process
                Throw-BoundlessMsiFailure `
                    -Message "msiexec.exe was canceled because $cancellationReason." `
                    -CompletionState "uncertain"
            }
            if ((Get-Date) -ge $deadline) {
                Stop-BoundlessProcessBoundary -Process $process
                Throw-BoundlessMsiFailure `
                    -Message "msiexec.exe exceeded the bounded $TimeoutSeconds second install window." `
                    -CompletionState "uncertain"
            }
        }
        if (-not $process.WaitForTreeExit(5000)) {
            Stop-BoundlessProcessBoundary -Process $process
            Throw-BoundlessMsiFailure `
                -Message "msiexec.exe exited but its owned process tree did not close." `
                -CompletionState "uncertain"
        }
        $exitCode = $process.ExitCode
        if ($exitCode -notin @(0, 3010)) {
            [void]$MsiMayHaveStartedEvent.Reset()
        }
        if (-not $MsiDefinitiveCompletionEvent.Set()) {
            Throw-BoundlessMsiFailure `
                -Message "msiexec.exe exited but definitive completion evidence could not be published." `
                -CompletionState "uncertain"
        }
        if ($exitCode -notin @(0, 3010)) {
            Throw-BoundlessMsiFailure `
                -Message "msiexec.exe failed with exit code $exitCode after the transaction client completed." `
                -CompletionState "definitive_failure"
        }
        return $exitCode
    }
    catch {
        $originalError = $_
        if (-not $originalError.Exception.Data.Contains("BoundlessMsiCompletionState")) {
            $originalError.Exception.Data["BoundlessMsiCompletionState"] = if ($msiStarted) {
                "uncertain"
            }
            else {
                "not_started"
            }
        }
        throw $originalError
    }
    finally {
        if ($null -ne $process) {
            if ($process.ActiveProcessCount -gt 0) {
                Stop-BoundlessProcessBoundary -Process $process
            }
            $process.Dispose()
        }
        Close-BoundlessInstallerCancellationBoundary -Boundary $cancellation
    }
}

function Resolve-CurrentPowerShellExecutable {
    $currentProcess = Get-Process -Id $PID -ErrorAction Stop
    if (-not [string]::IsNullOrWhiteSpace($currentProcess.Path)) {
        return $currentProcess.Path
    }

    foreach ($candidate in @("pwsh.exe", "powershell.exe")) {
        $command = Get-Command $candidate -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $command) {
            return $command.Source
        }
    }
    throw "Could not resolve the current PowerShell executable for elevation."
}

function New-BoundlessTrayOwnerMutexSecurity {
    param([string]$UserSid)

    Assert-AllowedUserSid -Sid $UserSid
    $security = [Security.AccessControl.MutexSecurity]::new()
    $security.SetSecurityDescriptorSddlForm(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;$UserSid)"
    )
    return $security
}

function Test-BoundlessTrayOwnerMutexSecurity {
    param(
        [Security.AccessControl.MutexSecurity]$Security,
        [string]$UserSid
    )

    Assert-AllowedUserSid -Sid $UserSid
    if (-not $Security.AreAccessRulesProtected) {
        return $false
    }
    $rules = @(
        $Security.GetAccessRules(
            $true,
            $true,
            [Security.Principal.SecurityIdentifier]
        )
    )
    $expectedSids = @(
        "S-1-5-18",
        "S-1-5-32-544",
        $UserSid
    ) | Select-Object -Unique
    $genericAll = [uint32]0x10000000
    $mutexFullControl = [uint32][Security.AccessControl.MutexRights]::FullControl
    foreach ($expectedSid in $expectedSids) {
        $matchingRule = $rules | Where-Object {
            $rights = [uint32]$_.MutexRights
            $_.IdentityReference.Value -eq $expectedSid -and
                $_.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow -and
                -not $_.IsInherited -and
                (
                    ($rights -band $genericAll) -eq $genericAll -or
                    ($rights -band $mutexFullControl) -eq $mutexFullControl
                )
        } | Select-Object -First 1
        if ($null -eq $matchingRule) {
            return $false
        }
    }
    return $true
}

function Test-BoundlessProtectedKernelObjectSecurity {
    param(
        [Security.AccessControl.NativeObjectSecurity]$Security,
        [object[]]$ExpectedRules
    )

    if (-not $Security.AreAccessRulesProtected) {
        return $false
    }
    $rules = @(
        $Security.GetAccessRules(
            $true,
            $true,
            [Security.Principal.SecurityIdentifier]
        )
    )
    foreach ($rule in $rules) {
        $rightsProperty = if (
            $null -ne $rule.PSObject.Properties["EventWaitHandleRights"]
        ) {
            "EventWaitHandleRights"
        }
        elseif ($null -ne $rule.PSObject.Properties["MutexRights"]) {
            "MutexRights"
        }
        else {
            return $false
        }
        $rights = [uint32]$rule.$rightsProperty
        $expected = $ExpectedRules | Where-Object {
            $_.sid -eq $rule.IdentityReference.Value -and
                [uint32]$_.rights -eq $rights
        } | Select-Object -First 1
        if (
            $null -eq $expected -or
            $rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or
            $rule.IsInherited
        ) {
            return $false
        }
    }
    foreach ($expected in $ExpectedRules) {
        $matchingRule = $rules | Where-Object {
            $rightsProperty = if (
                $null -ne $_.PSObject.Properties["EventWaitHandleRights"]
            ) {
                "EventWaitHandleRights"
            }
            else {
                "MutexRights"
            }
            $_.IdentityReference.Value -eq $expected.sid -and
                [uint32]$_.$rightsProperty -eq [uint32]$expected.rights -and
                $_.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow -and
                -not $_.IsInherited
        } | Select-Object -First 1
        if ($null -eq $matchingRule) {
            return $false
        }
    }
    return $true
}

function New-BoundlessNamedMutex {
    param(
        [string]$Name,
        [string]$UserSid,
        [bool]$InitiallyOwned
    )

    $security = New-BoundlessTrayOwnerMutexSecurity -UserSid $UserSid
    $arguments = [object[]]@($InitiallyOwned, $Name, $false, $security)
    $mutexAclType = "System.Threading.MutexAcl" -as [type]
    if ($null -ne $mutexAclType) {
        $createMethod = $mutexAclType.GetMethods() |
            Where-Object { $_.Name -eq "Create" -and $_.GetParameters().Count -eq 4 } |
            Select-Object -First 1
        if ($null -eq $createMethod) {
            throw "Could not resolve MutexAcl.Create for the tray quiescence lease."
        }
        $mutex = $createMethod.Invoke($null, $arguments)
    }
    else {
        $constructor = [Threading.Mutex].GetConstructors() |
            Where-Object { $_.GetParameters().Count -eq 4 } |
            Select-Object -First 1
        if ($null -eq $constructor) {
            throw "Could not resolve the secured Mutex constructor for the tray quiescence lease."
        }
        $mutex = $constructor.Invoke($arguments)
    }

    return [pscustomobject]@{
        mutex = $mutex
        created_new = [bool]$arguments[2]
        name = $Name
    }
}

function New-BoundlessInstallerControlEvent {
    param([string]$UserSid)

    Assert-AllowedUserSid -Sid $UserSid
    $name = "Local\Boundless.Installer.Cancel.v1.$([guid]::NewGuid().ToString('N'))"
    $security = [Security.AccessControl.EventWaitHandleSecurity]::new()
    $security.SetSecurityDescriptorSddlForm(
        "D:P(A;;RC;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)"
    )
    $arguments = [object[]]@(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $name,
        $false,
        $security
    )
    $eventAclType = "System.Threading.EventWaitHandleAcl" -as [type]
    if ($null -ne $eventAclType) {
        $createMethod = $eventAclType.GetMethods() |
            Where-Object { $_.Name -eq "Create" -and $_.GetParameters().Count -eq 5 } |
            Select-Object -First 1
        if ($null -eq $createMethod) {
            throw "Could not resolve EventWaitHandleAcl.Create for installer supervision."
        }
        $event = $createMethod.Invoke($null, $arguments)
    }
    else {
        $constructor = [Threading.EventWaitHandle].GetConstructors() |
            Where-Object { $_.GetParameters().Count -eq 5 } |
            Select-Object -First 1
        if ($null -eq $constructor) {
            throw "Could not resolve the secured EventWaitHandle constructor for installer supervision."
        }
        $event = $constructor.Invoke($arguments)
    }
    if (-not [bool]$arguments[3]) {
        $event.Dispose()
        throw "Could not create a unique installer supervision event."
    }
    return [pscustomobject]@{
        event = $event
        name = $name
        created_new = $true
        security = $security
    }
}

function Open-BoundlessInstallerCancellationBoundary {
    param(
        [string]$EventName,
        [int]$CoordinatorProcessId,
        [long]$CoordinatorStartTicks,
        [string]$MonitorMutexName
    )

    if ($CoordinatorProcessId -le 0 -or $CoordinatorStartTicks -le 0) {
        throw "Installer coordinator process identity was invalid."
    }
    if ($MonitorMutexName -notmatch '^Local\\Boundless\.Installer\.Monitor\.v1\.[0-9a-f]{32}$') {
        throw "Installer monitor liveness mutex name was invalid."
    }
    $event = Open-BoundlessInstallerCancellationEvent -Name $EventName
    $coordinator = $null
    $monitor = $null
    try {
        $coordinator = Get-Process -Id $CoordinatorProcessId -ErrorAction Stop
        if ($coordinator.StartTime.ToUniversalTime().Ticks -ne $CoordinatorStartTicks) {
            throw "Installer coordinator process identity changed."
        }
        $monitor = [Threading.Mutex]::OpenExisting($MonitorMutexName)
        return [pscustomobject]@{
            event = $event
            coordinator = $coordinator
            monitor = $monitor
        }
    }
    catch {
        if ($null -ne $monitor) { $monitor.Dispose() }
        if ($null -ne $coordinator) { $coordinator.Dispose() }
        $event.Dispose()
        throw "Installer cancellation boundary was unavailable. $($_.Exception.Message)"
    }
}

function Get-BoundlessInstallerCancellationReason {
    param([object]$Boundary)

    if ($Boundary.event.WaitOne(0)) {
        return "coordinator cancellation was signaled"
    }
    if ($Boundary.coordinator.HasExited) {
        return "coordinator process ended"
    }
    foreach ($probe in @(
            [pscustomobject]@{ name = "quiescence monitor"; mutex = $Boundary.monitor }
        )) {
        $acquired = $false
        try {
            $acquired = $probe.mutex.WaitOne(0)
            if ($acquired) {
                return "$($probe.name) ownership ended"
            }
        }
        catch [Threading.AbandonedMutexException] {
            $acquired = $true
            return "$($probe.name) was abandoned"
        }
        finally {
            if ($acquired) {
                try { $probe.mutex.ReleaseMutex() } catch { }
            }
        }
    }
    return ""
}

function Close-BoundlessInstallerCancellationBoundary {
    param([object]$Boundary)

    if ($null -eq $Boundary) { return }
    $Boundary.monitor.Dispose()
    $Boundary.coordinator.Dispose()
    $Boundary.event.Dispose()
}

function New-BoundlessInstallerCompletionEvent {
    param([string]$UserSid)

    Assert-AllowedUserSid -Sid $UserSid
    $name = "Local\Boundless.Installer.TreeComplete.v1.$([guid]::NewGuid().ToString('N'))"
    $security = [Security.AccessControl.EventWaitHandleSecurity]::new()
    $security.SetSecurityDescriptorSddlForm(
        "D:P(A;;RC;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00100000;;;$UserSid)"
    )
    $arguments = [object[]]@(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $name,
        $false,
        $security
    )
    $eventAclType = "System.Threading.EventWaitHandleAcl" -as [type]
    if ($null -ne $eventAclType) {
        $createMethod = $eventAclType.GetMethods() |
            Where-Object { $_.Name -eq "Create" -and $_.GetParameters().Count -eq 5 } |
            Select-Object -First 1
        if ($null -eq $createMethod) {
            throw "Could not resolve EventWaitHandleAcl.Create for installer completion."
        }
        $event = $createMethod.Invoke($null, $arguments)
    }
    else {
        $constructor = [Threading.EventWaitHandle].GetConstructors() |
            Where-Object { $_.GetParameters().Count -eq 5 } |
            Select-Object -First 1
        if ($null -eq $constructor) {
            throw "Could not resolve the secured EventWaitHandle constructor for installer completion."
        }
        $event = $constructor.Invoke($arguments)
    }
    if (-not [bool]$arguments[3]) {
        $event.Dispose()
        throw "Could not create a unique installer completion event."
    }
    return [pscustomobject]@{
        event = $event
        name = $name
        created_new = $true
        security = $security
    }
}

function Get-BoundlessInstallerPhaseEventPrefix {
    param(
        [ValidateSet(
            "ServiceInitialRunning",
            "MsiMayHaveStarted",
            "MsiDefinitiveCompletion",
            "MsiIdleProven"
        )]
        [string]$Phase
    )

    return "Boundless.Installer.$Phase.v1"
}

function New-BoundlessInstallerPhaseEvent {
    param(
        [string]$UserSid,
        [ValidateSet(
            "ServiceInitialRunning",
            "MsiMayHaveStarted",
            "MsiDefinitiveCompletion",
            "MsiIdleProven"
        )]
        [string]$Phase,
        [string]$InstanceId = ""
    )

    Assert-AllowedUserSid -Sid $UserSid
    if ([string]::IsNullOrWhiteSpace($InstanceId)) {
        $InstanceId = [guid]::NewGuid().ToString('N')
    }
    elseif ($InstanceId -notmatch '^[0-9a-f]{32}$') {
        throw "Installer phase instance id was invalid."
    }
    $prefix = Get-BoundlessInstallerPhaseEventPrefix -Phase $Phase
    $name = "Local\$prefix.$InstanceId"
    $security = [Security.AccessControl.EventWaitHandleSecurity]::new()
    # The desktop user and monitor can only observe phase evidence. Only the
    # creating coordinator handle, SYSTEM, or an elevated administrator can
    # mutate the authoritative service/MSI state.
    $security.SetSecurityDescriptorSddlForm(
        "D:P(A;;RC;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00100000;;;$UserSid)"
    )
    $arguments = [object[]]@(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $name,
        $false,
        $security
    )
    $eventAclType = "System.Threading.EventWaitHandleAcl" -as [type]
    if ($null -ne $eventAclType) {
        $createMethod = $eventAclType.GetMethods() |
            Where-Object { $_.Name -eq "Create" -and $_.GetParameters().Count -eq 5 } |
            Select-Object -First 1
        if ($null -eq $createMethod) {
            throw "Could not resolve EventWaitHandleAcl.Create for installer phase evidence."
        }
        $event = $createMethod.Invoke($null, $arguments)
    }
    else {
        $constructor = [Threading.EventWaitHandle].GetConstructors() |
            Where-Object { $_.GetParameters().Count -eq 5 } |
            Select-Object -First 1
        if ($null -eq $constructor) {
            throw "Could not resolve the secured EventWaitHandle constructor for installer phase evidence."
        }
        $event = $constructor.Invoke($arguments)
    }
    if (-not [bool]$arguments[3]) {
        $event.Dispose()
        throw "Could not create a unique installer $Phase phase event."
    }
    return [pscustomobject]@{
        event = $event
        name = $name
        phase = $Phase
        created_new = $true
        security = $security
    }
}

function Open-BoundlessInstallerPhaseEvent {
    param(
        [string]$Name,
        [ValidateSet(
            "ServiceInitialRunning",
            "MsiMayHaveStarted",
            "MsiDefinitiveCompletion",
            "MsiIdleProven"
        )]
        [string]$Phase
    )

    $prefix = [regex]::Escape((Get-BoundlessInstallerPhaseEventPrefix -Phase $Phase))
    if ($Name -notmatch "^Local\\$prefix\.[0-9a-f]{32}$") {
        throw "Installer $Phase phase event name was invalid."
    }
    return [Threading.EventWaitHandle]::OpenExisting($Name)
}

function Update-BoundlessInstallerPhaseEvidence {
    param([object]$Lease)

    $mayHaveStarted = $Lease.msi_may_have_started_event.WaitOne(0)
    $definitive = $Lease.msi_definitive_completion_event.WaitOne(0)
    $idleProven = $Lease.msi_idle_proven_event.WaitOne(0)
    $completionState = if ($definitive) {
        "definitive"
    }
    elseif (-not $mayHaveStarted) {
        "not_started"
    }
    else {
        "uncertain"
    }
    $Lease.evidence.installer_completion_state = $completionState
    $Lease.evidence.msi_may_have_started = $mayHaveStarted
    $Lease.evidence.msi_definitive_completion = $definitive
    $Lease.evidence.msi_transaction_idle_proven = $idleProven
    return $completionState
}

function Test-BoundlessNormalQuiescenceReleaseAllowed {
    param(
        [bool]$InstallerTreeClosed,
        [ValidateSet("not_started", "definitive", "uncertain")]
        [string]$CompletionState,
        [bool]$MsiTransactionIdleProven,
        [bool]$RecoveryAuthorityDrained = $true,
        [bool]$RecoveryActionSettled = $true
    )

    return (
        $InstallerTreeClosed -and
        $RecoveryAuthorityDrained -and
        $RecoveryActionSettled -and
        ($CompletionState -ne "uncertain" -or $MsiTransactionIdleProven)
    )
}

function Wait-BoundlessWindowsInstallerTransactionIdleProof {
    param(
        [int]$TimeoutMilliseconds = 15000,
        [string]$MutexName = "Global\_MSIExecute"
    )

    $deadline = (Get-Date).AddMilliseconds($TimeoutMilliseconds)
    do {
        $mutex = $null
        $owned = $false
        $created = $false
        try {
            $mutex = [Threading.Mutex]::new($true, $MutexName, [ref]$created)
            $owned = $created
            if (-not $owned) {
                try { $owned = $mutex.WaitOne(0) }
                catch [Threading.AbandonedMutexException] { $owned = $true }
            }
            if ($owned) { return $true }
        }
        catch { }
        finally {
            if ($owned -and $null -ne $mutex) {
                try { $mutex.ReleaseMutex() } catch { }
            }
            if ($null -ne $mutex) { $mutex.Dispose() }
        }
        Start-Sleep -Milliseconds 50
    } while ((Get-Date) -lt $deadline)
    return $false
}

function Dispose-BoundlessInstallerPhaseEvidence {
    param([object]$Lease)

    foreach ($propertyName in @(
            "service_initial_running_event",
            "msi_may_have_started_event",
            "msi_definitive_completion_event",
            "msi_idle_proven_event"
        )) {
        $property = $Lease.PSObject.Properties[$propertyName]
        if ($null -ne $property -and $null -ne $property.Value) {
            $property.Value.Dispose()
            $property.Value = $null
        }
    }
}

function New-BoundlessPrivilegedLivenessMutex {
    param([string]$Name)

    if ($Name -notmatch '^Local\\Boundless\.Installer\.Monitor\.v1\.[0-9a-f]{32}$') {
        throw "Installer monitor liveness mutex name was invalid."
    }
    $security = [Security.AccessControl.MutexSecurity]::new()
    $security.SetSecurityDescriptorSddlForm(
        "D:P(A;;RC;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)"
    )
    $arguments = [object[]]@($true, $Name, $false, $security)
    $mutexAclType = "System.Threading.MutexAcl" -as [type]
    if ($null -ne $mutexAclType) {
        $createMethod = $mutexAclType.GetMethods() |
            Where-Object { $_.Name -eq "Create" -and $_.GetParameters().Count -eq 4 } |
            Select-Object -First 1
        if ($null -eq $createMethod) {
            throw "Could not resolve MutexAcl.Create for installer monitor liveness."
        }
        $mutex = $createMethod.Invoke($null, $arguments)
    }
    else {
        $constructor = [Threading.Mutex].GetConstructors() |
            Where-Object { $_.GetParameters().Count -eq 4 } |
            Select-Object -First 1
        if ($null -eq $constructor) {
            throw "Could not resolve the secured Mutex constructor for installer monitor liveness."
        }
        $mutex = $constructor.Invoke($arguments)
    }
    if (-not [bool]$arguments[2]) {
        $mutex.Dispose()
        throw "Could not create a unique installer monitor liveness mutex."
    }
    return [pscustomobject]@{
        mutex = $mutex
        name = $Name
        created_new = $true
        owned = $true
        security = $security
    }
}

function Get-BoundlessTrayKernelObjectBaseName {
    param(
        [string]$UserSid,
        [int]$SessionId
    )

    return "Local\Boundless.Tray.SingleInstance.v1.$UserSid.$SessionId"
}

function Get-BoundlessTrayOwnerMutexName {
    param(
        [string]$UserSid,
        [int]$SessionId
    )
    return "$(Get-BoundlessTrayKernelObjectBaseName -UserSid $UserSid -SessionId $SessionId).Owner"
}

function Get-BoundlessTrayShutdownEventName {
    param(
        [string]$UserSid,
        [int]$SessionId
    )
    return "$(Get-BoundlessTrayKernelObjectBaseName -UserSid $UserSid -SessionId $SessionId).Shutdown"
}

function Request-BoundlessTrayShutdownSignal {
    param(
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId
    )

    $eventName = Get-BoundlessTrayShutdownEventName `
        -UserSid $ExpectedOwnerSid `
        -SessionId $ExpectedSessionId
    try {
        $shutdownEvent = [Threading.EventWaitHandle]::OpenExisting($eventName)
    }
    catch {
        $cause = if ($null -ne $_.Exception.InnerException) {
            $_.Exception.InnerException
        }
        else {
            $_.Exception
        }
        if (
            $cause -is [Threading.WaitHandleCannotBeOpenedException] -or
            $cause -is [UnauthorizedAccessException]
        ) {
            return $false
        }
        throw "Could not open the trusted Boundless tray shutdown event '$eventName'. $($cause.Message)"
    }
    try {
        if (-not $shutdownEvent.Set()) {
            throw "The trusted Boundless tray shutdown event rejected Set()."
        }
        return $true
    }
    finally {
        $shutdownEvent.Dispose()
    }
}

function Get-BoundlessTrayQuiescenceSentinelName {
    param(
        [string]$UserSid,
        [int]$SessionId
    )
    return "Local\Boundless.Tray.UpgradeQuiescence.v1.$UserSid.$SessionId"
}

function New-BoundlessTrayQuiescenceMonitorCommand {
    param(
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId,
        [string]$SentinelName,
        [string]$ReadyEventName,
        [string]$HandoffEventName,
        [string]$MonitorMutexName,
        [string]$TreeJobName,
        [string]$CompletionEventName,
        [string]$HeartbeatEventName,
        [string]$MsiMayHaveStartedEventName = "",
        [string]$MsiDefinitiveCompletionEventName = "",
        [string]$MsiIdleProvenEventName = "",
        [string]$InstallerTransactionMutexName = "Global\_MSIExecute",
        [int]$StableMilliseconds = 500,
        [int]$FixtureProcessId = 0,
        [string]$FixtureProcessName = ""
    )

    if (
        -not [string]::IsNullOrWhiteSpace($FixtureProcessName) -and
        $FixtureProcessName -notmatch '^BoundlessFixtureTray[0-9a-f]{8}$'
    ) {
        throw "Tray quiescence monitor received an invalid fixture process name."
    }
    if ($HeartbeatEventName -notmatch '^Local\\Boundless\.Tray\.UpgradeMonitorHeartbeat\.v1\.[0-9a-f]{32}$') {
        throw "Tray quiescence monitor received an invalid heartbeat event."
    }
    if ($InstallerTransactionMutexName -notmatch '^Global\\(?:_MSIExecute|Boundless\.Test\.MsiExecute\.[0-9a-f]{32})$') {
        throw "Tray quiescence monitor received an invalid Windows Installer transaction mutex."
    }
    foreach ($phaseEvent in @(
            [pscustomobject]@{ name = $MsiMayHaveStartedEventName; phase = "MsiMayHaveStarted" },
            [pscustomobject]@{ name = $MsiDefinitiveCompletionEventName; phase = "MsiDefinitiveCompletion" },
            [pscustomobject]@{ name = $MsiIdleProvenEventName; phase = "MsiIdleProven" }
        )) {
        if ([string]::IsNullOrWhiteSpace($phaseEvent.name)) { continue }
        $prefix = [regex]::Escape((Get-BoundlessInstallerPhaseEventPrefix -Phase $phaseEvent.phase))
        if ($phaseEvent.name -notmatch "^Local\\$prefix\.[0-9a-f]{32}$") {
            throw "Tray quiescence monitor received an invalid $($phaseEvent.phase) event."
        }
    }

    $payload = [ordered]@{
        expected_owner_sid = $ExpectedOwnerSid
        expected_session_id = $ExpectedSessionId
        sentinel_name = $SentinelName
        ready_event_name = $ReadyEventName
        handoff_event_name = $HandoffEventName
        monitor_mutex_name = $MonitorMutexName
        tree_job_name = $TreeJobName
        completion_event_name = $CompletionEventName
        heartbeat_event_name = $HeartbeatEventName
        msi_may_have_started_event_name = $MsiMayHaveStartedEventName
        msi_definitive_completion_event_name = $MsiDefinitiveCompletionEventName
        msi_idle_proven_event_name = $MsiIdleProvenEventName
        installer_transaction_mutex_name = $InstallerTransactionMutexName
        stable_milliseconds = $StableMilliseconds
        fixture_process_id = $FixtureProcessId
        fixture_process_name = $FixtureProcessName
    }
    $payloadBase64 = [Convert]::ToBase64String(
        [Text.Encoding]::UTF8.GetBytes(($payload | ConvertTo-Json -Compress))
    )
    $source = @'
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
Add-Type -TypeDefinition @"
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class BoundlessUpgradeMonitorNativeMethods
{
    private const uint JOB_OBJECT_QUERY = 0x0004;
    private const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;
    private const uint TOKEN_QUERY = 0x0008;
    private const int JobObjectBasicAccountingInformation = 1;
    private const int ERROR_FILE_NOT_FOUND = 2;
    private const int ERROR_INVALID_PARAMETER = 87;

    [StructLayout(LayoutKind.Sequential)]
    private struct JOBOBJECT_BASIC_ACCOUNTING_INFORMATION
    {
        public long TotalUserTime;
        public long TotalKernelTime;
        public long ThisPeriodTotalUserTime;
        public long ThisPeriodTotalKernelTime;
        public uint TotalPageFaultCount;
        public uint TotalProcesses;
        public uint ActiveProcesses;
        public uint TotalTerminatedProcesses;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct SID_AND_ATTRIBUTES
    {
        public IntPtr Sid;
        public uint Attributes;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct TOKEN_USER
    {
        public SID_AND_ATTRIBUTES User;
    }

    [DllImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool PostThreadMessage(
        uint threadId,
        uint message,
        UIntPtr wParam,
        IntPtr lParam);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr OpenJobObjectW(uint desiredAccess, bool inheritHandle, string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool QueryInformationJobObject(
        IntPtr job,
        int informationClass,
        out JOBOBJECT_BASIC_ACCOUNTING_INFORMATION information,
        uint length,
        IntPtr returnLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr handle);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(uint desiredAccess, bool inheritHandle, int processId);

    [DllImport("advapi32.dll", SetLastError = true)]
    private static extern bool OpenProcessToken(IntPtr processHandle, uint desiredAccess, out IntPtr tokenHandle);

    [DllImport("advapi32.dll", SetLastError = true)]
    private static extern bool GetTokenInformation(
        IntPtr tokenHandle,
        int tokenInformationClass,
        IntPtr tokenInformation,
        int tokenInformationLength,
        out int returnLength);

    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool ConvertSidToStringSidW(IntPtr sid, out IntPtr stringSid);

    [DllImport("kernel32.dll")]
    private static extern IntPtr LocalFree(IntPtr memory);

    public static int GetNamedJobActiveProcessCount(string name)
    {
        IntPtr job = OpenJobObjectW(JOB_OBJECT_QUERY, false, name);
        if (job == IntPtr.Zero)
        {
            int error = Marshal.GetLastWin32Error();
            if (error == ERROR_FILE_NOT_FOUND) { return -1; }
            throw new Win32Exception(error, "OpenJobObject failed");
        }
        try
        {
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION accounting;
            if (!QueryInformationJobObject(
                job,
                JobObjectBasicAccountingInformation,
                out accounting,
                unchecked((uint)Marshal.SizeOf(typeof(JOBOBJECT_BASIC_ACCOUNTING_INFORMATION))),
                IntPtr.Zero))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "QueryInformationJobObject failed");
            }
            return unchecked((int)accounting.ActiveProcesses);
        }
        finally { CloseHandle(job); }
    }

    public static string GetProcessOwnerSid(int processId)
    {
        IntPtr process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, processId);
        if (process == IntPtr.Zero)
        {
            int error = Marshal.GetLastWin32Error();
            if (error == ERROR_INVALID_PARAMETER) { return String.Empty; }
            throw new Win32Exception(error, "OpenProcess(owner lookup) failed");
        }
        IntPtr token = IntPtr.Zero;
        IntPtr buffer = IntPtr.Zero;
        IntPtr sidText = IntPtr.Zero;
        try
        {
            if (!OpenProcessToken(process, TOKEN_QUERY, out token))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenProcessToken failed");
            int required;
            GetTokenInformation(token, 1, IntPtr.Zero, 0, out required);
            int sizeError = Marshal.GetLastWin32Error();
            if (required <= 0 || sizeError != 122)
                throw new Win32Exception(sizeError, "GetTokenInformation(size) failed");
            buffer = Marshal.AllocHGlobal(required);
            if (!GetTokenInformation(token, 1, buffer, required, out required))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "GetTokenInformation(TokenUser) failed");
            TOKEN_USER user = (TOKEN_USER)Marshal.PtrToStructure(buffer, typeof(TOKEN_USER));
            if (!ConvertSidToStringSidW(user.User.Sid, out sidText))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "ConvertSidToStringSid failed");
            return Marshal.PtrToStringUni(sidText);
        }
        finally
        {
            if (sidText != IntPtr.Zero) LocalFree(sidText);
            if (buffer != IntPtr.Zero) Marshal.FreeHGlobal(buffer);
            if (token != IntPtr.Zero) CloseHandle(token);
            CloseHandle(process);
        }
    }
}
"@
function New-BoundlessPrivilegedLivenessMutex {
__PRIVILEGED_LIVENESS_MUTEX_FUNCTION__
}
function Wait-InstallerTreeClosureAfterCoordinatorDeath {
    param(
        [Threading.EventWaitHandle]$CompletionEvent,
        [Threading.EventWaitHandle]$HeartbeatEvent,
        [string]$TreeJobName,
        [object]$Payload
    )
    $jobObserved = $false
    $missingSince = $null
    while ($true) {
        [void]$HeartbeatEvent.Set()
        # The currently installed tray may predate the upgrade sentinel check.
        # Keep closing replacement trays until the elevated tree has drained.
        [void](Stop-ReplacementTrays -Payload $Payload)
        if ($CompletionEvent.WaitOne(0)) {
            return
        }
        $active = [BoundlessUpgradeMonitorNativeMethods]::GetNamedJobActiveProcessCount($TreeJobName)
        if ($active -ge 0) {
            $jobObserved = $true
            $missingSince = $null
        }
        elseif ($jobObserved) {
            return
        }
        else {
            if ($null -eq $missingSince) { $missingSince = Get-Date }
            if (((Get-Date) - $missingSince).TotalSeconds -ge 5) {
                # No owned job was ever published. Any delayed elevated helper
                # must still validate the now-ended sentinel before it can
                # create a child, so there is no MSI tree to retain here.
                return
            }
        }
        Start-Sleep -Milliseconds 50
    }
}
function Open-SynchronizeEvent {
    param([string]$Name)
    $rights = [Security.AccessControl.EventWaitHandleRights]::Synchronize
    $aclType = "System.Threading.EventWaitHandleAcl" -as [type]
    if ($null -ne $aclType) {
        $method = $aclType.GetMethods() | Where-Object {
            $_.Name -eq "OpenExisting" -and $_.GetParameters().Count -eq 2
        } | Select-Object -First 1
        return $method.Invoke($null, [object[]]@($Name, $rights))
    }
    return [Threading.EventWaitHandle]::OpenExisting($Name, $rights)
}
function Test-WindowsInstallerTransactionIdle {
    param([string]$MutexName)
    $mutex = $null
    $owned = $false
    $created = $false
    try {
        # Creating-or-opening and then acquiring the authoritative Windows
        # Installer execution mutex closes the missing-object race. Merely
        # observing the msiexec client tree as empty is not transaction proof.
        $mutex = [Threading.Mutex]::new($true, $MutexName, [ref]$created)
        $owned = $created
        if (-not $owned) {
            try { $owned = $mutex.WaitOne(0) }
            catch [Threading.AbandonedMutexException] { $owned = $true }
        }
        return $owned
    }
    catch {
        return $false
    }
    finally {
        if ($owned -and $null -ne $mutex) {
            try { $mutex.ReleaseMutex() } catch { }
        }
        if ($null -ne $mutex) { $mutex.Dispose() }
    }
}
function Wait-WindowsInstallerTransactionIdleFailClosed {
    param(
        [Threading.EventWaitHandle]$HeartbeatEvent,
        [object]$Payload
    )
    while (-not (Test-WindowsInstallerTransactionIdle `
        -MutexName ([string]$Payload.installer_transaction_mutex_name))) {
        [void]$HeartbeatEvent.Set()
        [void](Stop-ReplacementTrays -Payload $Payload)
        Start-Sleep -Milliseconds 100
    }
}
function Hold-QuiescenceAfterGuardianFailure {
    param(
        [Threading.EventWaitHandle]$HeartbeatEvent,
        [object]$Payload
    )
    while ($true) {
        try { [void]$HeartbeatEvent.Set() } catch { }
        try { [void](Stop-ReplacementTrays -Payload $Payload) } catch { }
        [Threading.Thread]::Sleep(100)
    }
}
function Stop-ReplacementTrays {
    param([object]$Payload)
    $targets = @(
        if ([int]$Payload.fixture_process_id -gt 0) {
            Get-Process -Id ([int]$Payload.fixture_process_id) -ErrorAction SilentlyContinue |
                Where-Object { $_.SessionId -eq [int]$Payload.expected_session_id }
        }
        elseif (-not [string]::IsNullOrWhiteSpace([string]$Payload.fixture_process_name)) {
            Get-Process -Name ([string]$Payload.fixture_process_name) -ErrorAction SilentlyContinue |
                Where-Object { $_.SessionId -eq [int]$Payload.expected_session_id }
        }
        else {
            Get-Process -Name "boundlesstray" -ErrorAction SilentlyContinue |
                Where-Object { $_.SessionId -eq [int]$Payload.expected_session_id }
        }
    )
    foreach ($target in $targets) {
        try {
            $ownerSid = [BoundlessUpgradeMonitorNativeMethods]::GetProcessOwnerSid($target.Id)
        }
        catch {
            if ($null -eq (Get-Process -Id $target.Id -ErrorAction SilentlyContinue)) {
                continue
            }
            throw
        }
        if ([string]::IsNullOrWhiteSpace($ownerSid)) {
            continue
        }
        if ($ownerSid -ne [string]$Payload.expected_owner_sid) {
            throw "Replacement tray PID $($target.Id) belonged to unexpected SID $ownerSid."
        }
        try {
            $threads = @($target.Threads)
        }
        catch {
            if ($null -eq (Get-Process -Id $target.Id -ErrorAction SilentlyContinue)) {
                continue
            }
            throw
        }
        foreach ($thread in $threads) {
            [void][BoundlessUpgradeMonitorNativeMethods]::PostThreadMessage(
                [uint32]$thread.Id,
                [uint32]0x0012,
                [UIntPtr]::Zero,
                [IntPtr]::Zero
            )
        }
    }
    return $targets.Count
}
$payloadJson = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD_BASE64__")
)
$payload = $payloadJson | ConvertFrom-Json
$ready = [Threading.EventWaitHandle]::OpenExisting([string]$payload.ready_event_name)
$handoff = [Threading.EventWaitHandle]::OpenExisting([string]$payload.handoff_event_name)
$heartbeat = [Threading.EventWaitHandle]::OpenExisting([string]$payload.heartbeat_event_name)
$completion = Open-SynchronizeEvent -Name ([string]$payload.completion_event_name)
$msiMayHaveStarted = if ([string]::IsNullOrWhiteSpace([string]$payload.msi_may_have_started_event_name)) {
    $null
}
else {
    Open-SynchronizeEvent -Name ([string]$payload.msi_may_have_started_event_name)
}
$msiDefinitiveCompletion = if ([string]::IsNullOrWhiteSpace([string]$payload.msi_definitive_completion_event_name)) {
    $null
}
else {
    Open-SynchronizeEvent -Name ([string]$payload.msi_definitive_completion_event_name)
}
$msiIdleProven = if ([string]::IsNullOrWhiteSpace([string]$payload.msi_idle_proven_event_name)) {
    $null
}
else {
    Open-SynchronizeEvent -Name ([string]$payload.msi_idle_proven_event_name)
}
$sentinel = [Threading.Mutex]::OpenExisting([string]$payload.sentinel_name)
$liveness = New-BoundlessPrivilegedLivenessMutex -Name ([string]$payload.monitor_mutex_name)
$sentinelOwned = $false
$sentinelReleaseAuthorized = $false
$livenessOwned = $true
try {
    $stableSince = $null
    $readySignaled = $false
    while ($true) {
        [void]$heartbeat.Set()
        $coordinatorEnded = $false
        $coordinatorAbandoned = $false
        try {
            if ($sentinel.WaitOne(0)) {
                $sentinelOwned = $true
                $coordinatorEnded = $true
            }
        }
        catch [Threading.AbandonedMutexException] {
            $sentinelOwned = $true
            $coordinatorEnded = $true
            $coordinatorAbandoned = $true
        }
        if ($coordinatorEnded) {
            if ($coordinatorAbandoned) {
                try {
                    $liveness.mutex.ReleaseMutex()
                    $livenessOwned = $false
                    [void]$handoff.Set()
                    Wait-InstallerTreeClosureAfterCoordinatorDeath `
                        -CompletionEvent $completion `
                        -HeartbeatEvent $heartbeat `
                        -TreeJobName ([string]$payload.tree_job_name) `
                        -Payload $payload
                    $uncertainTransaction = (
                        $null -ne $msiMayHaveStarted -and
                        $msiMayHaveStarted.WaitOne(0) -and
                        ($null -eq $msiDefinitiveCompletion -or -not $msiDefinitiveCompletion.WaitOne(0)) -and
                        ($null -eq $msiIdleProven -or -not $msiIdleProven.WaitOne(0))
                    )
                    if ($uncertainTransaction) {
                        Wait-WindowsInstallerTransactionIdleFailClosed `
                            -HeartbeatEvent $heartbeat `
                            -Payload $payload
                    }
                    $sentinelReleaseAuthorized = $true
                }
                catch {
                    Hold-QuiescenceAfterGuardianFailure `
                        -HeartbeatEvent $heartbeat `
                        -Payload $payload
                }
            }
            else {
                $sentinelReleaseAuthorized = $true
            }
            break
        }
        $targetCount = Stop-ReplacementTrays -Payload $payload
        if ($targetCount -gt 0) {
            $stableSince = $null
        }
        else {
            if ($null -eq $stableSince) {
                $stableSince = Get-Date
            }
            if (
                -not $readySignaled -and
                ((Get-Date) - $stableSince).TotalMilliseconds -ge [int]$payload.stable_milliseconds
            ) {
                [void]$ready.Set()
                $readySignaled = $true
            }
        }
        Start-Sleep -Milliseconds 50
    }
}
finally {
    if ($sentinelOwned -and $sentinelReleaseAuthorized) {
        try { $sentinel.ReleaseMutex() } catch { }
    }
    if ($livenessOwned) {
        try { $liveness.mutex.ReleaseMutex() } catch { }
    }
    $liveness.mutex.Dispose()
    $sentinel.Dispose()
    if ($null -ne $msiIdleProven) { $msiIdleProven.Dispose() }
    if ($null -ne $msiDefinitiveCompletion) { $msiDefinitiveCompletion.Dispose() }
    if ($null -ne $msiMayHaveStarted) { $msiMayHaveStarted.Dispose() }
    $completion.Dispose()
    $heartbeat.Dispose()
    $handoff.Dispose()
    $ready.Dispose()
}
'@
    $livenessMutexDefinition = (
        Get-Command New-BoundlessPrivilegedLivenessMutex -CommandType Function -ErrorAction Stop
    ).Definition
    $source = $source.Replace(
        "__PRIVILEGED_LIVENESS_MUTEX_FUNCTION__",
        $livenessMutexDefinition
    )
    $source = $source.Replace("__PAYLOAD_BASE64__", $payloadBase64)
    $encodedCommand = ConvertTo-BoundlessCompressedEncodedCommand -Source $source
    if ($encodedCommand.Length -gt 30000) {
        throw "The tray quiescence monitor exceeded the safe Windows command-line budget."
    }
    return $encodedCommand
}

function Start-BoundlessTrayQuiescenceMonitor {
    param(
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId,
        [string]$SentinelName,
        [string]$TreeJobName,
        [string]$CompletionEventName,
        [string]$MsiMayHaveStartedEventName = "",
        [string]$MsiDefinitiveCompletionEventName = "",
        [string]$MsiIdleProvenEventName = "",
        [string]$InstallerTransactionMutexName = "Global\_MSIExecute",
        [int]$FixtureProcessId = 0,
        [string]$FixtureProcessName = ""
    )

    $monitorId = [guid]::NewGuid().ToString('N')
    $readyEventName = "Local\Boundless.Tray.UpgradeMonitorReady.v1.$monitorId"
    $monitorMutexName = "Local\Boundless.Installer.Monitor.v1.$monitorId"
    $handoff = New-BoundlessSentinelOwnerEvent `
        -Prefix "Boundless.Tray.MonitorHandoff.v1" `
        -UserSid $ExpectedOwnerSid
    try {
        $heartbeat = New-BoundlessSentinelOwnerEvent `
            -Prefix "Boundless.Tray.UpgradeMonitorHeartbeat.v1" `
            -UserSid $ExpectedOwnerSid
    }
    catch {
        $handoff.event.Dispose()
        throw
    }
    $readyCreated = $false
    $readyEvent = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $readyEventName,
        [ref]$readyCreated
    )
    if (-not $readyCreated) {
        $readyEvent.Dispose()
        $heartbeat.event.Dispose()
        $handoff.event.Dispose()
        throw "Could not create a unique tray quiescence monitor handshake."
    }
    try {
        $encodedCommand = New-BoundlessTrayQuiescenceMonitorCommand `
            -ExpectedOwnerSid $ExpectedOwnerSid `
            -ExpectedSessionId $ExpectedSessionId `
            -SentinelName $SentinelName `
            -ReadyEventName $readyEventName `
            -HandoffEventName $handoff.name `
            -MonitorMutexName $monitorMutexName `
            -TreeJobName $TreeJobName `
            -CompletionEventName $CompletionEventName `
            -HeartbeatEventName $heartbeat.name `
            -MsiMayHaveStartedEventName $MsiMayHaveStartedEventName `
            -MsiDefinitiveCompletionEventName $MsiDefinitiveCompletionEventName `
            -MsiIdleProvenEventName $MsiIdleProvenEventName `
            -InstallerTransactionMutexName $InstallerTransactionMutexName `
            -FixtureProcessId $FixtureProcessId `
            -FixtureProcessName $FixtureProcessName
        $arguments = @("-NoProfile", "-EncodedCommand", $encodedCommand)
        $process = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList (@($arguments | ForEach-Object { ConvertTo-ProcessArgument -Value $_ }) -join " ") `
            -WindowStyle Hidden `
            -PassThru
        return [pscustomobject]@{
            process = $process
            ready_event = $readyEvent
            handoff_event = $handoff.event
            heartbeat_event = $heartbeat.event
            ready_event_name = $readyEventName
            heartbeat_event_name = $heartbeat.name
            liveness_mutex_name = $monitorMutexName
            stable_milliseconds = 500
        }
    }
    catch {
        $readyEvent.Dispose()
        $handoff.event.Dispose()
        $heartbeat.event.Dispose()
        throw
    }
}

function Wait-BoundlessTrayQuiescenceMonitorReady {
    param(
        [object]$Monitor,
        [int]$TimeoutSeconds
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        if ($Monitor.ready_event.WaitOne(50)) {
            return
        }
        if ($Monitor.process.HasExited) {
            throw "Tray quiescence monitor exited before proving a stable-zero tray state; exit=$($Monitor.process.ExitCode)."
        }
    } while ((Get-Date) -lt $deadline)
    throw "Tray quiescence monitor did not prove stable-zero within $($TimeoutSeconds)s."
}

function Complete-BoundlessTrayQuiescenceMonitor {
    param(
        [object]$Monitor,
        [bool]$ExitedBeforeSentinelRelease
    )

    try {
        if (-not $Monitor.process.WaitForExit(5000)) {
            $Monitor.process.Kill()
            throw "Tray quiescence monitor did not stop after the sentinel was released."
        }
        $exitCode = $Monitor.process.ExitCode
        if ($ExitedBeforeSentinelRelease -or $exitCode -ne 0) {
            throw "Tray quiescence monitor did not span the full MSI window; early=$ExitedBeforeSentinelRelease exit=$exitCode."
        }
        return [pscustomobject]@{
            completed = $true
            exit_code = $exitCode
        }
    }
    finally {
        $Monitor.handoff_event.Dispose()
        $Monitor.heartbeat_event.Dispose()
        $Monitor.ready_event.Dispose()
        $Monitor.process.Dispose()
    }
}

function New-BoundlessSentinelOwnerEvent {
    param(
        [string]$Prefix,
        [string]$UserSid,
        [ValidateSet("FullControl", "Synchronize")]
        [string]$UserAccess = "FullControl"
    )

    Assert-AllowedUserSid -Sid $UserSid
    $name = "Local\$Prefix.$([guid]::NewGuid().ToString('N'))"
    $userRights = if ($UserAccess -eq "Synchronize") { "0x00100000" } else { "GA" }
    $security = [Security.AccessControl.EventWaitHandleSecurity]::new()
    $security.SetSecurityDescriptorSddlForm(
        "D:P(A;;RC;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)(A;;$userRights;;;$UserSid)"
    )
    $arguments = [object[]]@(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $name,
        $false,
        $security
    )
    $eventAclType = "System.Threading.EventWaitHandleAcl" -as [type]
    if ($null -ne $eventAclType) {
        $method = $eventAclType.GetMethods() | Where-Object {
            $_.Name -eq "Create" -and $_.GetParameters().Count -eq 5
        } | Select-Object -First 1
        $event = $method.Invoke($null, $arguments)
    }
    else {
        $constructor = [Threading.EventWaitHandle].GetConstructors() | Where-Object {
            $_.GetParameters().Count -eq 5
        } | Select-Object -First 1
        $event = $constructor.Invoke($arguments)
    }
    if (-not [bool]$arguments[3]) {
        $event.Dispose()
        throw "Could not create a unique tray sentinel-owner event."
    }
    return [pscustomobject]@{ event = $event; name = $name }
}

function Start-BoundlessTrayQuiescenceSentinelOwner {
    param(
        [string]$UserSid,
        [int]$SessionId
    )

    $sentinelName = Get-BoundlessTrayQuiescenceSentinelName `
        -UserSid $UserSid `
        -SessionId $SessionId
    $ready = New-BoundlessSentinelOwnerEvent `
        -Prefix "Boundless.Tray.SentinelOwnerReady.v1" `
        -UserSid $UserSid
    $release = New-BoundlessSentinelOwnerEvent `
        -Prefix "Boundless.Tray.SentinelOwnerRelease.v1" `
        -UserSid $UserSid `
        -UserAccess "Synchronize"
    $parent = [Diagnostics.Process]::GetCurrentProcess()
    $payload = [ordered]@{
        sentinel_name = $sentinelName
        ready_event_name = $ready.name
        release_event_name = $release.name
        user_sid = $UserSid
        parent_process_id = $parent.Id
        parent_start_ticks = $parent.StartTime.ToUniversalTime().Ticks
    }
    $payloadBase64 = [Convert]::ToBase64String(
        [Text.Encoding]::UTF8.GetBytes(($payload | ConvertTo-Json -Compress))
    )
    $source = @'
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$payload = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD__")
) | ConvertFrom-Json
$parent = Get-Process -Id ([int]$payload.parent_process_id) -ErrorAction Stop
if ($parent.StartTime.ToUniversalTime().Ticks -ne [int64]$payload.parent_start_ticks) { exit 41 }
$security = [Security.AccessControl.MutexSecurity]::new()
$security.SetSecurityDescriptorSddlForm(
    "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;$($payload.user_sid))"
)
$arguments = [object[]]@($true, [string]$payload.sentinel_name, $false, $security)
$aclType = "System.Threading.MutexAcl" -as [type]
if ($null -ne $aclType) {
    $method = $aclType.GetMethods() | Where-Object {
        $_.Name -eq "Create" -and $_.GetParameters().Count -eq 4
    } | Select-Object -First 1
    $sentinel = $method.Invoke($null, $arguments)
}
else {
    $constructor = [Threading.Mutex].GetConstructors() | Where-Object {
        $_.GetParameters().Count -eq 4
    } | Select-Object -First 1
    $sentinel = $constructor.Invoke($arguments)
}
if (-not [bool]$arguments[2]) { exit 42 }
$ready = [Threading.EventWaitHandle]::OpenExisting([string]$payload.ready_event_name)
$releaseRights = [Security.AccessControl.EventWaitHandleRights]::Synchronize
$eventAclType = "System.Threading.EventWaitHandleAcl" -as [type]
if ($null -ne $eventAclType) {
    $openMethod = $eventAclType.GetMethods() | Where-Object {
        $_.Name -eq "OpenExisting" -and $_.GetParameters().Count -eq 2
    } | Select-Object -First 1
    $release = $openMethod.Invoke(
        $null,
        [object[]]@([string]$payload.release_event_name, $releaseRights)
    )
}
else {
    $release = [Threading.EventWaitHandle]::OpenExisting(
        [string]$payload.release_event_name,
        $releaseRights
    )
}
[void]$ready.Set()
while ($true) {
    if ($release.WaitOne(50)) {
        $sentinel.ReleaseMutex()
        $sentinel.Dispose()
        $ready.Dispose()
        $release.Dispose()
        $parent.Dispose()
        exit 0
    }
    if ($parent.HasExited) {
        [Environment]::Exit(43)
    }
}
'@.Replace("__PAYLOAD__", $payloadBase64)
    $process = $null
    try {
        $process = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($source))
            ) `
            -WindowStyle Hidden `
            -PassThru
        $deadline = (Get-Date).AddSeconds(10)
        while (-not $ready.event.WaitOne(50)) {
            if ($process.HasExited -or (Get-Date) -ge $deadline) {
                $exitDetail = if ($process.HasExited) { $process.ExitCode } else { "running" }
                throw "Tray quiescence sentinel owner did not publish its owned mutex; exit=$exitDetail."
            }
        }
        return [pscustomobject]@{
            process = $process
            ready_event = $ready.event
            release_event = $release.event
            sentinel_name = $sentinelName
        }
    }
    catch {
        if ($null -ne $process) {
            if (-not $process.HasExited) { Stop-BoundlessProcessBoundary -Process $process }
            $process.Dispose()
        }
        $ready.event.Dispose()
        $release.event.Dispose()
        throw
    }
}

function Stop-BoundlessTrayQuiescenceSentinelOwner {
    param(
        [object]$Owner,
        [switch]$Abandon
    )

    if ($null -eq $Owner) { return }
    try {
        if (-not $Owner.process.HasExited) {
            if ($Abandon) {
                Stop-BoundlessProcessBoundary -Process $Owner.process -TimeoutMilliseconds 5000
            }
            else {
                [void]$Owner.release_event.Set()
                if (-not $Owner.process.WaitForExit(5000)) {
                    Stop-BoundlessProcessBoundary -Process $Owner.process -TimeoutMilliseconds 5000
                    throw "Tray quiescence sentinel owner did not release normally."
                }
            }
        }
    }
    finally {
        $Owner.ready_event.Dispose()
        $Owner.release_event.Dispose()
        $Owner.process.Dispose()
    }
}

function Enter-BoundlessTrayQuiescence {
    param(
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId,
        [int]$TimeoutSeconds = 15
    )

    $mutexNameArgs = @{
        UserSid = $ExpectedOwnerSid
        SessionId = $ExpectedSessionId
    }
    $mutexName = Get-BoundlessTrayOwnerMutexName @mutexNameArgs
    $sentinelOwner = Start-BoundlessTrayQuiescenceSentinelOwner `
        -UserSid $ExpectedOwnerSid `
        -SessionId $ExpectedSessionId
    $sentinelName = $sentinelOwner.sentinel_name
    $treeJobName = "Local\Boundless.Installer.Tree.v1.$([guid]::NewGuid().ToString('N'))"
    $phaseInstanceId = [guid]::NewGuid().ToString('N')
    $completion = $null
    $serviceInitialRunning = $null
    $msiMayHaveStarted = $null
    $msiDefinitiveCompletion = $null
    $msiIdleProven = $null
    $monitor = $null
    $ownerLease = $null
    $completed = $false
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $attempts = 0
    try {
        $completion = New-BoundlessInstallerCompletionEvent -UserSid $ExpectedOwnerSid
        $serviceInitialRunning = New-BoundlessInstallerPhaseEvent `
            -UserSid $ExpectedOwnerSid `
            -Phase "ServiceInitialRunning" `
            -InstanceId $phaseInstanceId
        $msiMayHaveStarted = New-BoundlessInstallerPhaseEvent `
            -UserSid $ExpectedOwnerSid `
            -Phase "MsiMayHaveStarted" `
            -InstanceId $phaseInstanceId
        $msiDefinitiveCompletion = New-BoundlessInstallerPhaseEvent `
            -UserSid $ExpectedOwnerSid `
            -Phase "MsiDefinitiveCompletion" `
            -InstanceId $phaseInstanceId
        $msiIdleProven = New-BoundlessInstallerPhaseEvent `
            -UserSid $ExpectedOwnerSid `
            -Phase "MsiIdleProven" `
            -InstanceId $phaseInstanceId
        $monitor = Start-BoundlessTrayQuiescenceMonitor `
            -ExpectedOwnerSid $ExpectedOwnerSid `
            -ExpectedSessionId $ExpectedSessionId `
            -SentinelName $sentinelName `
            -TreeJobName $treeJobName `
            -CompletionEventName $completion.name `
            -MsiMayHaveStartedEventName $msiMayHaveStarted.name `
            -MsiDefinitiveCompletionEventName $msiDefinitiveCompletion.name `
            -MsiIdleProvenEventName $msiIdleProven.name
        do {
            $attempts += 1
            $shutdownArgs = @{
                ExpectedOwnerSid = $ExpectedOwnerSid
                ExpectedSessionId = $ExpectedSessionId
                TimeoutSeconds = [Math]::Max(1, [int](($deadline - (Get-Date)).TotalSeconds))
            }
            $shutdown = Stop-BoundlessTrayForUpgrade @shutdownArgs
            $leaseArgs = @{
                Name = $mutexName
                UserSid = $ExpectedOwnerSid
                InitiallyOwned = $true
            }
            $leaseAttempt = New-BoundlessNamedMutex @leaseArgs
            if ($leaseAttempt.created_new) {
                $ownerLease = $leaseAttempt.mutex
                break
            }

            $leaseAttempt.mutex.Dispose()
            Start-Sleep -Milliseconds 50
        } while ((Get-Date) -lt $deadline)
        if ($null -eq $ownerLease) {
            throw "Could not acquire the Boundless tray quiescence lease within $($TimeoutSeconds)s. The UAC/MSI phase was not started."
        }
        $remainingSeconds = [Math]::Max(1, [int](($deadline - (Get-Date)).TotalSeconds))
        Wait-BoundlessTrayQuiescenceMonitorReady `
            -Monitor $monitor `
            -TimeoutSeconds $remainingSeconds
        $completed = $true
        return [pscustomobject]@{
            mutex = $ownerLease
            sentinel_mutex = $null
            sentinel_owner = $sentinelOwner
            monitor = $monitor
            completion_event = $completion.event
            completion_event_name = $completion.name
            service_initial_running_event = $serviceInitialRunning.event
            service_initial_running_event_name = $serviceInitialRunning.name
            msi_may_have_started_event = $msiMayHaveStarted.event
            msi_may_have_started_event_name = $msiMayHaveStarted.name
            msi_definitive_completion_event = $msiDefinitiveCompletion.event
            msi_definitive_completion_event_name = $msiDefinitiveCompletion.name
            msi_idle_proven_event = $msiIdleProven.event
            msi_idle_proven_event_name = $msiIdleProven.name
            installer_transaction_mutex_name = "Global\_MSIExecute"
            tree_job_name = $treeJobName
            expected_owner_sid = $ExpectedOwnerSid
            expected_session_id = $ExpectedSessionId
            elevated_process = $null
            evidence = [pscustomobject]@{
                name = $mutexName
                sentinel_name = $sentinelName
                sentinel_acquired = $true
                acquired = $true
                attempts = $attempts
                shutdown = $shutdown
                integrity = "creator_default"
                monitor_process_id = $monitor.process.Id
                monitor_ready = $true
                monitor_stable_milliseconds = $monitor.stable_milliseconds
                monitor_completed = $false
                monitor_exit_code = $null
                spans_elevation_and_msi = $true
                installer_tree_closed = $false
                installer_completion_state = "not_started"
                msi_may_have_started = $false
                msi_definitive_completion = $false
                msi_transaction_idle_proven = $false
                quiescence_abandoned_to_monitor = $false
                quiescence_guardian_process_id = $null
                elevated_wrapper_hard_kill_used = $false
                parent_service_recovery_reconciled = $false
                parent_service_recovery_status = ""
                recovery_authority_drained = $true
                recovery_action_settled = $true
                recovery_authority_job_name = ""
            }
        }
    }
    finally {
        if (-not $completed) {
            $monitorExitedEarly = $null -ne $monitor -and $monitor.process.HasExited
            try {
                Stop-BoundlessTrayQuiescenceSentinelOwner -Owner $sentinelOwner
                if ($null -ne $monitor) {
                    Complete-BoundlessTrayQuiescenceMonitor `
                        -Monitor $monitor `
                        -ExitedBeforeSentinelRelease $monitorExitedEarly | Out-Null
                }
            }
            finally {
                if ($null -ne $ownerLease) {
                    try { $ownerLease.ReleaseMutex() } finally { $ownerLease.Dispose() }
                }
                if ($null -ne $completion) {
                    $completion.event.Dispose()
                }
                foreach ($phase in @(
                        $serviceInitialRunning,
                        $msiMayHaveStarted,
                        $msiDefinitiveCompletion,
                        $msiIdleProven
                    )) {
                    if ($null -ne $phase) { $phase.event.Dispose() }
                }
            }
        }
    }
}

function Exit-BoundlessTrayQuiescence {
    param([object]$Lease)

    if ($null -eq $Lease -or $null -eq $Lease.mutex) {
        return
    }
    $monitorExitedEarly = $Lease.monitor.process.HasExited
    try {
        if ($null -ne $Lease.PSObject.Properties["sentinel_owner"]) {
            Stop-BoundlessTrayQuiescenceSentinelOwner -Owner $Lease.sentinel_owner
        }
        elseif ($null -ne $Lease.sentinel_mutex) {
            try { $Lease.sentinel_mutex.ReleaseMutex() }
            finally { $Lease.sentinel_mutex.Dispose() }
        }
        $monitorResult = Complete-BoundlessTrayQuiescenceMonitor `
            -Monitor $Lease.monitor `
            -ExitedBeforeSentinelRelease $monitorExitedEarly
    }
    finally {
        try { $Lease.mutex.ReleaseMutex() } finally { $Lease.mutex.Dispose() }
        $Lease.completion_event.Dispose()
        Dispose-BoundlessInstallerPhaseEvidence -Lease $Lease
    }
    $Lease.evidence.monitor_completed = $monitorResult.completed
    $Lease.evidence.monitor_exit_code = $monitorResult.exit_code
    return $monitorResult
}

function Hold-BoundlessSynchronousTrayQuiescenceFailClosed {
    param(
        [object]$Lease,
        [string]$Reason
    )

    Write-Warning "$Reason Retaining tray quiescence fail-closed."
    while ($true) {
        try {
            Stop-BoundlessTrayForUpgrade `
                -ExpectedOwnerSid $Lease.expected_owner_sid `
                -ExpectedSessionId $Lease.expected_session_id `
                -TimeoutSeconds 2 | Out-Null
        }
        catch { }
        Start-Sleep -Milliseconds 250
    }
}

function Stop-BoundlessTrayQuiescenceMonitorProcess {
    param([object]$Monitor)

    if ($null -eq $Monitor) { return }
    try {
        if (-not $Monitor.process.HasExited) {
            Stop-BoundlessProcessBoundary -Process $Monitor.process -TimeoutMilliseconds 5000
        }
    }
    finally {
        $Monitor.handoff_event.Dispose()
        $Monitor.heartbeat_event.Dispose()
        $Monitor.ready_event.Dispose()
        $Monitor.process.Dispose()
    }
}

function Close-BoundlessTrayQuiescenceMonitorHandles {
    param([object]$Monitor)

    if ($null -eq $Monitor) { return }
    $Monitor.handoff_event.Dispose()
    $Monitor.heartbeat_event.Dispose()
    $Monitor.ready_event.Dispose()
    $Monitor.process.Dispose()
}

function Start-BoundlessTrayQuiescenceTakeoverMonitor {
    param(
        [object]$Lease,
        [bool]$WaitForReady = $true
    )

    while ($true) {
        $monitor = $null
        try {
            $monitor = Start-BoundlessTrayQuiescenceMonitor `
                -ExpectedOwnerSid $Lease.expected_owner_sid `
                -ExpectedSessionId $Lease.expected_session_id `
                -SentinelName $Lease.evidence.sentinel_name `
                -TreeJobName $Lease.tree_job_name `
                -CompletionEventName $Lease.completion_event_name `
                -MsiMayHaveStartedEventName $Lease.msi_may_have_started_event_name `
                -MsiDefinitiveCompletionEventName $Lease.msi_definitive_completion_event_name `
                -MsiIdleProvenEventName $Lease.msi_idle_proven_event_name `
                -InstallerTransactionMutexName $Lease.installer_transaction_mutex_name
            if ($WaitForReady) {
                Wait-BoundlessTrayQuiescenceMonitorReady -Monitor $monitor -TimeoutSeconds 10
            }
            else {
                $deadline = (Get-Date).AddSeconds(10)
                while (-not $monitor.heartbeat_event.WaitOne(50)) {
                    if ($monitor.process.HasExited -or (Get-Date) -ge $deadline) {
                        $exitDetail = if ($monitor.process.HasExited) {
                            $monitor.process.ExitCode
                        }
                        else {
                            "running"
                        }
                        throw "Tray quiescence takeover monitor did not open the sentinel; exit=$exitDetail."
                    }
                }
                [void]$monitor.heartbeat_event.Reset()
            }
            return $monitor
        }
        catch {
            if ($null -ne $monitor) {
                Stop-BoundlessTrayQuiescenceMonitorProcess -Monitor $monitor
            }
            Write-Warning "Could not pre-arm an independent tray quiescence takeover monitor: $($_.Exception.Message)"
            try {
                Stop-BoundlessTrayForUpgrade `
                    -ExpectedOwnerSid $Lease.expected_owner_sid `
                    -ExpectedSessionId $Lease.expected_session_id `
                    -TimeoutSeconds 2 | Out-Null
            }
            catch { }
            Start-Sleep -Milliseconds 250
        }
    }
}

function Wait-BoundlessRecoveryAuthorityDrainProof {
    param(
        [string]$JobName,
        [int]$TimeoutMilliseconds = 15000,
        [scriptblock]$ActiveProcessProbe = $null
    )

    if ($JobName -notmatch '^Local\\Boundless\.Installer\.RecoveryAuthority\.v1\.[0-9a-f]{32}$') {
        return $false
    }
    Initialize-BoundlessProcessTreeNativeMethods
    $deadline = (Get-Date).AddMilliseconds($TimeoutMilliseconds)
    do {
        try {
            $active = if ($null -ne $ActiveProcessProbe) {
                [int](& $ActiveProcessProbe $JobName)
            }
            else {
                [BoundlessProcessTreeNativeMethods]::GetNamedJobActiveProcessCount(
                    $JobName
                )
            }
            if ($active -le 0) { return $true }
        }
        catch { }
        Start-Sleep -Milliseconds 50
    } while ((Get-Date) -lt $deadline)
    return $false
}

function Resolve-BoundlessUnconfirmedTreeAndQuiescence {
    param(
        [object]$Lease,
        [scriptblock]$RecoveryAuthorityActiveProcessProbe = $null,
        [int]$RecoveryAuthorityDrainTimeoutMilliseconds = 15000,
        [scriptblock]$FailClosedAction = $null
    )

    if ($null -eq $Lease -or $null -eq $Lease.mutex) {
        return
    }
    $recoveryActionSettled = (
        $null -eq $Lease.evidence.PSObject.Properties["recovery_action_settled"] -or
        [bool]$Lease.evidence.recovery_action_settled
    )
    if (-not $recoveryActionSettled) {
        $reason = "Privileged recovery SCM action settlement remained unproven."
        if ($null -ne $FailClosedAction) {
            & $FailClosedAction $Lease $reason
            return
        }
        Hold-BoundlessSynchronousTrayQuiescenceFailClosed `
            -Lease $Lease `
            -Reason $reason
    }
    $recoveryAuthorityDrained = (
        $null -eq $Lease.evidence.PSObject.Properties["recovery_authority_drained"] -or
        [bool]$Lease.evidence.recovery_authority_drained
    )
    if (-not $recoveryAuthorityDrained) {
        $jobName = if (
            $null -ne $Lease.evidence.PSObject.Properties["recovery_authority_job_name"]
        ) {
            [string]$Lease.evidence.recovery_authority_job_name
        }
        else { "" }
        $drained = Wait-BoundlessRecoveryAuthorityDrainProof `
            -JobName $jobName `
            -TimeoutMilliseconds $RecoveryAuthorityDrainTimeoutMilliseconds `
            -ActiveProcessProbe $RecoveryAuthorityActiveProcessProbe
        if (-not $drained) {
            $reason = "Privileged recovery authority drain remained unproven."
            if ($null -ne $FailClosedAction) {
                & $FailClosedAction $Lease $reason
                return
            }
            Hold-BoundlessSynchronousTrayQuiescenceFailClosed `
                -Lease $Lease `
                -Reason $reason
        }
        $Lease.evidence.recovery_authority_drained = $true
    }
    $monitorAvailable = -not $Lease.monitor.process.HasExited
    $hasTransferMetadata = (
        $null -ne $Lease.PSObject.Properties["sentinel_owner"] -and
        $null -ne $Lease.sentinel_owner -and
        $null -ne $Lease.PSObject.Properties["completion_event_name"] -and
        $null -ne $Lease.PSObject.Properties["msi_may_have_started_event_name"] -and
        $null -ne $Lease.PSObject.Properties["msi_definitive_completion_event_name"] -and
        $null -ne $Lease.PSObject.Properties["msi_idle_proven_event_name"] -and
        $null -ne $Lease.PSObject.Properties["installer_transaction_mutex_name"] -and
        $null -ne $Lease.PSObject.Properties["expected_owner_sid"] -and
        $null -ne $Lease.PSObject.Properties["expected_session_id"] -and
        $null -ne $Lease.evidence.PSObject.Properties["sentinel_name"]
    )
    if ($hasTransferMetadata) {
        # The independent sentinel owner remains alive while the old monitor is
        # removed and a fresh monitor proves that it opened the sentinel. Only
        # then may the owner be abandoned. After that point this function never
        # falls back to process-local quiescence: a fresh independent monitor is
        # always armed before a stalled predecessor is stopped.
        Stop-BoundlessTrayQuiescenceMonitorProcess -Monitor $Lease.monitor
        $Lease.monitor = Start-BoundlessTrayQuiescenceTakeoverMonitor `
            -Lease $Lease `
            -WaitForReady $true
        $sentinelKeepAlive = [Threading.Mutex]::OpenExisting(
            [string]$Lease.evidence.sentinel_name
        )
        try {
            Stop-BoundlessTrayQuiescenceSentinelOwner `
                -Owner $Lease.sentinel_owner `
                -Abandon
            $Lease.sentinel_owner = $null

            [void]$Lease.monitor.heartbeat_event.Reset()
            $lastHeartbeat = Get-Date
            while ($true) {
                if (
                    $Lease.monitor.handoff_event.WaitOne(0) -and
                    -not $Lease.monitor.process.HasExited
                ) {
                    break
                }
                if ($Lease.monitor.heartbeat_event.WaitOne(0)) {
                    [void]$Lease.monitor.heartbeat_event.Reset()
                    $lastHeartbeat = Get-Date
                }
                $monitorStalled = (
                    $Lease.monitor.process.HasExited -or
                    ((Get-Date) - $lastHeartbeat).TotalMilliseconds -ge 5000
                )
                if ($monitorStalled) {
                    $replacement = Start-BoundlessTrayQuiescenceTakeoverMonitor `
                        -Lease $Lease `
                        -WaitForReady $false
                    Stop-BoundlessTrayQuiescenceMonitorProcess -Monitor $Lease.monitor
                    $Lease.monitor = $replacement
                    $lastHeartbeat = Get-Date
                    continue
                }
                Start-Sleep -Milliseconds 25
            }

            $Lease.evidence.quiescence_guardian_process_id = $Lease.monitor.process.Id
            try { $Lease.mutex.ReleaseMutex() } finally { $Lease.mutex.Dispose() }
            $Lease.mutex = $null
            Close-BoundlessTrayQuiescenceMonitorHandles -Monitor $Lease.monitor
            $Lease.completion_event.Dispose()
            Dispose-BoundlessInstallerPhaseEvidence -Lease $Lease
            if ($null -ne $Lease.PSObject.Properties["elevated_process"] -and
                $null -ne $Lease.elevated_process) {
                $Lease.elevated_process.Dispose()
                $Lease.elevated_process = $null
            }
            $Lease.evidence.quiescence_abandoned_to_monitor = $true
            return
        }
        finally {
            $sentinelKeepAlive.Dispose()
        }
    }
    elseif ($null -ne $Lease.sentinel_mutex) {
        # Fixture-only direct ownership cannot be transferred while this thread
        # remains alive, so it must use the synchronous fail-closed path below.
        $monitorAvailable = $false
    }

    # Without an acknowledged monitor handoff, keep the owner mutex held and
    # synchronously stop the elevated root, then wait until its private job is
    # empty or absent. A same-SID query-handle pin can prolong this wait, but it
    # cannot make an active installer tree look drained.
    try {
        if ($null -ne $Lease.PSObject.Properties["elevated_process"] -and
            $null -ne $Lease.elevated_process) {
            if (-not $Lease.elevated_process.HasExited) {
                Stop-BoundlessProcessBoundary `
                    -Process $Lease.elevated_process `
                    -TimeoutMilliseconds 5000
            }
        }
        Initialize-BoundlessProcessTreeNativeMethods
        while ($true) {
            $active = [BoundlessProcessTreeNativeMethods]::GetNamedJobActiveProcessCount(
                $Lease.tree_job_name
            )
            if ($active -le 0) {
                break
            }
            Start-Sleep -Milliseconds 50
        }
    }
    catch {
        Hold-BoundlessSynchronousTrayQuiescenceFailClosed `
            -Lease $Lease `
            -Reason "Installer process-tree closure could not be proved: $($_.Exception.Message)"
    }
    try {
        $hasPhaseEvidence = (
            $null -ne $Lease.PSObject.Properties["msi_may_have_started_event"] -and
            $null -ne $Lease.PSObject.Properties["msi_definitive_completion_event"] -and
            $null -ne $Lease.PSObject.Properties["msi_idle_proven_event"]
        )
        if ($hasPhaseEvidence) {
            $completionState = Update-BoundlessInstallerPhaseEvidence -Lease $Lease
            if (
                $completionState -eq "uncertain" -and
                -not $Lease.evidence.msi_transaction_idle_proven
            ) {
                $idleProven = Wait-BoundlessWindowsInstallerTransactionIdleProof `
                    -TimeoutMilliseconds 15000
                if ($idleProven) {
                    [void]$Lease.msi_idle_proven_event.Set()
                    [void](Update-BoundlessInstallerPhaseEvidence -Lease $Lease)
                }
                else {
                    Hold-BoundlessSynchronousTrayQuiescenceFailClosed `
                        -Lease $Lease `
                        -Reason "Windows Installer transaction idle remained unproven."
                }
            }
        }
    }
    catch {
        Hold-BoundlessSynchronousTrayQuiescenceFailClosed `
            -Lease $Lease `
            -Reason "Installer completion evidence could not be resolved: $($_.Exception.Message)"
    }
    try {
        if ($null -ne $Lease.PSObject.Properties["sentinel_owner"] -and
            $null -ne $Lease.sentinel_owner) {
            Stop-BoundlessTrayQuiescenceSentinelOwner -Owner $Lease.sentinel_owner
            $Lease.sentinel_owner = $null
        }
    }
    catch {
        Hold-BoundlessSynchronousTrayQuiescenceFailClosed `
            -Lease $Lease `
            -Reason "Tray quiescence sentinel could not be released safely: $($_.Exception.Message)"
    }
    try { $Lease.mutex.ReleaseMutex() } finally { $Lease.mutex.Dispose() }
    if ($null -ne $Lease.sentinel_mutex) {
        try { $Lease.sentinel_mutex.ReleaseMutex() }
        finally { $Lease.sentinel_mutex.Dispose() }
    }
    $Lease.monitor.handoff_event.Dispose()
    $Lease.monitor.heartbeat_event.Dispose()
    $Lease.monitor.ready_event.Dispose()
    $Lease.monitor.process.Dispose()
    $Lease.completion_event.Dispose()
    Dispose-BoundlessInstallerPhaseEvidence -Lease $Lease
    if ($null -ne $Lease.PSObject.Properties["elevated_process"] -and
        $null -ne $Lease.elevated_process) {
        $Lease.elevated_process.Dispose()
        $Lease.elevated_process = $null
    }
    $Lease.evidence.installer_tree_closed = $true
}

function Get-BoundlessAdminOnlyStageSddl {
    # A protected DACL is sufficient because the stage is created atomically
    # below a machine-owned known folder. Avoid a mandatory-label SACL here:
    # setting one during directory creation requires a privilege that a normal
    # split-token administrator does not receive merely by accepting UAC.
    return "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
}

function New-BoundlessSecuredDirectoryAtomic {
    param(
        [string]$Path,
        [Security.AccessControl.DirectorySecurity]$Security
    )

    $create = [IO.Directory].GetMethods() |
        Where-Object {
            $parameters = $_.GetParameters()
            $_.Name -eq "CreateDirectory" -and $parameters.Count -eq 2 -and
                $parameters[0].ParameterType -eq [string] -and
                $parameters[1].ParameterType -eq [Security.AccessControl.DirectorySecurity]
        } | Select-Object -First 1
    $invokeArguments = [Array]::CreateInstance([object], 2)
    if ($null -ne $create) {
        # MethodInfo.Invoke does not unwrap PowerShell's PSObject wrappers from
        # an object[] literal under either Windows PowerShell 5.1 or pwsh.
        $invokeArguments.SetValue($Path.PSObject.BaseObject, 0)
        $invokeArguments.SetValue($Security.PSObject.BaseObject, 1)
    }
    else {
        $aclType = "System.IO.FileSystemAclExtensions" -as [type]
        if ($null -eq $aclType) {
            throw "No secured directory creation API is available."
        }
        $create = $aclType.GetMethods() |
            Where-Object {
                $parameters = $_.GetParameters()
                $_.Name -eq "CreateDirectory" -and $parameters.Count -eq 2 -and
                    $parameters[0].ParameterType -eq [Security.AccessControl.DirectorySecurity] -and
                    $parameters[1].ParameterType -eq [string]
            } | Select-Object -First 1
        if ($null -eq $create) {
            throw "No FileSystemAclExtensions.CreateDirectory API is available."
        }
        $invokeArguments.SetValue($Security.PSObject.BaseObject, 0)
        $invokeArguments.SetValue($Path.PSObject.BaseObject, 1)
    }

    $null = $create.Invoke($null, $invokeArguments)
    return Get-Item -LiteralPath $Path -Force -ErrorAction Stop
}

function Get-BoundlessProgramDataRoot {
    $path = [Environment]::GetFolderPath(
        [Environment+SpecialFolder]::CommonApplicationData
    )
    if ([string]::IsNullOrWhiteSpace($path)) {
        throw "Could not resolve the Windows CommonApplicationData known folder."
    }
    return [IO.Path]::GetFullPath($path).TrimEnd('\')
}

function Wait-BoundlessInstallerTreeBoundaryClosed {
    param(
        [Diagnostics.Process]$InstallerProcess,
        [Threading.EventWaitHandle]$CompletionEvent,
        [string]$TreeJobName,
        [int]$TimeoutMilliseconds = 10000
    )

    Initialize-BoundlessProcessTreeNativeMethods
    $deadline = (Get-Date).AddMilliseconds($TimeoutMilliseconds)
    do {
        if ($CompletionEvent.WaitOne(0)) {
            return
        }
        $active = [BoundlessProcessTreeNativeMethods]::GetNamedJobActiveProcessCount($TreeJobName)
        if ($InstallerProcess.HasExited -and $active -lt 0) {
            # A named kill-on-close job is destroyed only after its associated
            # processes terminate. Missing after the elevated root exits is
            # therefore the crash-safe completion proof.
            return
        }
        Start-Sleep -Milliseconds 25
    } while ((Get-Date) -lt $deadline)
    $active = [BoundlessProcessTreeNativeMethods]::GetNamedJobActiveProcessCount($TreeJobName)
    throw "Elevated installer process tree did not close within $TimeoutMilliseconds ms; active=$active."
}

function Invoke-BoundlessRecoveryLauncherBounded {
    param(
        [string]$LauncherSource,
        [int]$TimeoutMilliseconds,
        [object]$RecoveryAuthority,
        [int]$SettlementTimeoutMilliseconds = 35000
    )

    $launcher = $null
    $launcherFailure = $null
    $synchronization = $null
    $synchronizationFailure = $null
    $closeFailure = $null
    try {
        try {
            $launcher = Start-BoundlessOwnedProcessBoundary `
                -FilePath (Resolve-CurrentPowerShellExecutable) `
                -ArgumentList @(
                    "-NoProfile",
                    "-EncodedCommand",
                    [Convert]::ToBase64String(
                        [Text.Encoding]::Unicode.GetBytes($LauncherSource)
                    )
                ) `
                -CreateNoWindow
            if (-not $launcher.WaitForExit($TimeoutMilliseconds)) {
                throw "Parent service recovery elevation launch/execution exceeded $TimeoutMilliseconds milliseconds."
            }
            if (-not $launcher.WaitForTreeExit(5000)) {
                throw "Parent service recovery launcher left an owned descendant."
            }
            if ($launcher.ExitCode -ne 0) {
                throw "Parent service recovery launcher failed with exit code $($launcher.ExitCode)."
            }
            if ($RecoveryAuthority.job.ActiveProcessCount -gt 0) {
                throw "Parent service recovery launcher exited before its privileged child drained."
            }
        }
        catch {
            $launcherFailure = $_
        }

        if ($null -ne $launcherFailure) {
            try {
                $synchronization = Revoke-BoundlessRecoveryAuthorityAndSynchronizeAction `
                    -Authority $RecoveryAuthority `
                    -SettlementTimeoutMilliseconds $SettlementTimeoutMilliseconds
            }
            catch {
                $synchronizationFailure = $_
            }
        }
        else {
            # A successful launcher exit plus an empty authority job proves the
            # privileged helper finished its bounded SCM settlement and cannot
            # issue a later service mutation.
            $RecoveryAuthority.action_settled = $true
        }

        try {
            Close-BoundlessRecoveryAuthority `
                -Authority $RecoveryAuthority `
                -Revoke:($null -ne $launcherFailure) `
                -ActionFenceOwned:(
                    $null -ne $synchronization -and
                    [bool]$synchronization.fence_owned
                )
        }
        catch {
            $closeFailure = $_
        }

        if ($null -ne $synchronizationFailure) {
            throw (
                "$($launcherFailure.Exception.Message) Recovery action-fence " +
                "synchronization also failed: " +
                $synchronizationFailure.Exception.Message
            )
        }
        if ($null -ne $closeFailure) {
            $prefix = if ($null -ne $launcherFailure) {
                "$($launcherFailure.Exception.Message) "
            }
            else { "" }
            throw (
                $prefix +
                "Recovery authority drain also failed: " +
                $closeFailure.Exception.Message
            )
        }
        if (
            $null -ne $launcherFailure -and
            -not [bool]$synchronization.action_committed
        ) {
            throw $launcherFailure
        }
        return [pscustomobject]@{
            launcher_completed = $null -eq $launcherFailure
            launcher_failure = if ($null -ne $launcherFailure) {
                $launcherFailure.Exception.Message
            }
            else { "" }
            action_committed = (
                $null -ne $synchronization -and
                [bool]$synchronization.action_committed
            )
            action_fence_synchronized = (
                $null -eq $launcherFailure -or
                (
                    $null -ne $synchronization -and
                    [bool]$synchronization.fence_owned
                )
            )
            authority_drained = [bool]$RecoveryAuthority.drained
            action_settled = [bool]$RecoveryAuthority.action_settled
        }
    }
    finally {
        if ($null -ne $launcher) {
            if ($launcher.ActiveProcessCount -gt 0) {
                Stop-BoundlessProcessBoundary -Process $launcher -TimeoutMilliseconds 5000
            }
            $launcher.Dispose()
        }
    }
}

function Restore-BoundlessServiceAfterHardKilledElevatedInstall {
    param(
        [object]$QuiescenceLease,
        [string]$StagedHelperPath,
        [int]$TimeoutMilliseconds = 60000,
        [int]$ActionSettlementTimeoutMilliseconds = 35000,
        [string]$FixtureLauncherSource = "",
        [scriptblock]$BeforeFixtureLauncherAction = $null,
        [scriptblock]$ServiceStatusProbe = $null
    )

    if (
        $null -eq $QuiescenceLease.service_initial_running_event -or
        $null -eq $QuiescenceLease.msi_may_have_started_event -or
        $null -eq $QuiescenceLease.msi_definitive_completion_event -or
        $null -eq $QuiescenceLease.msi_idle_proven_event -or
        $null -eq $QuiescenceLease.PSObject.Properties["expected_owner_sid"]
    ) {
        throw "Parent service recovery did not receive protected installer phase evidence."
    }
    if (-not $QuiescenceLease.service_initial_running_event.WaitOne(0)) {
        return [pscustomobject]@{
            required = $false
            status = "original_service_not_running_or_starting"
        }
    }
    $msiMayHaveStarted = $QuiescenceLease.msi_may_have_started_event.WaitOne(0)
    $msiDefinitive = $QuiescenceLease.msi_definitive_completion_event.WaitOne(0)
    $msiIdleProven = $QuiescenceLease.msi_idle_proven_event.WaitOne(0)
    $requiresMsiIdleProof = $msiMayHaveStarted -and -not $msiDefinitive -and -not $msiIdleProven

    if ($null -eq $ServiceStatusProbe) {
        $ServiceStatusProbe = {
            Get-BoundlessServiceStatusBounded -TimeoutSeconds 2
        }
    }
    $statusBeforeRecovery = & $ServiceStatusProbe
    if ($statusBeforeRecovery -in @("Running", "StartPending")) {
        return [pscustomobject]@{
            required = $true
            status = "already_running_or_starting"
        }
    }
    if ($statusBeforeRecovery -notin @("Stopped", "StopPending")) {
        throw "Parent service recovery found ineligible BoundlessService state $statusBeforeRecovery."
    }

    $recoveryAuthority = New-BoundlessRecoveryAuthority `
        -UserSid $QuiescenceLease.expected_owner_sid
    if ($null -ne $QuiescenceLease.PSObject.Properties["evidence"]) {
        $QuiescenceLease.evidence | Add-Member `
            -NotePropertyName recovery_authority_drained `
            -NotePropertyValue $false `
            -Force
        $QuiescenceLease.evidence | Add-Member `
            -NotePropertyName recovery_action_settled `
            -NotePropertyValue $false `
            -Force
        $QuiescenceLease.evidence | Add-Member `
            -NotePropertyName recovery_authority_job_name `
            -NotePropertyValue $recoveryAuthority.job_name `
            -Force
    }
    $authorityTransferred = $false
    $recoveryLaunch = $null
    try {
    $launcherSource = if (-not [string]::IsNullOrWhiteSpace($FixtureLauncherSource)) {
        $FixtureLauncherSource
    }
    else {
        $recoverySwitch = if ($requiresMsiIdleProof) {
            "-ElevatedBootstrapMsiIdleServiceRecovery"
        }
        else {
            "-ElevatedBootstrapServiceRecovery"
        }
        $arguments = @(
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            $StagedHelperPath,
            $recoverySwitch,
            "-ElevatedInstallServiceInitialRunningEvent",
            $QuiescenceLease.service_initial_running_event_name,
            "-ElevatedInstallMsiMayHaveStartedEvent",
            $QuiescenceLease.msi_may_have_started_event_name,
            "-ElevatedInstallMsiDefinitiveCompletionEvent",
            $QuiescenceLease.msi_definitive_completion_event_name,
            "-ElevatedInstallMsiIdleProvenEvent",
            $QuiescenceLease.msi_idle_proven_event_name,
            "-ElevatedBootstrapRecoveryJob",
            $recoveryAuthority.job_name,
            "-ElevatedBootstrapRecoveryRevocationEvent",
            $recoveryAuthority.revocation_event_name,
            "-ElevatedBootstrapRecoveryActionFence",
            $recoveryAuthority.action_fence_name,
            "-ElevatedBootstrapRecoveryActionCommittedEvent",
            $recoveryAuthority.action_committed_event_name
        )
        $payload = [ordered]@{
            file_path = Resolve-CurrentPowerShellExecutable
            argument_line = (@(
                $arguments | ForEach-Object { ConvertTo-ProcessArgument -Value $_ }
            ) -join " ")
            use_run_as = -not (Test-IsAdministrator)
        }
        $payloadBase64 = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes(($payload | ConvertTo-Json -Compress))
        )
        @'
$ErrorActionPreference = "Stop"
$payload = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD__")
) | ConvertFrom-Json
$process = $null
try {
    $start = @{
        FilePath = [string]$payload.file_path
        ArgumentList = [string]$payload.argument_line
        WindowStyle = "Hidden"
        PassThru = $true
    }
    if ([bool]$payload.use_run_as) { $start.Verb = "RunAs" }
    $process = Start-Process @start
    $process.WaitForExit()
    exit $process.ExitCode
}
catch {
    Write-Error $_
    exit 91
}
finally {
    if ($null -ne $process) { $process.Dispose() }
}
'@.Replace("__PAYLOAD__", $payloadBase64)
    }

    if ($null -ne $BeforeFixtureLauncherAction) {
        if ([string]::IsNullOrWhiteSpace($FixtureLauncherSource)) {
            throw "Before-launch recovery fixture hook is not available in production mode."
        }
        & $BeforeFixtureLauncherAction $recoveryAuthority
    }
    $launcherSource = $launcherSource.Replace(
        "__RECOVERY_JOB__",
        $recoveryAuthority.job_name
    ).Replace(
        "__RECOVERY_REVOCATION__",
        $recoveryAuthority.revocation_event_name
    ).Replace(
        "__RECOVERY_ACTION_FENCE__",
        $recoveryAuthority.action_fence_name
    ).Replace(
        "__RECOVERY_ACTION_COMMITTED__",
        $recoveryAuthority.action_committed_event_name
    )
    $authorityTransferred = $true
    $recoveryLaunch = Invoke-BoundlessRecoveryLauncherBounded `
        -LauncherSource $launcherSource `
        -TimeoutMilliseconds $TimeoutMilliseconds `
        -RecoveryAuthority $recoveryAuthority `
        -SettlementTimeoutMilliseconds $ActionSettlementTimeoutMilliseconds
    }
    finally {
        try {
            if (-not $authorityTransferred) {
                $setupSynchronization = $null
                $setupSynchronizationFailure = $null
                $setupCloseFailure = $null
                try {
                    $setupSynchronization = Revoke-BoundlessRecoveryAuthorityAndSynchronizeAction `
                        -Authority $recoveryAuthority `
                        -SettlementTimeoutMilliseconds $ActionSettlementTimeoutMilliseconds
                }
                catch {
                    $setupSynchronizationFailure = $_
                }
                try {
                    Close-BoundlessRecoveryAuthority `
                        -Authority $recoveryAuthority `
                        -Revoke `
                        -ActionFenceOwned:(
                            $null -ne $setupSynchronization -and
                            [bool]$setupSynchronization.fence_owned
                        )
                }
                catch {
                    $setupCloseFailure = $_
                }
                if ($null -ne $setupSynchronizationFailure) {
                    throw (
                        "Recovery setup action-fence synchronization failed: " +
                        $setupSynchronizationFailure.Exception.Message
                    )
                }
                if ($null -ne $setupCloseFailure) {
                    throw (
                        "Recovery setup authority drain failed: " +
                        $setupCloseFailure.Exception.Message
                    )
                }
            }
        }
        finally {
            if ($null -ne $QuiescenceLease.PSObject.Properties["evidence"]) {
                $QuiescenceLease.evidence.recovery_authority_drained = (
                    [bool]$recoveryAuthority.drained
                )
                $QuiescenceLease.evidence.recovery_action_settled = (
                    [bool]$recoveryAuthority.action_settled
                )
            }
        }
    }
    if (
        $requiresMsiIdleProof -and
        -not $QuiescenceLease.msi_definitive_completion_event.WaitOne(0) -and
        -not $QuiescenceLease.msi_idle_proven_event.WaitOne(0)
    ) {
        throw "Parent service recovery did not publish a definitive or authoritative MSI-idle boundary before restart."
    }
    $statusAfterRecovery = if (
        $null -ne $recoveryLaunch -and
        $recoveryLaunch.action_committed
    ) {
        Wait-BoundlessServiceTransition `
            -DesiredStatus "Running" `
            -Worker $null `
            -StatusProbe $ServiceStatusProbe `
            -TimeoutSeconds ([Math]::Max(
                1,
                [int][Math]::Ceiling($ActionSettlementTimeoutMilliseconds / 1000.0)
            )) `
            -FailurePrefix "Parent service recovery committed-start reconciliation"
    }
    else {
        & $ServiceStatusProbe
    }
    if ($statusAfterRecovery -notin @("Running", "StartPending")) {
        throw "Parent service recovery process exited successfully but BoundlessService remained $statusAfterRecovery."
    }
    return [pscustomobject]@{
        required = $true
        status = if ($requiresMsiIdleProof) { "restored_after_msi_boundary" } else { "restored" }
    }
}

function Wait-BoundlessElevatedInstallSupervised {
    param(
        [Diagnostics.Process]$InstallerProcess,
        [object]$Monitor,
        [Threading.EventWaitHandle]$CancellationEvent,
        [Threading.EventWaitHandle]$CompletionEvent,
        [string]$TreeJobName,
        [object]$TreeClosureState,
        [scriptblock]$HardKillRecoveryAction = $null,
        [int]$TimeoutSeconds = 900,
        [int]$CancellationGraceMilliseconds = 30000,
        [int]$HeartbeatTimeoutMilliseconds = 5000
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $lastHeartbeat = Get-Date
    while (-not $InstallerProcess.WaitForExit(50)) {
        if ($Monitor.heartbeat_event.WaitOne(0)) {
            [void]$Monitor.heartbeat_event.Reset()
            $lastHeartbeat = Get-Date
        }
        $failure = ""
        if ($Monitor.process.HasExited) {
            $failure = "Tray quiescence monitor exited during the elevated install; exit=$($Monitor.process.ExitCode)."
        }
        elseif (((Get-Date) - $lastHeartbeat).TotalMilliseconds -ge $HeartbeatTimeoutMilliseconds) {
            $failure = "Tray quiescence monitor heartbeat stalled during the elevated install."
        }
        elseif ((Get-Date) -ge $deadline) {
            $failure = "Elevated Boundless install exceeded the bounded $TimeoutSeconds second window."
        }
        if ([string]::IsNullOrWhiteSpace($failure)) {
            continue
        }

        if (-not $CancellationEvent.Set()) {
            throw "$failure Installer cancellation signaling failed."
        }
        $hardKillUsed = $false
        if (-not $InstallerProcess.WaitForExit($CancellationGraceMilliseconds)) {
            $hardKillUsed = $true
            $TreeClosureState | Add-Member `
                -NotePropertyName hard_kill_used `
                -NotePropertyValue $true `
                -Force
            Stop-BoundlessProcessBoundary `
                -Process $InstallerProcess `
                -TimeoutMilliseconds 5000
        }
        Wait-BoundlessInstallerTreeBoundaryClosed `
            -InstallerProcess $InstallerProcess `
            -CompletionEvent $CompletionEvent `
            -TreeJobName $TreeJobName
        $TreeClosureState.confirmed = $true
        if ($hardKillUsed -and $null -ne $HardKillRecoveryAction) {
            try {
                $recovery = & $HardKillRecoveryAction
                $TreeClosureState | Add-Member `
                    -NotePropertyName parent_service_recovery_reconciled `
                    -NotePropertyValue $true `
                    -Force
                $TreeClosureState | Add-Member `
                    -NotePropertyName parent_service_recovery_status `
                    -NotePropertyValue ([string]$recovery.status) `
                    -Force
            }
            catch {
                $recoveryFailure = $_
                $TreeClosureState | Add-Member `
                    -NotePropertyName parent_service_recovery_reconciled `
                    -NotePropertyValue $false `
                    -Force
                $TreeClosureState | Add-Member `
                    -NotePropertyName parent_service_recovery_status `
                    -NotePropertyValue "failed" `
                    -Force
                throw (
                    "$failure The staged installer process boundary was canceled. " +
                    "Parent service recovery after the hard kill also failed: " +
                    $recoveryFailure.Exception.Message
                )
            }
        }
        throw "$failure The staged installer process boundary was canceled."
    }
    Wait-BoundlessInstallerTreeBoundaryClosed `
        -InstallerProcess $InstallerProcess `
        -CompletionEvent $CompletionEvent `
        -TreeJobName $TreeJobName
    $TreeClosureState.confirmed = $true
    return $InstallerProcess.ExitCode
}

function Get-BoundlessLogHandoffSddl {
    param([string]$UserSid)

    Assert-AllowedUserSid -Sid $UserSid
    return "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;$UserSid)"
}

function Get-BoundlessLogHandoffFileSddl {
    param([string]$UserSid)

    Assert-AllowedUserSid -Sid $UserSid
    return "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;$UserSid)"
}

function Get-BoundlessOwnedTreeSddl {
    param([string]$UserSid)

    Assert-AllowedUserSid -Sid $UserSid
    # JOB_OBJECT_QUERY only: the unelevated monitor can prove drain without
    # receiving assign, terminate, or ACL mutation rights.
    return "D:P(A;;RC;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00000004;;;$UserSid)"
}

function New-BoundlessRecoveryAuthority {
    param([string]$UserSid)

    Assert-AllowedUserSid -Sid $UserSid
    Initialize-BoundlessRecoveryAuthorityNativeMethods
    $authorityId = [guid]::NewGuid().ToString('N')
    $jobName = "Local\Boundless.Installer.RecoveryAuthority.v1.$authorityId"
    $actionFenceName = "Local\Boundless.Installer.RecoveryAction.v1.$authorityId"
    $revocation = New-BoundlessSentinelOwnerEvent `
        -Prefix "Boundless.Installer.RecoveryRevoked.v1" `
        -UserSid $UserSid
    $actionFence = $null
    $actionCommitted = $null
    $job = $null
    try {
        $actionFence = New-BoundlessNamedMutex `
            -Name $actionFenceName `
            -UserSid $UserSid `
            -InitiallyOwned $false
        if (-not $actionFence.created_new) {
            throw "Recovery action fence unexpectedly already existed."
        }
        $actionCommitted = New-BoundlessSentinelOwnerEvent `
            -Prefix "Boundless.Installer.RecoveryActionCommitted.v1" `
            -UserSid $UserSid
        $job = [BoundlessRecoveryAuthorityNativeMethodsV1]::Create(
            $jobName,
            "D:P(A;;RC;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;$UserSid)"
        )
        return [pscustomobject]@{
            job = $job
            job_name = $jobName
            revocation_event = $revocation.event
            revocation_event_name = $revocation.name
            action_fence = $actionFence.mutex
            action_fence_name = $actionFenceName
            action_committed_event = $actionCommitted.event
            action_committed_event_name = $actionCommitted.name
            drained = $false
            action_settled = $false
        }
    }
    catch {
        if ($null -ne $job) { $job.Dispose() }
        if ($null -ne $actionCommitted) { $actionCommitted.event.Dispose() }
        if ($null -ne $actionFence) { $actionFence.mutex.Dispose() }
        $revocation.event.Dispose()
        throw
    }
}

function Join-BoundlessRecoveryAuthority {
    param(
        [string]$JobName,
        [string]$RevocationEventName,
        [string]$ActionFenceName,
        [string]$ActionCommittedEventName
    )

    if ($JobName -notmatch '^Local\\Boundless\.Installer\.RecoveryAuthority\.v1\.[0-9a-f]{32}$') {
        throw "Recovery authority job name was invalid."
    }
    if ($RevocationEventName -notmatch '^Local\\Boundless\.Installer\.RecoveryRevoked\.v1\.[0-9a-f]{32}$') {
        throw "Recovery revocation event name was invalid."
    }
    if ($ActionFenceName -notmatch '^Local\\Boundless\.Installer\.RecoveryAction\.v1\.[0-9a-f]{32}$') {
        throw "Recovery action fence name was invalid."
    }
    if ($ActionCommittedEventName -notmatch '^Local\\Boundless\.Installer\.RecoveryActionCommitted\.v1\.[0-9a-f]{32}$') {
        throw "Recovery action committed event name was invalid."
    }
    $revocation = [Threading.EventWaitHandle]::OpenExisting($RevocationEventName)
    $actionFence = $null
    $actionCommitted = $null
    try {
        if ($revocation.WaitOne(0)) { throw "Recovery authority was revoked before admission." }
        $actionFence = [Threading.Mutex]::OpenExisting($ActionFenceName)
        $actionCommitted = [Threading.EventWaitHandle]::OpenExisting(
            $ActionCommittedEventName
        )
        Initialize-BoundlessRecoveryAuthorityNativeMethods
        [BoundlessRecoveryAuthorityNativeMethodsV1]::Join($JobName)
        if ($revocation.WaitOne(0)) { throw "Recovery authority was revoked during admission." }
        return [pscustomobject]@{
            revocation_event = $revocation
            action_fence = $actionFence
            action_committed_event = $actionCommitted
        }
    }
    catch {
        if ($null -ne $actionCommitted) { $actionCommitted.Dispose() }
        if ($null -ne $actionFence) { $actionFence.Dispose() }
        $revocation.Dispose()
        throw
    }
}

function Revoke-BoundlessRecoveryAuthorityAndSynchronizeAction {
    param(
        [object]$Authority,
        [int]$SettlementTimeoutMilliseconds = 35000
    )

    if (-not $Authority.revocation_event.Set()) {
        throw "Could not revoke parent service recovery authority."
    }
    $fenceOwned = $false
    $fenceAbandoned = $false
    try {
        try {
            $fenceOwned = $Authority.action_fence.WaitOne(
                $SettlementTimeoutMilliseconds
            )
        }
        catch [Threading.AbandonedMutexException] {
            $fenceOwned = $true
            $fenceAbandoned = $true
        }
        if (-not $fenceOwned) {
            throw "Recovery action fence did not settle within $SettlementTimeoutMilliseconds milliseconds."
        }
        if ($fenceAbandoned) {
            throw "Recovery action fence was abandoned before SCM mutation settlement was proved."
        }
        $Authority.action_settled = $true
        return [pscustomobject]@{
            fence_owned = $true
            fence_abandoned = $false
            action_committed = $Authority.action_committed_event.WaitOne(0)
        }
    }
    catch {
        if ($fenceOwned) {
            try { $Authority.action_fence.ReleaseMutex() } catch { }
        }
        throw
    }
}

function Close-BoundlessRecoveryAuthority {
    param(
        [object]$Authority,
        [switch]$Revoke,
        [switch]$ActionFenceOwned,
        [int]$DrainTimeoutMilliseconds = 5000,
        [scriptblock]$DrainProof = $null
    )

    if ($null -eq $Authority) { return }
    $Authority.drained = $false
    try {
        if ($Revoke) {
            [void]$Authority.revocation_event.Set()
            if ($Authority.job.ActiveProcessCount -gt 0) {
                $Authority.job.Terminate(1)
            }
        }
        elseif ($Authority.job.ActiveProcessCount -gt 0) {
            throw "Recovery authority still had an active privileged process at successful completion."
        }
        $drained = if ($null -ne $DrainProof) {
            [bool](& $DrainProof $Authority $DrainTimeoutMilliseconds)
        }
        else {
            $Authority.job.WaitForEmpty($DrainTimeoutMilliseconds)
        }
        if (-not $drained) {
            throw "Recovery authority job did not drain within $DrainTimeoutMilliseconds milliseconds."
        }
        $Authority.drained = $true
    }
    finally {
        if ($ActionFenceOwned) {
            try { $Authority.action_fence.ReleaseMutex() } catch { }
        }
        $Authority.job.Dispose()
        $Authority.revocation_event.Dispose()
        $Authority.action_committed_event.Dispose()
        $Authority.action_fence.Dispose()
    }
}

function Close-BoundlessRecoveryAuthorityClient {
    param([object]$Authority)

    if ($null -eq $Authority) { return }
    $Authority.action_committed_event.Dispose()
    $Authority.action_fence.Dispose()
    $Authority.revocation_event.Dispose()
}

function Test-BoundlessInstallerStagePath {
    param(
        [string]$Path,
        [string]$ProgramDataRoot = ""
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $false
    }
    if ([string]::IsNullOrWhiteSpace($ProgramDataRoot)) {
        $ProgramDataRoot = Get-BoundlessProgramDataRoot
    }
    $fullPath = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $fullProgramData = [IO.Path]::GetFullPath($ProgramDataRoot).TrimEnd('\')
    $parent = [IO.Directory]::GetParent($fullPath)
    if ($null -eq $parent -or -not $parent.FullName.Equals(
        $fullProgramData,
        [StringComparison]::OrdinalIgnoreCase
    )) {
        return $false
    }
    return [IO.Path]::GetFileName($fullPath) -match '^BoundlessInstaller-[0-9a-f]{32}$'
}

function Get-BoundlessInstallerSourcePackageName {
    param([string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw "Installer source package path was empty."
    }

    $leafName = [IO.Path]::GetFileName($Path)
    if (
        [string]::IsNullOrWhiteSpace($leafName) -or
        $leafName.Length -le 4 -or
        -not [IO.Path]::GetExtension($leafName).Equals(
            ".msi",
            [StringComparison]::OrdinalIgnoreCase
        ) -or
        $leafName.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0
    ) {
        throw "Installer source package name was not a safe MSI leaf name."
    }

    return $leafName
}

function Copy-BoundlessInstallerLogHandoff {
    param(
        [string]$StageRoot,
        [string]$StagedLogPath,
        [string]$DestinationPath,
        [string]$ProgramDataRoot = ""
    )

    if ([string]::IsNullOrWhiteSpace($DestinationPath)) {
        return [pscustomobject]@{ requested = $false; copied = $false; destination = "" }
    }
    if ([string]::IsNullOrWhiteSpace($ProgramDataRoot)) {
        $ProgramDataRoot = Get-BoundlessProgramDataRoot
    }
    if (-not (Test-BoundlessInstallerStagePath -Path $StageRoot -ProgramDataRoot $ProgramDataRoot)) {
        throw "Refusing installer log handoff from an unsafe stage boundary."
    }
    if (-not (Test-Path -LiteralPath $StageRoot -PathType Container)) {
        return [pscustomobject]@{
            requested = $true
            copied = $false
            destination = $DestinationPath
            reason = "not_produced"
        }
    }
    $resolvedStage = (Resolve-Path -LiteralPath $StageRoot -ErrorAction Stop).Path
    $stageItem = Get-Item -LiteralPath $resolvedStage -Force -ErrorAction Stop
    if (($stageItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Refusing installer log handoff from a reparse-point stage."
    }
    $expectedLogPath = Join-Path $resolvedStage "Boundless-install.log"
    if (-not (Test-WindowsPathEqual -Left $StagedLogPath -Right $expectedLogPath)) {
        throw "Refusing installer log handoff from an unexpected path."
    }
    if (-not (Test-Path -LiteralPath $expectedLogPath -PathType Leaf)) {
        return [pscustomobject]@{
            requested = $true
            copied = $false
            destination = $DestinationPath
            reason = "not_produced"
        }
    }
    $entries = @(Get-ChildItem -LiteralPath $resolvedStage -Force -ErrorAction Stop)
    if ($entries.Count -ne 1 -or $entries[0].Name -ne "Boundless-install.log") {
        throw "Refusing installer log handoff because the completed stage contained unexpected entries."
    }
    if (($entries[0].Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Refusing installer log handoff from a reparse-point file."
    }

    $resolvedDestination = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath(
        $DestinationPath
    )
    $destinationParent = Split-Path -Parent $resolvedDestination
    if (-not [string]::IsNullOrWhiteSpace($destinationParent)) {
        New-Item -ItemType Directory -Path $destinationParent -Force -ErrorAction Stop | Out-Null
    }
    Copy-Item `
        -LiteralPath $expectedLogPath `
        -Destination $resolvedDestination `
        -Force `
        -ErrorAction Stop
    $sourceHash = (Get-FileHash -LiteralPath $expectedLogPath -Algorithm SHA256).Hash
    $destinationHash = (Get-FileHash -LiteralPath $resolvedDestination -Algorithm SHA256).Hash
    if (-not $sourceHash.Equals($destinationHash, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Installer log handoff copy hash did not match the completed staged log."
    }

    Remove-Item -LiteralPath $expectedLogPath -Force -ErrorAction Stop
    Remove-Item -LiteralPath $resolvedStage -Force -ErrorAction Stop
    return [pscustomobject]@{
        requested = $true
        copied = $true
        destination = $resolvedDestination
        sha256 = $destinationHash
    }
}

function Get-BoundlessElevatedInstallErrorFromLog {
    param([string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path) -or -not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return ""
    }
    $matches = [regex]::Matches(
        [IO.File]::ReadAllText($Path),
        '(?m)^BE=(?<value>\S+)\r?$'
    )
    if ($matches.Count -eq 0) {
        return ""
    }
    return [uri]::UnescapeDataString($matches[$matches.Count - 1].Groups["value"].Value)
}

function Assert-BoundlessAdminOnlyAcl {
    param(
        [string]$Path,
        [bool]$RequireProtected = $false
    )

    $acl = Get-Acl -LiteralPath $Path -ErrorAction Stop
    if ($RequireProtected -and -not $acl.AreAccessRulesProtected) {
        throw "Installer staging ACL inherited permissions: $Path"
    }

    $allowedSids = @("S-1-5-18", "S-1-5-32-544")
    $observedAllowedSids = @()
    foreach ($rule in @($acl.Access)) {
        $sid = $rule.IdentityReference.Translate(
            [Security.Principal.SecurityIdentifier]
        ).Value
        if ($rule.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow) {
            if ($sid -notin $allowedSids) {
                throw "Installer staging ACL granted access to unexpected SID $sid at $Path"
            }
            $observedAllowedSids += $sid
        }
    }
    foreach ($requiredSid in $allowedSids) {
        if ($requiredSid -notin $observedAllowedSids) {
            throw "Installer staging ACL omitted required SID $requiredSid at $Path"
        }
    }
    return $acl
}

function New-BoundlessStagingProbeCommand {
    param(
        [string]$ProbeParent,
        [string]$SourcePath,
        [string]$UserSid
    )

    $stageLeaf = "BoundlessInstaller-$([guid]::NewGuid().ToString('N'))"
    $payload = [ordered]@{
        stage_parent = $ProbeParent
        stage_leaf = $stageLeaf
        source_path = $SourcePath
        source_sha256 = (Get-FileHash -LiteralPath $SourcePath -Algorithm SHA256).Hash
        user_sid = $UserSid
        stage_sddl = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;$UserSid)"
    }
    $payloadJson = $payload | ConvertTo-Json -Compress
    $payloadBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($payloadJson))
    $source = @'
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
function New-BoundlessSecuredDirectoryAtomic {
__SECURED_DIRECTORY_FUNCTION__
}
function Assert-ProbeAcl {
    param([string]$Path, [string[]]$ExpectedSids)
    $acl = Get-Acl -LiteralPath $Path -ErrorAction Stop
    if (-not $acl.AreAccessRulesProtected) {
        throw "Staging probe inherited an ACL."
    }
    $observed = @()
    foreach ($rule in @($acl.Access)) {
        $sid = $rule.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
        if ($rule.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow) {
            if ($sid -notin $ExpectedSids) {
                throw "Staging probe granted an unexpected principal."
            }
            $observed += $sid
        }
    }
    foreach ($sid in $ExpectedSids) {
        if ($sid -notin $observed) {
            throw "Staging probe omitted a required principal."
        }
    }
}
$payloadJson = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD_BASE64__")
)
$payload = $payloadJson | ConvertFrom-Json
$parent = (Resolve-Path -LiteralPath $payload.stage_parent -ErrorAction Stop).Path.TrimEnd('\')
$stageRoot = Join-Path $parent $payload.stage_leaf
$trustedStage = $false
try {
    if (
        [IO.Directory]::GetParent([IO.Path]::GetFullPath($stageRoot)).FullName -ne $parent -or
        [IO.Path]::GetFileName($stageRoot) -notmatch '^BoundlessInstaller-[0-9a-f]{32}$'
    ) {
        throw "Staging probe received an unsafe boundary."
    }
    $security = [Security.AccessControl.DirectorySecurity]::new()
    $security.SetSecurityDescriptorSddlForm([string]$payload.stage_sddl)
    $item = New-BoundlessSecuredDirectoryAtomic -Path $stageRoot -Security $security
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Staging probe created a reparse point."
    }
    $probeSids = @("S-1-5-18", "S-1-5-32-544", [string]$payload.user_sid)
    Assert-ProbeAcl -Path $stageRoot -ExpectedSids $probeSids
    $trustedStage = $true

    $stagedCopy = Join-Path $stageRoot "probe.bin"
    Copy-Item -LiteralPath $payload.source_path -Destination $stagedCopy -ErrorAction Stop
    if ((Get-FileHash -LiteralPath $stagedCopy -Algorithm SHA256).Hash -ne $payload.source_sha256) {
        throw "Staging probe copy hash did not match."
    }
}
finally {
    if ($trustedStage -and (Test-Path -LiteralPath $stageRoot)) {
        $resolved = (Resolve-Path -LiteralPath $stageRoot).Path
        $resolvedParent = [IO.Directory]::GetParent($resolved)
        $leaf = [IO.Path]::GetFileName($resolved)
        $item = Get-Item -LiteralPath $resolved -Force
        if (
            $null -eq $resolvedParent -or
            -not $resolvedParent.FullName.Equals($parent, [StringComparison]::OrdinalIgnoreCase) -or
            $leaf -notmatch '^BoundlessInstaller-[0-9a-f]{32}$' -or
            ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
        ) {
            throw "Staging probe refused an unsafe cleanup boundary."
        }
        Assert-ProbeAcl -Path $resolved -ExpectedSids $probeSids
        Remove-Item -LiteralPath $resolved -Recurse -Force -ErrorAction Stop
    }
}
if (Test-Path -LiteralPath $stageRoot) {
    throw "Staging probe did not clean its stage."
}
Write-Output "boundless_staging_child_probe=passed"
'@
    $securedDirectoryFunction = (
        Get-Command New-BoundlessSecuredDirectoryAtomic -CommandType Function -ErrorAction Stop
    ).Definition
    $source = $source.Replace("__SECURED_DIRECTORY_FUNCTION__", $securedDirectoryFunction)
    $source = $source.Replace("__PAYLOAD_BASE64__", $payloadBase64)
    $encodedCommand = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($source))
    if ($encodedCommand.Length -gt 30000) {
        throw "The staging child-process probe exceeded the safe Windows command-line budget."
    }
    return [pscustomobject]@{
        encoded_command = $encodedCommand
        stage_path = Join-Path $ProbeParent $stageLeaf
    }
}

function Invoke-BoundlessStagingChildProbes {
    param([string]$SourcePath)

    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
    $probeParent = Join-Path $tempRoot (
        "BoundlessStagingProbe-$([guid]::NewGuid().ToString('N'))"
    )
    $userSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $testedHosts = @()
    try {
        New-Item -ItemType Directory -Path $probeParent -ErrorAction Stop | Out-Null
        foreach ($hostName in @("powershell.exe", "pwsh.exe")) {
            $hostCommand = Get-Command $hostName -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($null -eq $hostCommand) {
                continue
            }
            $probe = New-BoundlessStagingProbeCommand `
                -ProbeParent $probeParent `
                -SourcePath $SourcePath `
                -UserSid $userSid
            $probeProcessArgs = @{
                FilePath = $hostCommand.Source
                ArgumentList = @("-NoProfile", "-EncodedCommand", $probe.encoded_command)
                TimeoutSeconds = 20
            }
            if ($hostName -eq "powershell.exe") {
                # pwsh prepends its own modules to the inherited PSModulePath.
                # Windows PowerShell can then find the pwsh Security manifest
                # first and fail to load Get-Acl. Restore Desktop-edition paths
                # for this cross-host executable probe.
                $userWindowsModules = Join-Path (
                    [Environment]::GetFolderPath([Environment+SpecialFolder]::MyDocuments)
                ) "WindowsPowerShell\Modules"
                $machineWindowsModules = @(
                    [Environment]::GetEnvironmentVariable("PSModulePath", "Machine") -split ';' |
                        Where-Object { $_ -match '(?i)\\WindowsPowerShell\\' }
                )
                $probeProcessArgs.EnvironmentVariables = @{
                    PSModulePath = (@($userWindowsModules) + $machineWindowsModules) -join ';'
                }
            }
            try {
                $result = Invoke-BoundedProcess @probeProcessArgs
            }
            catch {
                throw "Could not launch staging child-process probe under $hostName at '$($hostCommand.Source)'. $($_.Exception.Message)"
            }
            if (
                $result.exit_code -ne 0 -or
                $result.stdout -notmatch 'boundless_staging_child_probe=passed' -or
                (Test-Path -LiteralPath $probe.stage_path)
            ) {
                throw "Staging child-process probe failed under $hostName. exit=$($result.exit_code) stdout='$($result.stdout)' stderr='$($result.stderr)'"
            }
            $testedHosts += $hostName
        }
        if ($testedHosts.Count -eq 0) {
            throw "No PowerShell host was available for the staging child-process probe."
        }
        return @($testedHosts)
    }
    finally {
        if (Test-Path -LiteralPath $probeParent) {
            $resolved = (Resolve-Path -LiteralPath $probeParent).Path
            $parent = [IO.Directory]::GetParent($resolved)
            $leaf = [IO.Path]::GetFileName($resolved)
            if (
                $null -eq $parent -or
                -not $parent.FullName.TrimEnd('\').Equals($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or
                $leaf -notmatch '^BoundlessStagingProbe-[0-9a-f]{32}$'
            ) {
                throw "Refusing unsafe staging probe cleanup: $resolved"
            }
            Remove-Item -LiteralPath $resolved -Recurse -Force -ErrorAction Stop
        }
    }
}

function Assert-ElevatedInstallResult {
    param([object]$Result)

    if ($null -eq $Result -or $Result.status -ne "passed") {
        $detail = if ($null -ne $Result -and $Result.PSObject.Properties.Match("error").Count -gt 0) {
            $Result.error
        }
        else {
            "elevated install result was missing or malformed"
        }
        throw "Elevated Boundless install failed: $detail"
    }
    if ($Result.msi_exit_code -notin @(0, 3010)) {
        throw "Elevated Boundless install returned unexpected MSI exit code $($Result.msi_exit_code)."
    }
    $inputInjectorShutdownProperty = $Result.PSObject.Properties.Match("input_injector_shutdown")
    if ($inputInjectorShutdownProperty.Count -ne 1) {
        throw "Elevated Boundless install result omitted the input_injector_shutdown field."
    }
    $inputInjectorShutdown = $inputInjectorShutdownProperty[0].Value
    if ($null -ne $inputInjectorShutdown) {
        foreach ($member in @("initial_count", "elapsed_milliseconds", "force_kill_used")) {
            if ($inputInjectorShutdown.PSObject.Properties.Match($member).Count -ne 1) {
                throw "Elevated Boundless install result input_injector_shutdown omitted '$member'."
            }
        }
    }
    if ($Result.service_shutdown.force_kill_used) {
        throw "Elevated Boundless install reported a forbidden service force-kill."
    }
    if (
        $null -eq $Result.installer_stage -or
        -not $Result.installer_stage.admin_only -or
        -not $Result.installer_stage.hash_verified
    ) {
        throw "Elevated Boundless install did not prove an admin-only hash-verified MSI stage."
    }
    return $Result
}

function Invoke-BoundlessMsiWithServiceRecovery {
    param(
        [object]$ServiceShutdown,
        [scriptblock]$MsiAction,
        [scriptblock]$RestartAction
    )

    try {
        return & $MsiAction
    }
    catch {
        $originalError = $_
        $completionState = [string]$originalError.Exception.Data["BoundlessMsiCompletionState"]
        if (
            $ServiceShutdown.initial_status -in @("Running", "StartPending") -and
            $completionState -in @("definitive_failure", "not_started")
        ) {
            try {
                $recovery = & $RestartAction
                $originalError.Exception.Data["BoundlessServiceRecovery"] = (
                    "start_requested=$($recovery.start_requested);final_status=$($recovery.final_status)"
                )
            }
            catch {
                $originalError.Exception.Data["BoundlessServiceRecoveryError"] = $_.Exception.Message
                Write-Warning "BoundlessService recovery after MSI failure also failed: $($_.Exception.Message)"
            }
        }
        throw $originalError
    }
}

function Invoke-ElevatedInstallPhase {
    param(
        [string]$ResolvedInstallerPath,
        [string]$Sid,
        [string]$ExpectedInstallerSha256,
        [string]$CancellationEventName,
        [int]$CoordinatorProcessId,
        [long]$CoordinatorStartTicks,
        [string]$MonitorMutexName,
        [string]$ServiceInitialRunningEventName,
        [string]$MsiMayHaveStartedEventName,
        [string]$MsiDefinitiveCompletionEventName,
        [string]$MsiIdleProvenEventName,
        [int]$TimeoutSeconds
    )

    if (-not (Test-IsAdministrator)) {
        throw "Internal elevated install phase was not elevated."
    }

    $stageRoot = Split-Path -Parent $ResolvedInstallerPath
    Get-BoundlessInstallerSourcePackageName -Path $ResolvedInstallerPath | Out-Null
    if (-not (Test-BoundlessInstallerStagePath -Path $stageRoot)) {
        throw "Internal elevated install phase did not receive the expected immutable MSI stage."
    }
    Assert-BoundlessAdminOnlyAcl -Path $stageRoot -RequireProtected $true | Out-Null
    Assert-BoundlessAdminOnlyAcl -Path $ResolvedInstallerPath | Out-Null
    $stagedHash = (Get-FileHash -LiteralPath $ResolvedInstallerPath -Algorithm SHA256).Hash
    if (-not $stagedHash.Equals($ExpectedInstallerSha256, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Immutable staged MSI hash verification failed."
    }

    $serviceInitialRunning = Open-BoundlessInstallerPhaseEvent `
        -Name $ServiceInitialRunningEventName `
        -Phase "ServiceInitialRunning"
    $msiMayHaveStarted = Open-BoundlessInstallerPhaseEvent `
        -Name $MsiMayHaveStartedEventName `
        -Phase "MsiMayHaveStarted"
    $msiDefinitiveCompletion = Open-BoundlessInstallerPhaseEvent `
        -Name $MsiDefinitiveCompletionEventName `
        -Phase "MsiDefinitiveCompletion"
    $msiIdleProven = Open-BoundlessInstallerPhaseEvent `
        -Name $MsiIdleProvenEventName `
        -Phase "MsiIdleProven"
    try {
        # The request and status polling are bounded independently. A blocked
        # ServiceController call remains inside an owned child process tree that is
        # drained before this function can return or allow MSI to start.
        $inputInjectorShutdown = Stop-BoundlessInputInjectorBeforeMsi `
            -ExpectedOwnerSid $Sid `
            -ExpectedSessionId ([Diagnostics.Process]::GetCurrentProcess().SessionId)
        $serviceShutdown = Stop-BoundlessServiceBeforeMsi `
            -InitialRunningEvent $serviceInitialRunning
        $msiArgs = @{
            ResolvedInstallerPath = $ResolvedInstallerPath
            Sid = $Sid
            CancellationEventName = $CancellationEventName
            CoordinatorProcessId = $CoordinatorProcessId
            CoordinatorStartTicks = $CoordinatorStartTicks
            MonitorMutexName = $MonitorMutexName
            MsiMayHaveStartedEvent = $msiMayHaveStarted
            MsiDefinitiveCompletionEvent = $msiDefinitiveCompletion
            TimeoutSeconds = $TimeoutSeconds
        }
        $exitCode = Invoke-BoundlessMsiWithServiceRecovery `
            -ServiceShutdown $serviceShutdown `
            -MsiAction { Invoke-BoundlessMsiElevated @msiArgs } `
            -RestartAction { Start-BoundlessServiceAfterFailedInstall }

        return [pscustomobject]@{
            status = "passed"
            msi_exit_code = $exitCode
            service_shutdown = $serviceShutdown
            input_injector_shutdown = $inputInjectorShutdown
            installer_stage = [pscustomobject]@{
                admin_only = $true
                hash_verified = $true
                staged_copy_used = $true
                cleaned = $false
            }
        }
    }
    finally {
        $msiIdleProven.Dispose()
        $msiDefinitiveCompletion.Dispose()
        $msiMayHaveStarted.Dispose()
        $serviceInitialRunning.Dispose()
    }
}

function New-BoundlessElevatedInstallCommand {
    param(
        [string]$ResolvedInstallerPath,
        [string]$Sid,
        [object]$InstallerAnchor,
        [string]$CancellationEventName,
        [int]$CoordinatorProcessId,
        [long]$CoordinatorStartTicks,
        [string]$MonitorMutexName,
        [string]$TreeJobName,
        [string]$CompletionEventName,
        [string]$ServiceInitialRunningEventName,
        [string]$MsiMayHaveStartedEventName,
        [string]$MsiDefinitiveCompletionEventName,
        [string]$MsiIdleProvenEventName,
        [int]$TimeoutSeconds = 900,
        [bool]$LogRequested = (-not [string]::IsNullOrWhiteSpace($LogPath))
    )

    $helperAnchor = Assert-BoundlessHelperStartupAnchor
    $InstallerAnchor = Assert-BoundlessInstallerAnchor `
        -Anchor $InstallerAnchor `
        -ResolvedInstallerPath $ResolvedInstallerPath
    $resolvedHelperPath = $helperAnchor.path
    if ($CancellationEventName -notmatch '^Local\\Boundless\.Installer\.Cancel\.v1\.[0-9a-f]{32}$') {
        throw "Elevated install command received an invalid cancellation event."
    }
    if ($CoordinatorProcessId -le 0 -or $CoordinatorStartTicks -le 0) {
        throw "Elevated install command received an invalid coordinator identity."
    }
    if ($MonitorMutexName -notmatch '^Local\\Boundless\.Installer\.Monitor\.v1\.[0-9a-f]{32}$') {
        throw "Elevated install command received an invalid monitor liveness mutex."
    }
    if ($TreeJobName -notmatch '^Local\\Boundless\.Installer\.Tree\.v1\.[0-9a-f]{32}$') {
        throw "Elevated install command received an invalid process-tree job."
    }
    if ($CompletionEventName -notmatch '^Local\\Boundless\.Installer\.TreeComplete\.v1\.[0-9a-f]{32}$') {
        throw "Elevated install command received an invalid completion event."
    }
    $phaseInstanceIds = @()
    foreach ($phaseEvent in @(
            [pscustomobject]@{ name = $ServiceInitialRunningEventName; phase = "ServiceInitialRunning" },
            [pscustomobject]@{ name = $MsiMayHaveStartedEventName; phase = "MsiMayHaveStarted" },
            [pscustomobject]@{ name = $MsiDefinitiveCompletionEventName; phase = "MsiDefinitiveCompletion" },
            [pscustomobject]@{ name = $MsiIdleProvenEventName; phase = "MsiIdleProven" }
        )) {
        $prefix = [regex]::Escape((Get-BoundlessInstallerPhaseEventPrefix -Phase $phaseEvent.phase))
        if ($phaseEvent.name -notmatch "^Local\\$prefix\.[0-9a-f]{32}$") {
            throw "Elevated install command received an invalid $($phaseEvent.phase) event."
        }
        $phaseInstanceIds += ($phaseEvent.name -split '\.')[-1]
    }
    if (@($phaseInstanceIds | Select-Object -Unique).Count -ne 1) {
        throw "Elevated install command received phase events from different instances."
    }
    $stageLeaf = "BoundlessInstaller-$([guid]::NewGuid().ToString('N'))"
    $programData = Get-BoundlessProgramDataRoot
    $stageRoot = Join-Path $programData $stageLeaf
    $stagedLogPath = Join-Path $stageRoot "Boundless-install.log"
    # Validate before serializing the immutable in-memory payload. The elevated
    # bootstrap derives this same leaf from installer_path before staging it.
    $installerSourcePackageName = Get-BoundlessInstallerSourcePackageName `
        -Path $ResolvedInstallerPath
    $payload = [ordered]@{
        installer_path = $ResolvedInstallerPath
        installer_sha256 = $InstallerAnchor.sha256
        helper_path = $resolvedHelperPath
        helper_sha256 = $helperAnchor.sha256
        sid = $Sid
        quiet = [bool]$Quiet
        no_restart = [bool]$NoRestart
        log_requested = $LogRequested
        stage_leaf = $stageLeaf
        cancellation_event_name = $CancellationEventName
        coordinator_process_id = $CoordinatorProcessId
        coordinator_start_ticks = $CoordinatorStartTicks
        monitor_mutex_name = $MonitorMutexName
        tree_job_name = $TreeJobName
        completion_event_name = $CompletionEventName
        phase_instance_id = $phaseInstanceIds[0]
        install_timeout_seconds = $TimeoutSeconds
        stage_sddl = Get-BoundlessAdminOnlyStageSddl
        tree_job_sddl = Get-BoundlessOwnedTreeSddl -UserSid $Sid
        log_handoff_sddl = Get-BoundlessLogHandoffSddl -UserSid $Sid
        log_handoff_file_sddl = Get-BoundlessLogHandoffFileSddl -UserSid $Sid
    }
    $payloadJson = $payload | ConvertTo-Json -Compress
    $payloadBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($payloadJson))
    $source = @'
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
function New-BoundlessSecuredDirectoryAtomic {
__SECURED_DIRECTORY_FUNCTION__
}
function Assert-AdminAcl {
    param([string]$Path, [bool]$RequireProtected = $false)
    $acl = Get-Acl -LiteralPath $Path -ErrorAction Stop
    if ($RequireProtected -and -not $acl.AreAccessRulesProtected) {
        throw "Installer stage inherited permissions."
    }
    $required = @("S-1-5-18", "S-1-5-32-544")
    $observed = @()
    foreach ($rule in @($acl.Access)) {
        $sid = $rule.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
        if ($rule.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow) {
            if ($sid -notin $required) { throw "Installer stage granted unexpected access." }
            $observed += $sid
        }
    }
    foreach ($sid in $required) {
        if ($sid -notin $observed) { throw "Installer stage omitted a required principal." }
    }
}
function Quote-Argument {
    param([string]$Value)
    if ($Value -notmatch '[\s"]') { return $Value }
    return '"' + ($Value -replace '"', '\"') + '"'
}
Add-Type -TypeDefinition @"
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
public sealed class BoundlessElevatedJob : IDisposable {
 [StructLayout(LayoutKind.Sequential)] struct SA { public int n; public IntPtr sd; public bool inherit; }
 [StructLayout(LayoutKind.Sequential)] struct BL { public long a,b; public uint flags; public UIntPtr c,d; public uint e; public UIntPtr f; public uint g,h; }
 [StructLayout(LayoutKind.Sequential)] struct IO { public ulong a,b,c,d,e,f; }
 [StructLayout(LayoutKind.Sequential)] struct EL { public BL basic; public IO io; public UIntPtr a,b,c,d; }
 [StructLayout(LayoutKind.Sequential)] struct AC { public long a,b,c,d; public uint faults,total,active,ended; }
 [StructLayout(LayoutKind.Sequential)] struct SI { public int cb; public IntPtr a,b,c; public int d,e,f,g,h,i,j,k; public short l,m; public IntPtr n,o,p,q; }
 [StructLayout(LayoutKind.Sequential)] struct PI { public IntPtr process,thread; public int pid,tid; }
 [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(string s,uint r,out IntPtr p,IntPtr z);
 [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr p);
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateJobObjectW(ref SA a,string n);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool SetInformationJobObject(IntPtr j,int c,ref EL i,uint n);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool QueryInformationJobObject(IntPtr j,int c,out AC i,uint n,IntPtr r);
 [DllImport("kernel32.dll",SetLastError=true)] static extern IntPtr OpenProcess(uint a,bool i,int p);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool AssignProcessToJobObject(IntPtr j,IntPtr p);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool TerminateJobObject(IntPtr j,uint e);
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcessW(string a,StringBuilder c,IntPtr p,IntPtr t,bool i,uint f,IntPtr e,string d,ref SI s,out PI r);
 [DllImport("kernel32.dll",SetLastError=true)] static extern uint ResumeThread(IntPtr t);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool TerminateProcess(IntPtr p,uint e);
 [DllImport("kernel32.dll",SetLastError=true)] static extern uint WaitForSingleObject(IntPtr h,uint m);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr p,out uint e);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool CloseHandle(IntPtr h);
 IntPtr job,root;
 BoundlessElevatedJob(IntPtr h){job=h;}
 public static BoundlessElevatedJob Create(string name,string sddl){
  IntPtr sd=IntPtr.Zero,j=IntPtr.Zero;
  try{
   if(!ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl,1,out sd,IntPtr.Zero))throw new Win32Exception(Marshal.GetLastWin32Error());
   SA a=new SA();a.n=Marshal.SizeOf(typeof(SA));a.sd=sd;j=CreateJobObjectW(ref a,name);
   if(j==IntPtr.Zero)throw new Win32Exception(Marshal.GetLastWin32Error());
   if(Marshal.GetLastWin32Error()==183)throw new InvalidOperationException("owned process job already existed");
   EL l=new EL();l.basic.flags=0x2000;
   if(!SetInformationJobObject(j,9,ref l,(uint)Marshal.SizeOf(typeof(EL))))throw new Win32Exception(Marshal.GetLastWin32Error());
   BoundlessElevatedJob r=new BoundlessElevatedJob(j);j=IntPtr.Zero;return r;
  }finally{if(j!=IntPtr.Zero)CloseHandle(j);if(sd!=IntPtr.Zero)LocalFree(sd);}
 }
 public void Assign(int pid){IntPtr p=OpenProcess(0x101,false,pid);if(p==IntPtr.Zero)throw new Win32Exception(Marshal.GetLastWin32Error());try{if(!AssignProcessToJobObject(job,p))throw new Win32Exception(Marshal.GetLastWin32Error());}finally{CloseHandle(p);}}
 public Process StartOwned(string applicationName,string commandLine){
  if(root!=IntPtr.Zero)throw new InvalidOperationException();
  SI s=new SI();s.cb=Marshal.SizeOf(typeof(SI));PI p=new PI();bool resumed=false;
  if(!CreateProcessW(applicationName,new StringBuilder(commandLine),IntPtr.Zero,IntPtr.Zero,false,0x08000004,IntPtr.Zero,null,ref s,out p))throw new Win32Exception(Marshal.GetLastWin32Error());
  try{
   if(!AssignProcessToJobObject(job,p.process))throw new Win32Exception(Marshal.GetLastWin32Error(),"AssignProcessToJobObject failed");
   if(ResumeThread(p.thread)==0xFFFFFFFF)throw new Win32Exception(Marshal.GetLastWin32Error(),"ResumeThread failed");
   resumed=true;Process r=Process.GetProcessById(p.pid);root=p.process;p.process=IntPtr.Zero;return r;
  }catch(Exception original){
   if(!resumed){
    Exception cleanup=null;uint wait=WaitForSingleObject(p.process,0);
    if(wait==258){if(!TerminateProcess(p.process,1))cleanup=new Win32Exception(Marshal.GetLastWin32Error(),"TerminateProcess(unassigned staged helper) failed");else if(WaitForSingleObject(p.process,5000)!=0)cleanup=new InvalidOperationException("unassigned staged helper did not terminate");}
    else if(wait!=0)cleanup=new Win32Exception(Marshal.GetLastWin32Error(),"WaitForSingleObject(unassigned staged helper) failed");
    if(cleanup!=null)throw new InvalidOperationException("staged helper admission failed and cleanup was not proven",new AggregateException(original,cleanup));
   }
   throw;
  }
  finally{if(p.thread!=IntPtr.Zero)CloseHandle(p.thread);if(p.process!=IntPtr.Zero)CloseHandle(p.process);}
 }
 public int Active { get { AC a;if(!QueryInformationJobObject(job,1,out a,(uint)Marshal.SizeOf(typeof(AC)),IntPtr.Zero))throw new Win32Exception(Marshal.GetLastWin32Error());return (int)a.active; } }
 public int RootExitCode { get {uint e;if(root==IntPtr.Zero)throw new InvalidOperationException();if(!GetExitCodeProcess(root,out e))throw new Win32Exception(Marshal.GetLastWin32Error());return (int)e;} }
 public void Terminate(){if(Active>0&&!TerminateJobObject(job,1))throw new Win32Exception(Marshal.GetLastWin32Error());}
 public void Dispose(){if(job!=IntPtr.Zero){CloseHandle(job);job=IntPtr.Zero;}if(root!=IntPtr.Zero){CloseHandle(root);root=IntPtr.Zero;}GC.SuppressFinalize(this);}
 ~BoundlessElevatedJob(){Dispose();}
}
"@
function Get-CancellationReason {
    param([object]$Boundary)
    if ($Boundary.event.WaitOne(0)) { return "coordinator cancellation was signaled" }
    if ($Boundary.coordinator.HasExited) { return "coordinator process ended" }
    foreach ($probe in @(
            [pscustomobject]@{ name = "quiescence monitor"; mutex = $Boundary.monitor }
        )) {
        $acquired = $false
        try {
            $acquired = $probe.mutex.WaitOne(0)
            if ($acquired) { return "$($probe.name) ownership ended" }
        }
        catch [Threading.AbandonedMutexException] {
            $acquired = $true
            return "$($probe.name) was abandoned"
        }
        finally {
            if ($acquired) { try { $probe.mutex.ReleaseMutex() } catch { } }
        }
    }
    return ""
}
function Wait-JobEmpty {
    param([BoundlessElevatedJob]$Job, [int]$TimeoutMilliseconds)
    $deadline = (Get-Date).AddMilliseconds($TimeoutMilliseconds)
    while ($Job.Active -gt 0 -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 20
    }
    return $Job.Active -eq 0
}
function Restore-BootstrapServiceBeforeMsiFailure {
    param(
        [string]$TreeJobSddl,
        [string]$StagedHelperPath = "",
        [string]$ServiceInitialRunningEventName = "",
        [string]$MsiMayHaveStartedEventName = "",
        [string]$MsiDefinitiveCompletionEventName = "",
        [string]$MsiIdleProvenEventName = "",
        [ValidateSet("ServiceRecovery", "MsiIdleProof")]
        [string]$WorkerMode = "ServiceRecovery",
        [int]$TimeoutMilliseconds = 25000,
        [string]$FixtureWorkerSource = ""
    )
    $j = $null
    $p = $null
    try {
        $j = [BoundlessElevatedJob]::Create(
            "Local\Boundless.Installer.Recovery.v1.$([guid]::NewGuid().ToString('N'))",
            $TreeJobSddl
        )
        $hostPath = (Get-Process -Id $PID -ErrorAction Stop).Path
        $a = if (-not [string]::IsNullOrWhiteSpace($FixtureWorkerSource)) {
            @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($FixtureWorkerSource))
            )
        }
        else {
            $workerSwitch = if ($WorkerMode -eq "ServiceRecovery") {
                "-ElevatedBootstrapServiceRecovery"
            }
            else {
                "-ElevatedBootstrapMsiIdleProof"
            }
            @(
                "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $StagedHelperPath,
                $workerSwitch,
                "-ElevatedInstallServiceInitialRunningEvent", $ServiceInitialRunningEventName,
                "-ElevatedInstallMsiMayHaveStartedEvent", $MsiMayHaveStartedEventName,
                "-ElevatedInstallMsiDefinitiveCompletionEvent", $MsiDefinitiveCompletionEventName,
                "-ElevatedInstallMsiIdleProvenEvent", $MsiIdleProvenEventName
            )
        }
        $line = @(
            Quote-Argument $hostPath
            @($a | ForEach-Object { Quote-Argument $_ })
        ) -join " "
        $p = $j.StartOwned($hostPath, $line)
        if (-not $p.WaitForExit($TimeoutMilliseconds)) {
            $j.Terminate()
            [void](Wait-JobEmpty -Job $j -TimeoutMilliseconds 5000)
            return "mode=$WorkerMode;status=timeout"
        }
        if (-not (Wait-JobEmpty -Job $j -TimeoutMilliseconds 5000)) {
            $j.Terminate()
            [void](Wait-JobEmpty -Job $j -TimeoutMilliseconds 5000)
            return "mode=$WorkerMode;status=tree_not_drained"
        }
        $p.WaitForExit()
        if ($j.RootExitCode -eq 0) {
            return "mode=$WorkerMode;status=completed"
        }
        return "mode=$WorkerMode;status=failed"
    }
    catch {
        return "mode=$WorkerMode;status=error;message=$($_.Exception.Message)"
    }
    finally {
        if ($null -ne $j) {
            try { if ($j.Active -gt 0) { $j.Terminate() } } catch { }
            $j.Dispose()
        }
        if ($null -ne $p) { $p.Dispose() }
    }
}
function New-AdminEvent {
    param([string]$Name)
    $security = [Security.AccessControl.EventWaitHandleSecurity]::new()
    $security.SetSecurityDescriptorSddlForm(
        "O:BAG:BAD:P(A;;RC;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)"
    )
    $arguments = [object[]]@($false, [Threading.EventResetMode]::ManualReset, $Name, $false, $security)
    $aclType = "System.Threading.EventWaitHandleAcl" -as [type]
    if ($null -ne $aclType) {
        $method = $aclType.GetMethods() | Where-Object {
            $_.Name -eq "Create" -and $_.GetParameters().Count -eq 5
        } | Select-Object -First 1
        $event = $method.Invoke($null, $arguments)
    }
    else {
        $constructor = [Threading.EventWaitHandle].GetConstructors() | Where-Object {
            $_.GetParameters().Count -eq 5
        } | Select-Object -First 1
        $event = $constructor.Invoke($arguments)
    }
    if (-not [bool]$arguments[3]) { $event.Dispose(); throw "start gate already existed" }
    return $event
}
trap {
    try {
        $encoded = [uri]::EscapeDataString("$_")
        [IO.File]::AppendAllText($stagedLog, "`nBE=$encoded`n", [Text.Encoding]::Unicode)
    }
    catch { }
    break
}
$payloadJson = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD_BASE64__")
)
$payload = $payloadJson | ConvertFrom-Json
$phaseInstanceId = [string]$payload.phase_instance_id
if ($phaseInstanceId -notmatch '^[0-9a-f]{32}$') { throw "invalid phase instance" }
$service_initial_running_event = "Local\Boundless.Installer.ServiceInitialRunning.v1.$phaseInstanceId"
$msi_may_have_started_event = "Local\Boundless.Installer.MsiMayHaveStarted.v1.$phaseInstanceId"
$msi_definitive_completion_event = "Local\Boundless.Installer.MsiDefinitiveCompletion.v1.$phaseInstanceId"
$msi_idle_proven_event = "Local\Boundless.Installer.MsiIdleProven.v1.$phaseInstanceId"
$programDataKnownFolder = [Environment]::GetFolderPath(
    [Environment+SpecialFolder]::CommonApplicationData
)
if ([string]::IsNullOrWhiteSpace($programDataKnownFolder)) {
    throw "Could not resolve the Windows CommonApplicationData known folder."
}
$programData = [IO.Path]::GetFullPath($programDataKnownFolder).TrimEnd('\')
$stageLeaf = [string]$payload.stage_leaf
if ($stageLeaf -notmatch '^BoundlessInstaller-[0-9a-f]{32}$') {
    throw "Installer stage leaf was invalid."
}
$stageRoot = Join-Path $programData $stageLeaf
$stagedLog = Join-Path $stageRoot "Boundless-install.log"
$trustedStage = $false
$logHandoffReady = $false
$exitCode = 1
$job = $null
$child = $null
$startGate = $null
$completion = [Threading.EventWaitHandle]::OpenExisting([string]$payload.completion_event_name)
$serviceInitialRunning = [Threading.EventWaitHandle]::OpenExisting(
    $service_initial_running_event
)
$msiMayHaveStarted = [Threading.EventWaitHandle]::OpenExisting(
    $msi_may_have_started_event
)
$msiDefinitiveCompletion = [Threading.EventWaitHandle]::OpenExisting(
    $msi_definitive_completion_event
)
$msiIdleProven = [Threading.EventWaitHandle]::OpenExisting(
    $msi_idle_proven_event
)
$bootstrapRecoveryEvidence = "not_evaluated"
$cancellation = [pscustomobject]@{
    event = [Threading.EventWaitHandle]::OpenExisting([string]$payload.cancellation_event_name)
    coordinator = Get-Process -Id ([int]$payload.coordinator_process_id) -ErrorAction Stop
    monitor = [Threading.Mutex]::OpenExisting([string]$payload.monitor_mutex_name)
}
try {
    if (
        $cancellation.coordinator.StartTime.ToUniversalTime().Ticks -ne
        [int64]$payload.coordinator_start_ticks
    ) {
        throw "Elevated installer coordinator process identity changed."
    }
    $preflightCancellation = Get-CancellationReason -Boundary $cancellation
    if (-not [string]::IsNullOrWhiteSpace($preflightCancellation)) {
        throw "Elevated installer canceled before staging because $preflightCancellation."
    }
    $security = [Security.AccessControl.DirectorySecurity]::new()
    $security.SetSecurityDescriptorSddlForm([string]$payload.stage_sddl)
    $item = New-BoundlessSecuredDirectoryAtomic -Path $stageRoot -Security $security
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Installer stage was a reparse point."
    }
    Assert-AdminAcl -Path $stageRoot -RequireProtected $true
    $trustedStage = $true

    $sourcePackageName = [IO.Path]::GetFileName([string]$payload.installer_path)
    $stagedMsi = Join-Path $stageRoot $sourcePackageName
    $stagedHelper = Join-Path $stageRoot "Boundless-Install.ps1"
    Copy-Item -LiteralPath $payload.installer_path -Destination $stagedMsi -ErrorAction Stop
    Copy-Item -LiteralPath $payload.helper_path -Destination $stagedHelper -ErrorAction Stop
    Assert-AdminAcl -Path $stagedMsi
    Assert-AdminAcl -Path $stagedHelper
    if ((Get-FileHash -LiteralPath $stagedMsi -Algorithm SHA256).Hash -ne $payload.installer_sha256) {
        throw "Staged MSI hash mismatch."
    }
    if ((Get-FileHash -LiteralPath $stagedHelper -Algorithm SHA256).Hash -ne $payload.helper_sha256) {
        throw "Staged helper hash mismatch."
    }

    $prelaunchCancellation = Get-CancellationReason -Boundary $cancellation
    if (-not [string]::IsNullOrWhiteSpace($prelaunchCancellation)) {
        throw "Elevated installer canceled before helper launch because $prelaunchCancellation."
    }
    $job = [BoundlessElevatedJob]::Create(
        [string]$payload.tree_job_name,
        [string]$payload.tree_job_sddl
    )
    $startGateName = "Local\Boundless.Installer.StartGate.v1.$([guid]::NewGuid().ToString('N'))"
    $startGate = New-AdminEvent -Name $startGateName
    $stagedResult = Join-Path $stageRoot "Boundless-install-result.txt"
    $arguments = @(
        "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $stagedHelper,
        "-ElevatedInstall", "-InstallerPath", $stagedMsi,
        "-ExpectedInstallerSha256", $payload.installer_sha256,
        "-AllowedUserSid", $payload.sid,
        "-ElevatedInstallCancelEvent", $payload.cancellation_event_name,
        "-ElevatedInstallCoordinatorProcessId", ([string]$payload.coordinator_process_id),
        "-ElevatedInstallCoordinatorStartTicks", ([string]$payload.coordinator_start_ticks),
        "-ElevatedInstallMonitorMutex", $payload.monitor_mutex_name,
        "-ElevatedInstallStartGate", $startGateName,
        "-ElevatedInstallResultPath", $stagedResult,
        "-ElevatedInstallServiceInitialRunningEvent", $service_initial_running_event,
        "-ElevatedInstallMsiMayHaveStartedEvent", $msi_may_have_started_event,
        "-ElevatedInstallMsiDefinitiveCompletionEvent", $msi_definitive_completion_event,
        "-ElevatedInstallMsiIdleProvenEvent", $msi_idle_proven_event,
        "-ElevatedInstallTimeoutSeconds", ([string]$payload.install_timeout_seconds)
    )
    if ([bool]$payload.quiet) { $arguments += "-Quiet" }
    if ([bool]$payload.no_restart) { $arguments += "-NoRestart" }
    if ([bool]$payload.log_requested) {
        $arguments += @("-LogPath", $stagedLog)
    }
    $argumentLine = @($arguments | ForEach-Object { Quote-Argument $_ }) -join " "
    $hostPath = (Get-Process -Id $PID -ErrorAction Stop).Path
    $childCommandLine = "$(Quote-Argument $hostPath) $argumentLine"
    $child = $job.StartOwned($hostPath, $childCommandLine)
    [void]$startGate.Set()
    $childCanceledReason = ""
    $childDeadline = (Get-Date).AddSeconds([int]$payload.install_timeout_seconds)
    while (-not $child.WaitForExit(100)) {
        $reason = Get-CancellationReason -Boundary $cancellation
        if (-not [string]::IsNullOrWhiteSpace($reason)) {
            $childCanceledReason = "Staged installer helper was canceled because $reason."
        }
        elseif ((Get-Date) -ge $childDeadline) {
            $childCanceledReason = "Staged installer helper exceeded its bounded install window."
        }
        if (-not [string]::IsNullOrWhiteSpace($childCanceledReason)) {
            $job.Terminate()
            if (-not (Wait-JobEmpty -Job $job -TimeoutMilliseconds 5000)) {
                throw "Staged installer process tree did not stop after cancellation."
            }
            if (-not $child.WaitForExit(5000)) {
                throw "Staged installer root was not signaled after its process tree emptied."
            }
            break
        }
    }
    if (-not (Wait-JobEmpty -Job $job -TimeoutMilliseconds 5000)) {
        $childCanceledReason = "Staged installer helper exited with a surviving owned descendant."
        $job.Terminate()
        if (-not (Wait-JobEmpty -Job $job -TimeoutMilliseconds 5000)) {
            throw "Staged installer descendant did not stop after process-tree cancellation."
        }
    }
    $exitCode = $job.RootExitCode
    $child.Dispose()
    $child = $null
    $childFailureDetail = ""
    if (Test-Path -LiteralPath $stagedResult -PathType Leaf) {
        Assert-AdminAcl -Path $stagedResult
        $childFailureDetail = (Get-Content -LiteralPath $stagedResult -Raw -ErrorAction Stop).Trim()
        Remove-Item -LiteralPath $stagedResult -Force -ErrorAction Stop
    }
    if ([bool]$payload.log_requested -and (Test-Path -LiteralPath $stagedLog -PathType Leaf)) {
        Remove-Item -LiteralPath $stagedMsi -Force -ErrorAction Stop
        Remove-Item -LiteralPath $stagedHelper -Force -ErrorAction Stop
        $handoffFileSecurity = [Security.AccessControl.FileSecurity]::new()
        $handoffFileSecurity.SetSecurityDescriptorSddlForm([string]$payload.log_handoff_file_sddl)
        Set-Acl -LiteralPath $stagedLog -AclObject $handoffFileSecurity -ErrorAction Stop
        $handoffSecurity = [Security.AccessControl.DirectorySecurity]::new()
        $handoffSecurity.SetSecurityDescriptorSddlForm([string]$payload.log_handoff_sddl)
        Set-Acl -LiteralPath $stageRoot -AclObject $handoffSecurity -ErrorAction Stop
        $logHandoffReady = $true
    }
    if (-not [string]::IsNullOrWhiteSpace($childCanceledReason)) {
        throw $childCanceledReason
    }
    if ($exitCode -notin @(0, 3010)) {
        $detailSuffix = if ([string]::IsNullOrWhiteSpace($childFailureDetail)) {
            ""
        }
        else {
            " Original staged error: $childFailureDetail"
        }
        throw "Immutable staged helper failed with exit code $exitCode.$detailSuffix"
    }
}
finally {
    $treeCleanupFailure = $null
    $treeClosed = $true
    try {
        if ($null -ne $job) {
            if ($job.Active -gt 0) {
                $job.Terminate()
                if (-not (Wait-JobEmpty -Job $job -TimeoutMilliseconds 5000)) {
                    $treeClosed = $false
                    throw "Owned staged installer process tree remained active during cleanup."
                }
            }
            $job.Dispose()
            $job = $null
        }
        if ($null -ne $child) {
            if (-not $child.HasExited) {
                $child.Kill()
                if (-not $child.WaitForExit(5000)) {
                    $treeClosed = $false
                    throw "Unassigned staged installer helper did not stop during cleanup."
                }
            }
            $child.Dispose()
            $child = $null
        }
    }
    catch {
        $treeCleanupFailure = $_
        $treeClosed = $false
        if ($null -ne $job) { $job.Dispose(); $job = $null }
    }
    if ($treeClosed) {
        if (
            $serviceInitialRunning.WaitOne(0) -and
            -not $msiMayHaveStarted.WaitOne(0)
        ) {
            $bootstrapRecoveryEvidence = Restore-BootstrapServiceBeforeMsiFailure `
                -TreeJobSddl ([string]$payload.tree_job_sddl) `
                -StagedHelperPath $stagedHelper `
                -ServiceInitialRunningEventName $service_initial_running_event `
                -MsiMayHaveStartedEventName $msi_may_have_started_event `
                -MsiDefinitiveCompletionEventName $msi_definitive_completion_event `
                -MsiIdleProvenEventName $msi_idle_proven_event
        }
        elseif ($msiMayHaveStarted.WaitOne(0)) {
            $bootstrapRecoveryEvidence = "start_requested=False;reason=msi_may_have_started"
        }
        else {
            $bootstrapRecoveryEvidence = "start_requested=False;reason=original_service_not_running_or_unproven"
        }
        Write-Host "boundless_install_bootstrap_service_recovery=$bootstrapRecoveryEvidence"
        if (
            $msiMayHaveStarted.WaitOne(0) -and
            -not $msiDefinitiveCompletion.WaitOne(0) -and
            -not $msiIdleProven.WaitOne(0)
        ) {
            $idleEvidence = Restore-BootstrapServiceBeforeMsiFailure `
                -TreeJobSddl ([string]$payload.tree_job_sddl) `
                -StagedHelperPath $stagedHelper `
                -ServiceInitialRunningEventName $service_initial_running_event `
                -MsiMayHaveStartedEventName $msi_may_have_started_event `
                -MsiDefinitiveCompletionEventName $msi_definitive_completion_event `
                -MsiIdleProvenEventName $msi_idle_proven_event `
                -WorkerMode "MsiIdleProof" `
                -TimeoutMilliseconds 20000
            Write-Host "boundless_install_msi_idle_worker=$idleEvidence"
            Write-Host "boundless_install_msi_transaction_idle_proven=$($msiIdleProven.WaitOne(0).ToString().ToLowerInvariant())"
        }
    }
    else {
        Write-Host "boundless_install_bootstrap_service_recovery=start_requested=False;reason=installer_tree_not_closed"
    }
    if ($null -ne $startGate) { $startGate.Dispose() }
    $cancellation.monitor.Dispose()
    $cancellation.coordinator.Dispose()
    $cancellation.event.Dispose()
    if ($treeClosed) { [void]$completion.Set() }
    $completion.Dispose()
    if ($trustedStage -and -not $logHandoffReady -and (Test-Path -LiteralPath $stageRoot)) {
        $resolved = (Resolve-Path -LiteralPath $stageRoot).Path
        $parent = [IO.Directory]::GetParent($resolved)
        $leaf = [IO.Path]::GetFileName($resolved)
        $item = Get-Item -LiteralPath $resolved -Force
        if (
            $null -eq $parent -or
            -not $parent.FullName.Equals($programData, [StringComparison]::OrdinalIgnoreCase) -or
            $leaf -notmatch '^BoundlessInstaller-[0-9a-f]{32}$' -or
            ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
        ) {
            throw "Refusing unsafe installer stage cleanup."
        }
        Assert-AdminAcl -Path $resolved -RequireProtected $true
        Remove-Item -LiteralPath $resolved -Recurse -Force -ErrorAction Stop
    }
    $msiIdleProven.Dispose()
    $msiDefinitiveCompletion.Dispose()
    $msiMayHaveStarted.Dispose()
    $serviceInitialRunning.Dispose()
    if ($null -ne $treeCleanupFailure) { throw $treeCleanupFailure }
}
exit $exitCode
'@
    $securedDirectoryFunction = (
        Get-Command New-BoundlessSecuredDirectoryAtomic -CommandType Function -ErrorAction Stop
    ).Definition
    $source = $source.Replace("__SECURED_DIRECTORY_FUNCTION__", $securedDirectoryFunction)
    $source = $source.Replace("__PAYLOAD_BASE64__", $payloadBase64)
    $encodedCommand = ConvertTo-BoundlessCompressedEncodedCommand -Source $source
    # CreateProcess limits the complete command line to 32,767 UTF-16 code
    # units. Keep more than 2 KiB for the PowerShell host path and switches.
    $encodedCommandBudget = 30500
    if ($encodedCommand.Length -gt $encodedCommandBudget) {
        throw "The bounded elevated install command exceeded the safe Windows command-line budget ($($encodedCommand.Length) > $encodedCommandBudget)."
    }
    return [pscustomobject]@{
        source = $source
        encoded_command = $encodedCommand
        installer_sha256 = $payload.installer_sha256
        installer_source_package_name = $installerSourcePackageName
        helper_sha256 = $payload.helper_sha256
        stage_path = $stageRoot
        staged_log_path = $stagedLogPath
        log_requested = [bool]$payload.log_requested
    }
}

function Invoke-BoundlessMsi {
    param(
        [string]$ResolvedInstallerPath,
        [string]$Sid,
        [object]$InstallerAnchor,
        [object]$QuiescenceLease,
        [int]$TimeoutSeconds = 900
    )

    if (
        $null -eq $QuiescenceLease -or
        $null -eq $QuiescenceLease.monitor -or
        $null -eq $QuiescenceLease.completion_event
    ) {
        throw "Elevated install requires an active tray quiescence lease."
    }
    $callerLogPath = if ([string]::IsNullOrWhiteSpace($LogPath)) {
        ""
    }
    else {
        $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($LogPath)
    }
    $controlEvent = New-BoundlessInstallerControlEvent -UserSid $Sid
    $process = $null
    $treeClosureState = [pscustomobject]@{ confirmed = $false }
    try {
        $elevatedCommandArgs = @{
            ResolvedInstallerPath = $ResolvedInstallerPath
            Sid = $Sid
            InstallerAnchor = $InstallerAnchor
            CancellationEventName = $controlEvent.name
            CoordinatorProcessId = $QuiescenceLease.sentinel_owner.process.Id
            CoordinatorStartTicks = (
                $QuiescenceLease.sentinel_owner.process.StartTime.ToUniversalTime().Ticks
            )
            MonitorMutexName = $QuiescenceLease.monitor.liveness_mutex_name
            TreeJobName = $QuiescenceLease.tree_job_name
            CompletionEventName = $QuiescenceLease.completion_event_name
            ServiceInitialRunningEventName = $QuiescenceLease.service_initial_running_event_name
            MsiMayHaveStartedEventName = $QuiescenceLease.msi_may_have_started_event_name
            MsiDefinitiveCompletionEventName = $QuiescenceLease.msi_definitive_completion_event_name
            MsiIdleProvenEventName = $QuiescenceLease.msi_idle_proven_event_name
            TimeoutSeconds = $TimeoutSeconds
        }
        $elevatedCommand = New-BoundlessElevatedInstallCommand @elevatedCommandArgs
        $arguments = @(
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            $elevatedCommand.encoded_command
        )

        $startArgs = @{
            FilePath = (Resolve-CurrentPowerShellExecutable)
            ArgumentList = (@($arguments | ForEach-Object { ConvertTo-ProcessArgument -Value $_ }) -join " ")
            WindowStyle = "Hidden"
            PassThru = $true
        }
        if (-not (Test-IsAdministrator)) {
            $startArgs.Verb = "RunAs"
        }
        $process = Start-Process @startArgs
        $QuiescenceLease.elevated_process = $process
        $supervisionError = $null
        $exitCode = $null
        try {
            $exitCode = Wait-BoundlessElevatedInstallSupervised `
                -InstallerProcess $process `
                -Monitor $QuiescenceLease.monitor `
                -CancellationEvent $controlEvent.event `
                -CompletionEvent $QuiescenceLease.completion_event `
                -TreeJobName $QuiescenceLease.tree_job_name `
                -TreeClosureState $treeClosureState `
                -HardKillRecoveryAction {
                    Restore-BoundlessServiceAfterHardKilledElevatedInstall `
                        -QuiescenceLease $QuiescenceLease `
                        -StagedHelperPath (Join-Path `
                            $elevatedCommand.stage_path `
                            "Boundless-Install.ps1")
                } `
                -TimeoutSeconds $TimeoutSeconds
        }
        catch {
            $supervisionError = $_
        }
        $logHandoff = $null
        $elevatedErrorDetail = ""
        if ($elevatedCommand.log_requested) {
            try {
                $logHandoff = Copy-BoundlessInstallerLogHandoff `
                    -StageRoot $elevatedCommand.stage_path `
                    -StagedLogPath $elevatedCommand.staged_log_path `
                    -DestinationPath $callerLogPath
            }
            catch {
                if ($null -eq $supervisionError) {
                    throw
                }
                throw "$($supervisionError.Exception.Message) Installer log handoff also failed: $($_.Exception.Message)"
            }
            if (-not $logHandoff.copied) {
                $message = "Elevated installer did not produce the explicitly requested staged MSI log."
                if ($null -ne $supervisionError) {
                    throw "$($supervisionError.Exception.Message) $message"
                }
                throw $message
            }
            try {
                $elevatedErrorDetail = Get-BoundlessElevatedInstallErrorFromLog `
                    -Path $logHandoff.destination
            }
            catch {
                $elevatedErrorDetail = "Elevated installer error handoff could not be read: $($_.Exception.Message)"
            }
        }
        if ($null -ne $supervisionError) {
            if (-not [string]::IsNullOrWhiteSpace($elevatedErrorDetail)) {
                throw "$($supervisionError.Exception.Message) Elevated installer detail: $elevatedErrorDetail"
            }
            throw $supervisionError
        }
        if ($exitCode -notin @(0, 3010)) {
            $detailSuffix = if ([string]::IsNullOrWhiteSpace($elevatedErrorDetail)) {
                ""
            }
            else {
                " $elevatedErrorDetail"
            }
            throw "Elevated Boundless install phase exited with $exitCode.$detailSuffix"
        }
    }
    finally {
        if ($null -eq $process) {
            $treeClosureState.confirmed = $true
        }
        $QuiescenceLease.evidence.installer_tree_closed = $treeClosureState.confirmed
        if ($null -ne $treeClosureState.PSObject.Properties["hard_kill_used"]) {
            $QuiescenceLease.evidence.elevated_wrapper_hard_kill_used = (
                [bool]$treeClosureState.hard_kill_used
            )
        }
        if ($null -ne $treeClosureState.PSObject.Properties["parent_service_recovery_reconciled"]) {
            $QuiescenceLease.evidence.parent_service_recovery_reconciled = (
                [bool]$treeClosureState.parent_service_recovery_reconciled
            )
            $QuiescenceLease.evidence.parent_service_recovery_status = (
                [string]$treeClosureState.parent_service_recovery_status
            )
        }
        [void](Update-BoundlessInstallerPhaseEvidence -Lease $QuiescenceLease)
        if ($null -ne $process) {
            if ($treeClosureState.confirmed) {
                $process.Dispose()
                $QuiescenceLease.elevated_process = $null
            }
        }
        $controlEvent.event.Dispose()
    }

    # The elevated phase can launch MSI only after the bounded non-forced
    # service stop completed. Exact stop timing is printed in that phase; this
    # parent records only the cross-elevation contract.
    return Assert-ElevatedInstallResult -Result ([pscustomobject]@{
        status = "passed"
        msi_exit_code = $exitCode
        service_shutdown = [pscustomobject]@{
            initial_status = "captured_in_elevated_phase"
            final_status = "StoppedOrNotInstalledBeforeMsi"
            stop_requested = $null
            elapsed_milliseconds = $null
            force_kill_used = $false
            msi_service_control = "idempotent_verification_after_helper_stop"
        }
        # Detailed shutdown evidence remains owned by the staged elevated
        # helper. Preserve the parent result schema without inventing values
        # that were not serialized across the process boundary.
        input_injector_shutdown = $null
        installer_stage = [pscustomobject]@{
            admin_only = $true
            hash_verified = $true
            staged_copy_used = $true
            cleaned = $true
        }
        log_handoff = $logHandoff
    })
}

function Get-MsiProperty {
    param(
        [string]$Path,
        [string]$Property
    )

    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $installer.GetType().InvokeMember(
        "OpenDatabase",
        [System.Reflection.BindingFlags]::InvokeMethod,
        $null,
        $installer,
        @($Path, 0)
    )
    $escapedProperty = $Property.Replace("'", "''")
    $view = $database.GetType().InvokeMember(
        "OpenView",
        [System.Reflection.BindingFlags]::InvokeMethod,
        $null,
        $database,
        @("SELECT ``Value`` FROM ``Property`` WHERE ``Property``='$escapedProperty'")
    )
    $view.GetType().InvokeMember(
        "Execute",
        [System.Reflection.BindingFlags]::InvokeMethod,
        $null,
        $view,
        $null
    ) | Out-Null
    $record = $view.GetType().InvokeMember(
        "Fetch",
        [System.Reflection.BindingFlags]::InvokeMethod,
        $null,
        $view,
        $null
    )
    if ($null -eq $record) {
        throw "MSI property '$Property' was not found in $Path"
    }
    return $record.StringData(1)
}

function New-BoundlessInstallerAnchor {
    param([string]$ResolvedInstallerPath)

    $resolvedPath = (Resolve-Path -LiteralPath $ResolvedInstallerPath -ErrorAction Stop).Path
    $before = Get-Item -LiteralPath $resolvedPath -Force -ErrorAction Stop
    $hashBefore = (Get-FileHash -LiteralPath $resolvedPath -Algorithm SHA256).Hash
    $productVersion = Get-MsiProperty -Path $resolvedPath -Property "ProductVersion"
    $productCode = Get-MsiProperty -Path $resolvedPath -Property "ProductCode"
    $hashAfter = (Get-FileHash -LiteralPath $resolvedPath -Algorithm SHA256).Hash
    $after = Get-Item -LiteralPath $resolvedPath -Force -ErrorAction Stop
    if (
        -not $hashBefore.Equals($hashAfter, [StringComparison]::OrdinalIgnoreCase) -or
        [int64]$before.Length -ne [int64]$after.Length -or
        [int64]$before.LastWriteTimeUtc.Ticks -ne [int64]$after.LastWriteTimeUtc.Ticks
    ) {
        throw "MSI changed while its pre-UAC identity and metadata were being anchored."
    }
    if ($productVersion -notmatch '^\d+\.\d+\.\d+$') {
        throw "MSI ProductVersion was invalid while anchoring: $productVersion"
    }
    if ($productCode -notmatch '^\{[0-9A-Fa-f-]+\}$') {
        throw "MSI ProductCode was invalid while anchoring: $productCode"
    }
    return [pscustomobject]@{
        path = $resolvedPath
        sha256 = $hashBefore
        length = [int64]$before.Length
        last_write_utc_ticks = [int64]$before.LastWriteTimeUtc.Ticks
        product_version = $productVersion
        product_code = $productCode
    }
}

function Assert-BoundlessInstallerAnchor {
    param(
        [object]$Anchor,
        [string]$ResolvedInstallerPath
    )

    if ($null -eq $Anchor) {
        throw "MSI pre-UAC identity anchor was unavailable."
    }
    $resolvedPath = (Resolve-Path -LiteralPath $ResolvedInstallerPath -ErrorAction Stop).Path
    if (-not $resolvedPath.Equals($Anchor.path, [StringComparison]::OrdinalIgnoreCase)) {
        throw "MSI path changed after its pre-UAC identity was anchored."
    }
    $item = Get-Item -LiteralPath $resolvedPath -Force -ErrorAction Stop
    $hash = (Get-FileHash -LiteralPath $resolvedPath -Algorithm SHA256).Hash
    if (
        [int64]$item.Length -ne [int64]$Anchor.length -or
        [int64]$item.LastWriteTimeUtc.Ticks -ne [int64]$Anchor.last_write_utc_ticks -or
        -not $hash.Equals($Anchor.sha256, [StringComparison]::OrdinalIgnoreCase)
    ) {
        throw "MSI changed after its pre-UAC identity was anchored."
    }
    return $Anchor
}

function Get-BoundlessUninstallEntry {
    param([string]$ProductCode)

    foreach ($path in @(
        "Registry::HKEY_LOCAL_MACHINE\Software\Microsoft\Windows\CurrentVersion\Uninstall\$ProductCode",
        "Registry::HKEY_LOCAL_MACHINE\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\$ProductCode"
    )) {
        $entry = Get-ItemProperty -LiteralPath $path -ErrorAction SilentlyContinue
        if ($null -ne $entry) {
            return $entry
        }
    }
    return $null
}

function Invoke-BoundedProcess {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList,
        [int]$TimeoutSeconds = 10,
        [hashtable]$EnvironmentVariables = @{}
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.Arguments = @($ArgumentList | ForEach-Object { ConvertTo-ProcessArgument -Value $_ }) -join " "
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($entry in $EnvironmentVariables.GetEnumerator()) {
        $startInfo.EnvironmentVariables[$entry.Key] = [string]$entry.Value
    }

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw "Failed to start $FilePath."
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill()
            throw "$FilePath did not exit within $($TimeoutSeconds)s."
        }
        $process.WaitForExit()

        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        $exitCode = $process.ExitCode
        return [pscustomobject]@{
            exit_code = $exitCode
            stdout = $stdout.Trim()
            stderr = $stderr.Trim()
        }
    }
    finally {
        $process.Dispose()
    }
}

function Get-BoundlessServiceStopDecision {
    param(
        [string]$Status,
        [bool]$StopRequested
    )

    if ($Status -eq "Stopped") {
        return "complete"
    }
    if ($Status -eq "StopPending" -or $StopRequested) {
        return "wait"
    }
    return "request_stop"
}

function Get-BoundlessServiceStatusBounded {
    param(
        [int]$TimeoutSeconds = 2,
        [string]$FixtureSource = ""
    )

    $source = if (-not [string]::IsNullOrWhiteSpace($FixtureSource)) {
        $FixtureSource
    }
    else {
@'
$ErrorActionPreference = "Stop"
$service = Get-Service -Name "BoundlessService" -ErrorAction SilentlyContinue
if ($null -eq $service) { exit 31 }
$codes = @{
    Stopped = 40
    StartPending = 41
    StopPending = 42
    Running = 43
    ContinuePending = 44
    PausePending = 45
    Paused = 46
}
$status = $service.Status.ToString()
if (-not $codes.ContainsKey($status)) { exit 47 }
exit $codes[$status]
'@
    }
    $worker = Start-BoundlessOwnedProcessBoundary `
        -FilePath (Resolve-CurrentPowerShellExecutable) `
        -ArgumentList @(
            "-NoProfile",
            "-EncodedCommand",
            [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($source))
        ) `
        -CreateNoWindow
    try {
        if (-not $worker.WaitForExit($TimeoutSeconds * 1000)) {
            Stop-BoundlessProcessBoundary -Process $worker -TimeoutMilliseconds 5000
            throw "BoundlessService status query exceeded $TimeoutSeconds seconds."
        }
        if (-not $worker.WaitForTreeExit(5000)) {
            Stop-BoundlessProcessBoundary -Process $worker -TimeoutMilliseconds 5000
            throw "BoundlessService status query left an owned descendant."
        }
        $statuses = @{
            31 = "Missing"
            40 = "Stopped"
            41 = "StartPending"
            42 = "StopPending"
            43 = "Running"
            44 = "ContinuePending"
            45 = "PausePending"
            46 = "Paused"
        }
        if (-not $statuses.ContainsKey($worker.ExitCode)) {
            throw "BoundlessService status query failed with code $($worker.ExitCode)."
        }
        return $statuses[$worker.ExitCode]
    }
    finally {
        if ($worker.ActiveProcessCount -gt 0) {
            Stop-BoundlessProcessBoundary -Process $worker -TimeoutMilliseconds 5000
        }
        $worker.Dispose()
    }
}

function Start-BoundlessServiceControlWorker {
    param(
        [ValidateSet("stop", "start")]
        [string]$Action,
        [string]$FixtureSource = ""
    )

    $source = if (-not [string]::IsNullOrWhiteSpace($FixtureSource)) {
        $FixtureSource
    }
    else {
@'
$ErrorActionPreference = "Stop"
$service = Get-Service -Name "BoundlessService" -ErrorAction SilentlyContinue
if ($null -eq $service) { exit 31 }
if ("__ACTION__" -eq "stop") {
    if ($service.Status.ToString() -eq "Stopped") { exit 0 }
    if (-not $service.CanStop) { exit 32 }
    $service.Stop()
}
else {
    if ($service.Status.ToString() -eq "Running") { exit 0 }
    $service.Start()
}
exit 0
'@.Replace("__ACTION__", $Action)
    }
    return Start-BoundlessOwnedProcessBoundary `
        -FilePath (Resolve-CurrentPowerShellExecutable) `
        -ArgumentList @(
            "-NoProfile",
            "-EncodedCommand",
            [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($source))
        ) `
        -CreateNoWindow
}

function Wait-BoundlessServiceTransition {
    param(
        [ValidateSet("Stopped", "Running")]
        [string]$DesiredStatus,
        [object]$Worker,
        [scriptblock]$StatusProbe,
        [int]$TimeoutSeconds,
        [string]$FailurePrefix
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $lastStatus = "Unknown"
    try {
        do {
            $lastStatus = & $StatusProbe
            if ($lastStatus -eq $DesiredStatus) {
                if ($null -ne $Worker -and $Worker.ActiveProcessCount -gt 0) {
                    Stop-BoundlessProcessBoundary -Process $Worker -TimeoutMilliseconds 5000
                }
                return $lastStatus
            }
            if ($null -ne $Worker -and $Worker.HasExited -and $Worker.ExitCode -ne 0) {
                throw "$FailurePrefix request worker exited with code $($Worker.ExitCode); current=$lastStatus."
            }
            Start-Sleep -Milliseconds 100
        } while ((Get-Date) -lt $deadline)
        throw "$FailurePrefix did not reach $DesiredStatus within $($TimeoutSeconds)s; current=$lastStatus."
    }
    finally {
        if ($null -ne $Worker) {
            if ($Worker.ActiveProcessCount -gt 0) {
                Stop-BoundlessProcessBoundary -Process $Worker -TimeoutMilliseconds 5000
            }
            $Worker.Dispose()
        }
    }
}

function Stop-BoundlessServiceForUpgrade {
    param(
        [int]$TimeoutSeconds = 15,
        [scriptblock]$StatusProbe = $null,
        [scriptblock]$WorkerFactory = $null,
        [Threading.EventWaitHandle]$InitialRunningEvent = $null,
        [switch]$SkipAdministratorCheck
    )

    if (-not $SkipAdministratorCheck -and -not (Test-IsAdministrator)) {
        throw "Stopping BoundlessService for upgrade requires elevation."
    }
    if ($null -eq $StatusProbe) {
        $StatusProbe = {
            Get-BoundlessServiceStatusBounded -TimeoutSeconds 2
        }
    }
    $initialStatus = & $StatusProbe
    if ($initialStatus -in @("Running", "StartPending") -and $null -ne $InitialRunningEvent) {
        if (-not $InitialRunningEvent.Set()) {
            throw "Could not publish the originally-running BoundlessService boundary before stop."
        }
    }
    if ($initialStatus -eq "Missing") {
        return [pscustomobject]@{
            initial_status = "NotInstalled"
            final_status = "NotInstalled"
            stop_requested = $false
            elapsed_milliseconds = 0
            force_kill_used = $false
            msi_service_control = "idempotent_install_contract"
        }
    }
    if ($initialStatus -eq "Stopped") {
        return [pscustomobject]@{
            initial_status = $initialStatus
            final_status = $initialStatus
            stop_requested = $false
            elapsed_milliseconds = 0
            force_kill_used = $false
            msi_service_control = "idempotent_verification_after_helper_stop"
        }
    }

    $worker = $null
    $stopRequested = $false
    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    try {
        if ($initialStatus -ne "StopPending") {
            # Mark the request as attempted before invoking the worker factory.
            # If worker creation races with a fast successful stop and then
            # throws, the pre-MSI recovery boundary must still probe the live
            # service state rather than assume no mutation occurred.
            $stopRequested = $true
            $worker = if ($null -ne $WorkerFactory) {
                & $WorkerFactory
            }
            else {
                Start-BoundlessServiceControlWorker -Action "stop"
            }
        }
        $finalStatus = Wait-BoundlessServiceTransition `
            -DesiredStatus "Stopped" `
            -Worker $worker `
            -StatusProbe $StatusProbe `
            -TimeoutSeconds $TimeoutSeconds `
            -FailurePrefix "BoundlessService stop"
        $worker = $null
        $stopwatch.Stop()
        return [pscustomobject]@{
            initial_status = $initialStatus
            final_status = $finalStatus
            stop_requested = $stopRequested
            elapsed_milliseconds = $stopwatch.ElapsedMilliseconds
            force_kill_used = $false
            msi_service_control = "idempotent_verification_after_helper_stop"
        }
    }
    catch {
        $originalError = $_
        $exception = [InvalidOperationException]::new(
            "$($originalError.Exception.Message) The MSI was not started.",
            $originalError.Exception
        )
        $exception.Data["BoundlessMsiCompletionState"] = "not_started"
        $exception.Data["BoundlessServiceStopInitialStatus"] = $initialStatus
        $exception.Data["BoundlessServiceStopRequested"] = $stopRequested
        throw $exception
    }
}

function Stop-BoundlessServiceBeforeMsi {
    param(
        [int]$TimeoutSeconds = 15,
        [scriptblock]$StatusProbe = $null,
        [scriptblock]$WorkerFactory = $null,
        [scriptblock]$RecoveryStatusProbe = $null,
        [scriptblock]$RecoveryWorkerFactory = $null,
        [Threading.EventWaitHandle]$InitialRunningEvent = $null,
        [switch]$SkipAdministratorCheck
    )

    $stopArguments = @{
        TimeoutSeconds = $TimeoutSeconds
        StatusProbe = $StatusProbe
        WorkerFactory = $WorkerFactory
        InitialRunningEvent = $InitialRunningEvent
        SkipAdministratorCheck = $SkipAdministratorCheck
    }
    try {
        return Stop-BoundlessServiceForUpgrade @stopArguments
    }
    catch {
        $originalError = $_
        $initialStatus = [string]$originalError.Exception.Data[
            "BoundlessServiceStopInitialStatus"
        ]
        $stopRequested = [bool]$originalError.Exception.Data[
            "BoundlessServiceStopRequested"
        ]
        if ($initialStatus -notin @("Running", "StartPending") -or -not $stopRequested) {
            $originalError.Exception.Data["BoundlessServiceRecovery"] = (
                "start_requested=False;reason=original_service_not_running_or_stop_not_requested"
            )
            throw $originalError
        }

        $effectiveRecoveryProbe = if ($null -ne $RecoveryStatusProbe) {
            $RecoveryStatusProbe
        }
        elseif ($null -ne $StatusProbe) {
            $StatusProbe
        }
        else {
            { Get-BoundlessServiceStatusBounded -TimeoutSeconds 2 }
        }
        try {
            $recoveryStatus = & $effectiveRecoveryProbe
        }
        catch {
            $originalError.Exception.Data["BoundlessServiceRecoveryError"] = (
                "Could not establish the post-stop service state: $($_.Exception.Message)"
            )
            Write-Warning "BoundlessService recovery after pre-MSI stop failure could not establish service state: $($_.Exception.Message)"
            throw $originalError
        }

        if ($recoveryStatus -notin @("Stopped", "StopPending")) {
            $reason = if ($recoveryStatus -eq "Missing") {
                "service_missing_or_uninstall_policy"
            }
            else {
                "stop_not_observed"
            }
            $originalError.Exception.Data["BoundlessServiceRecovery"] = (
                "start_requested=False;current_status=$recoveryStatus;reason=$reason"
            )
            throw $originalError
        }

        try {
            $recoveryArguments = @{
                TimeoutSeconds = $TimeoutSeconds
                StatusProbe = $effectiveRecoveryProbe
                WorkerFactory = $RecoveryWorkerFactory
                SkipAdministratorCheck = $SkipAdministratorCheck
            }
            $recovery = Start-BoundlessServiceAfterFailedInstall @recoveryArguments
            $originalError.Exception.Data["BoundlessServiceRecovery"] = (
                "start_requested=$($recovery.start_requested);final_status=$($recovery.final_status)"
            )
        }
        catch {
            $originalError.Exception.Data["BoundlessServiceRecoveryError"] = $_.Exception.Message
            Write-Warning "BoundlessService recovery after pre-MSI stop failure also failed: $($_.Exception.Message)"
        }
        throw $originalError
    }
}

function Start-BoundlessServiceAfterFailedInstall {
    param(
        [int]$TimeoutSeconds = 15,
        [scriptblock]$StatusProbe = $null,
        [scriptblock]$WorkerFactory = $null,
        [object]$RecoveryAuthority = $null,
        [scriptblock]$BeforeServiceStartAction = $null,
        [switch]$SkipAdministratorCheck
    )

    if (-not $SkipAdministratorCheck -and -not (Test-IsAdministrator)) {
        throw "Restarting BoundlessService after install failure requires elevation."
    }
    if ($null -eq $StatusProbe) {
        $StatusProbe = {
            Get-BoundlessServiceStatusBounded -TimeoutSeconds 2
        }
    }
    $initialStatus = & $StatusProbe
    if ($initialStatus -eq "Missing") {
        throw "BoundlessService was no longer registered after the failed install boundary."
    }
    if ($initialStatus -eq "Running") {
        return [pscustomobject]@{ start_requested = $false; final_status = "Running" }
    }
    if ($initialStatus -eq "StopPending") {
        $initialStatus = Wait-BoundlessServiceTransition `
            -DesiredStatus "Stopped" `
            -Worker $null `
            -StatusProbe $StatusProbe `
            -TimeoutSeconds $TimeoutSeconds `
            -FailurePrefix "BoundlessService recovery stop settlement"
    }
    elseif ($initialStatus -eq "StartPending") {
        $finalStatus = Wait-BoundlessServiceTransition `
            -DesiredStatus "Running" `
            -Worker $null `
            -StatusProbe $StatusProbe `
            -TimeoutSeconds $TimeoutSeconds `
            -FailurePrefix "BoundlessService recovery existing start"
        return [pscustomobject]@{ start_requested = $false; final_status = $finalStatus }
    }
    $actionFenceOwned = $false
    try {
        if ($null -ne $RecoveryAuthority) {
            try {
                $actionFenceOwned = $RecoveryAuthority.action_fence.WaitOne(
                    ($TimeoutSeconds + 5) * 1000
                )
            }
            catch [Threading.AbandonedMutexException] {
                $actionFenceOwned = $true
                throw "BoundlessService recovery action fence was abandoned before admission."
            }
            if (-not $actionFenceOwned) {
                throw "BoundlessService recovery action fence admission timed out."
            }
            if ($RecoveryAuthority.revocation_event.WaitOne(0)) {
                throw "BoundlessService recovery authority was revoked before the start request."
            }
            if (-not $RecoveryAuthority.action_committed_event.Set()) {
                throw "Could not publish the committed BoundlessService recovery action."
            }
        }
        if ($null -ne $BeforeServiceStartAction) {
            & $BeforeServiceStartAction
        }
        $worker = if ($null -ne $WorkerFactory) {
            & $WorkerFactory
        }
        else {
            Start-BoundlessServiceControlWorker -Action "start"
        }
        $finalStatus = Wait-BoundlessServiceTransition `
            -DesiredStatus "Running" `
            -Worker $worker `
            -StatusProbe $StatusProbe `
            -TimeoutSeconds $TimeoutSeconds `
            -FailurePrefix "BoundlessService recovery start"
        return [pscustomobject]@{
            start_requested = $true
            final_status = $finalStatus
        }
    }
    finally {
        if ($actionFenceOwned) {
            $RecoveryAuthority.action_fence.ReleaseMutex()
        }
    }
}

function Get-ProcessOwnerSid {
    param([int]$ProcessId)

    Initialize-BoundlessInstallNativeMethods
    $ownerSid = [BoundlessInstallNativeMethodsV2]::GetProcessOwnerSid($ProcessId)
    if ([string]::IsNullOrWhiteSpace($ownerSid)) {
        throw "Process $ProcessId exited before its owner could be verified."
    }
    return $ownerSid
}

function Assert-BoundlessTrayShutdownTargets {
    param(
        [object[]]$Processes,
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId
    )

    foreach ($process in @($Processes)) {
        if ($process.session_id -ne $ExpectedSessionId) {
            throw "Refusing to stop Boundless tray PID $($process.id) from session $($process.session_id); expected session $ExpectedSessionId."
        }
        if ([string]::IsNullOrWhiteSpace($process.owner_sid) -or $process.owner_sid -ne $ExpectedOwnerSid) {
            throw "Refusing to stop Boundless tray PID $($process.id) because its owner SID could not be proven as $ExpectedOwnerSid."
        }
    }
    return @($Processes)
}

function Initialize-BoundlessInstallNativeMethods {
    if ($null -ne ("BoundlessInstallNativeMethodsV2" -as [type])) {
        return
    }

    Add-Type -TypeDefinition @"
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class BoundlessInstallNativeMethodsV2
{
    private const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;
    private const uint TOKEN_QUERY = 0x0008;

    [StructLayout(LayoutKind.Sequential)]
    private struct SID_AND_ATTRIBUTES
    {
        public IntPtr Sid;
        public uint Attributes;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct TOKEN_USER
    {
        public SID_AND_ATTRIBUTES User;
    }

    [DllImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool PostThreadMessage(
        uint threadId,
        uint message,
        UIntPtr wParam,
        IntPtr lParam);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(uint desiredAccess, bool inheritHandle, int processId);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr handle);

    [DllImport("advapi32.dll", SetLastError = true)]
    private static extern bool OpenProcessToken(IntPtr processHandle, uint desiredAccess, out IntPtr tokenHandle);

    [DllImport("advapi32.dll", SetLastError = true)]
    private static extern bool GetTokenInformation(
        IntPtr tokenHandle,
        int tokenInformationClass,
        IntPtr tokenInformation,
        int tokenInformationLength,
        out int returnLength);

    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool ConvertSidToStringSidW(IntPtr sid, out IntPtr stringSid);

    [DllImport("kernel32.dll")]
    private static extern IntPtr LocalFree(IntPtr memory);

    public static string GetProcessOwnerSid(int processId)
    {
        IntPtr process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, processId);
        if (process == IntPtr.Zero)
        {
            int error = Marshal.GetLastWin32Error();
            if (error == 87) { return String.Empty; }
            throw new Win32Exception(error, "OpenProcess(owner lookup) failed");
        }
        IntPtr token = IntPtr.Zero;
        IntPtr buffer = IntPtr.Zero;
        IntPtr sidText = IntPtr.Zero;
        try
        {
            if (!OpenProcessToken(process, TOKEN_QUERY, out token))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenProcessToken failed");
            int required;
            GetTokenInformation(token, 1, IntPtr.Zero, 0, out required);
            int sizeError = Marshal.GetLastWin32Error();
            if (required <= 0 || sizeError != 122)
                throw new Win32Exception(sizeError, "GetTokenInformation(size) failed");
            buffer = Marshal.AllocHGlobal(required);
            if (!GetTokenInformation(token, 1, buffer, required, out required))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "GetTokenInformation(TokenUser) failed");
            TOKEN_USER user = (TOKEN_USER)Marshal.PtrToStructure(buffer, typeof(TOKEN_USER));
            if (!ConvertSidToStringSidW(user.User.Sid, out sidText))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "ConvertSidToStringSid failed");
            return Marshal.PtrToStringUni(sidText);
        }
        finally
        {
            if (sidText != IntPtr.Zero) LocalFree(sidText);
            if (buffer != IntPtr.Zero) Marshal.FreeHGlobal(buffer);
            if (token != IntPtr.Zero) CloseHandle(token);
            CloseHandle(process);
        }
    }
}
"@
}

function Request-LegacyBoundlessTrayQuit {
    param([int[]]$ProcessIds)

    Initialize-BoundlessInstallNativeMethods
    $postCount = 0
    foreach ($processId in $ProcessIds) {
        $process = Get-Process -Id $processId -ErrorAction SilentlyContinue
        if ($null -eq $process) {
            continue
        }
        try {
            foreach ($thread in @($process.Threads)) {
                # v5.0.13 has no external Quit command. Posting WM_QUIT to its
                # same-user GUI/hook message queues causes eframe to unwind and
                # DashboardApp to drop; InputBrokerSupervisor::Drop then runs
                # the existing local fail-open and bounded detach path.
                if ([BoundlessInstallNativeMethodsV2]::PostThreadMessage(
                    [uint32]$thread.Id,
                    [uint32]0x0012,
                    [UIntPtr]::Zero,
                    [IntPtr]::Zero
                )) {
                    $postCount += 1
                }
            }
        }
        catch {
            if ($null -ne (Get-Process -Id $processId -ErrorAction SilentlyContinue)) {
                throw "Could not request graceful legacy shutdown for Boundless tray PID $processId. $($_.Exception.Message)"
            }
        }
    }
    return $postCount
}

function Wait-BoundlessTrayProcessIdsExited {
    param(
        [int[]]$ProcessIds,
        [int]$TimeoutMilliseconds
    )

    $deadline = (Get-Date).AddMilliseconds($TimeoutMilliseconds)
    do {
        $remaining = @(
            $ProcessIds |
                Where-Object { $null -ne (Get-Process -Id $_ -ErrorAction SilentlyContinue) }
        )
        if ($remaining.Count -eq 0) {
            return $true
        }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    return $false
}

function Stop-BoundlessTrayForUpgrade {
    param(
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId = -1,
        [int]$TimeoutSeconds = 8
    )

    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    $currentSessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
    if ($ExpectedSessionId -lt 0) {
        $ExpectedSessionId = $currentSessionId
    }
    if ($ExpectedSessionId -ne $currentSessionId) {
        throw "Refusing tray shutdown outside helper session $currentSessionId; requested $ExpectedSessionId."
    }
    $targets = @(
        Assert-BoundlessTrayShutdownTargets -Processes @(
            Get-BoundlessTrayProcessesForCurrentSession
        ) -ExpectedOwnerSid $ExpectedOwnerSid -ExpectedSessionId $ExpectedSessionId
    )
    if ($targets.Count -eq 0) {
        return [pscustomobject]@{
            initial_count = 0
            control_requests = 0
            legacy_thread_quit_posts = 0
            elapsed_milliseconds = 0
            force_kill_used = $false
        }
    }

    $processIds = @($targets | Select-Object -ExpandProperty id)
    # Never execute an image path discovered from a user-owned process while
    # this helper may already be elevated. New trays expose a trusted named
    # shutdown event; v5.0.13 and cross-credential UAC fall back to same-user,
    # same-session WM_QUIT after the target identity proof above.
    $shutdownSignaled = Request-BoundlessTrayShutdownSignal `
        -ExpectedOwnerSid $ExpectedOwnerSid `
        -ExpectedSessionId $ExpectedSessionId
    $controlRequests = if ($shutdownSignaled) { 1 } else { 0 }
    if (
        $shutdownSignaled -and
        (Wait-BoundlessTrayProcessIdsExited -ProcessIds $processIds -TimeoutMilliseconds 750)
    ) {
        $stopwatch.Stop()
        return [pscustomobject]@{
            initial_count = $targets.Count
            control_requests = $controlRequests
            legacy_thread_quit_posts = 0
            elapsed_milliseconds = $stopwatch.ElapsedMilliseconds
            force_kill_used = $false
        }
    }

    $legacyPosts = Request-LegacyBoundlessTrayQuit -ProcessIds $processIds
    $remainingMilliseconds = [Math]::Max(100, ($TimeoutSeconds * 1000) - [int]$stopwatch.ElapsedMilliseconds)
    if (-not (Wait-BoundlessTrayProcessIdsExited -ProcessIds $processIds -TimeoutMilliseconds $remainingMilliseconds)) {
        $remaining = @(
            $processIds |
                Where-Object { $null -ne (Get-Process -Id $_ -ErrorAction SilentlyContinue) }
        ) -join ","
        throw "Boundless tray did not exit gracefully within $($TimeoutSeconds)s (remaining PIDs: $remaining). Quit Boundless manually and rerun the helper. The UAC/MSI phase was not started."
    }

    $stopwatch.Stop()
    return [pscustomobject]@{
        initial_count = $targets.Count
        control_requests = $controlRequests
        legacy_thread_quit_posts = $legacyPosts
        elapsed_milliseconds = $stopwatch.ElapsedMilliseconds
        force_kill_used = $false
    }
}

function Wait-BoundlessServiceRunning {
    param([int]$TimeoutSeconds = 30)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $service = Get-Service -Name "BoundlessService" -ErrorAction SilentlyContinue
        if ($null -ne $service -and $service.Status.ToString() -eq "Running") {
            return $service
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    if ($null -eq $service) {
        throw "BoundlessService was not registered after installation."
    }
    throw "BoundlessService did not reach Running within $($TimeoutSeconds)s; current=$($service.Status)."
}

function ConvertFrom-BoundlessDaemonStatusOutput {
    param(
        [string]$Output,
        [string]$ExpectedVersion
    )

    $running = $Output -match '(^|\s)running=true(\s|$)'
    $versionMatch = [regex]::Match($Output, '(^|\s)daemon_version=(?<version>[^\s]+)(\s|$)')
    $reportedVersion = if ($versionMatch.Success) {
        $versionMatch.Groups['version'].Value
    }
    else {
        ""
    }

    return [pscustomobject]@{
        running = $running
        reported_version = $reportedVersion
        expected_version = $ExpectedVersion
        healthy = $running -and $reportedVersion -eq $ExpectedVersion
    }
}

function Wait-BoundlessDaemonApi {
    param(
        [string]$CliPath,
        [string]$ExpectedVersion,
        [int]$TimeoutSeconds = 30
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $lastResult = $null
    $lastStatus = $null
    do {
        $lastResult = Invoke-BoundedProcess -FilePath $CliPath -ArgumentList @("daemon", "status") -TimeoutSeconds 5
        if ($lastResult.exit_code -eq 0) {
            $lastStatus = ConvertFrom-BoundlessDaemonStatusOutput `
                -Output $lastResult.stdout `
                -ExpectedVersion $ExpectedVersion
            if ($lastStatus.healthy) {
                return $lastStatus
            }
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    $detail = if ($null -eq $lastResult) {
        "no status attempt completed"
    }
    else {
        $reportedVersion = if ($null -eq $lastStatus -or [string]::IsNullOrWhiteSpace($lastStatus.reported_version)) {
            "missing"
        }
        else {
            $lastStatus.reported_version
        }
        "exit_code=$($lastResult.exit_code) reported_version=$reportedVersion expected_version=$ExpectedVersion stderr=$($lastResult.stderr)"
    }
    throw "Boundless daemon API did not become healthy within $($TimeoutSeconds)s; $detail"
}

function Get-BoundlessVersionFromOutput {
    param(
        [string]$Output,
        [string]$ExecutableName
    )

    $match = [regex]::Match(
        $Output,
        "(?m)^\s*$([regex]::Escape($ExecutableName))\s+(?<version>[^\s]+)\s*$"
    )
    if (-not $match.Success) {
        throw "$ExecutableName --version returned an unexpected value: '$Output'"
    }
    return $match.Groups['version'].Value
}

function Get-BoundlessExecutableVersion {
    param(
        [string]$Path,
        [string]$ExecutableName,
        [int]$TimeoutSeconds = 10
    )

    $result = Invoke-BoundedProcess -FilePath $Path -ArgumentList @("--version") -TimeoutSeconds $TimeoutSeconds
    if ($result.exit_code -ne 0) {
        throw "$ExecutableName --version failed with exit code $($result.exit_code): $($result.stderr)"
    }
    return Get-BoundlessVersionFromOutput -Output $result.stdout -ExecutableName $ExecutableName
}

function Get-BoundlessTrayProcessesForCurrentSession {
    $sessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
    return @(
        Get-Process -Name "boundlesstray" -ErrorAction SilentlyContinue |
            Where-Object { $_.SessionId -eq $sessionId } |
            ForEach-Object {
                $path = try { $_.Path } catch { "" }
                $responding = try { $_.Responding } catch { $false }
                [pscustomobject]@{
                    id = $_.Id
                    session_id = $_.SessionId
                    owner_sid = Get-ProcessOwnerSid -ProcessId $_.Id
                    path = $path
                    responding = $responding
                }
            }
    )
}

function Test-WindowsPathEqual {
    param(
        [string]$Left,
        [string]$Right
    )

    if ([string]::IsNullOrWhiteSpace($Left) -or [string]::IsNullOrWhiteSpace($Right)) {
        return $false
    }
    $leftFull = [IO.Path]::GetFullPath($Left).TrimEnd('\')
    $rightFull = [IO.Path]::GetFullPath($Right).TrimEnd('\')
    return $leftFull.Equals($rightFull, [StringComparison]::OrdinalIgnoreCase)
}

function Get-WindowsCommandExecutablePath {
    param([string]$CommandLine)

    if ([string]::IsNullOrWhiteSpace($CommandLine)) {
        throw "Windows command line was empty while parsing its executable."
    }
    $trimmed = $CommandLine.Trim()
    if ($trimmed.StartsWith('"')) {
        $match = [regex]::Match($trimmed, '^"(?<path>[^\"]+)"(?=\s|$)')
    }
    else {
        $match = [regex]::Match(
            $trimmed,
            '^(?<path>.+?\.exe)(?=\s|$)',
            [Text.RegularExpressions.RegexOptions]::IgnoreCase
        )
    }
    if (-not $match.Success) {
        throw "Could not parse an executable token from Windows command line: $CommandLine"
    }
    try {
        return [IO.Path]::GetFullPath($match.Groups['path'].Value).TrimEnd('\')
    }
    catch {
        throw "Windows command line executable path was invalid: $($match.Groups['path'].Value)"
    }
}

function Assert-WindowsServiceExecutablePathFixtures {
    $expected = 'C:\Program Files\Boundless\boundless-service.exe'
    foreach ($commandLine in @(
        '"C:\Program Files\Boundless\boundless-service.exe" --allowed-user-sid=S-1-5-21-1',
        'C:\Program Files\Boundless\boundless-service.exe --allowed-user-sid=S-1-5-21-1'
    )) {
        $actual = Get-WindowsCommandExecutablePath -CommandLine $commandLine
        if (-not (Test-WindowsPathEqual -Left $actual -Right $expected)) {
            throw "Service executable parser fixture did not accept the exact executable token: $commandLine"
        }
    }
    foreach ($commandLine in @(
        '"C:\Program Files\Boundless\boundless-service.exe.evil" --allowed-user-sid=S-1-5-21-1',
        'C:\Program Files\Boundless\boundless-service.exe.evil --allowed-user-sid=S-1-5-21-1'
    )) {
        $accepted = $false
        try {
            $actual = Get-WindowsCommandExecutablePath -CommandLine $commandLine
            $accepted = Test-WindowsPathEqual -Left $actual -Right $expected
        }
        catch {
            $accepted = $false
        }
        if ($accepted) {
            throw "Service executable parser fixture accepted a suffix-confused executable: $commandLine"
        }
    }
}

function Assert-SoleBoundlessTraySnapshot {
    param(
        [object[]]$Processes,
        [string]$ExpectedTrayPath,
        [string]$Phase
    )

    $processes = @($Processes)
    if ($processes.Count -gt 1) {
        throw "Expected at most one Boundless tray $Phase, found $($processes.Count) in the current session."
    }
    if ($processes.Count -eq 0) {
        return $null
    }

    $process = $processes[0]
    if (-not (Test-WindowsPathEqual -Left $process.path -Right $ExpectedTrayPath)) {
        throw "Boundless tray $Phase was running from an unexpected path. Expected '$ExpectedTrayPath', got '$($process.path)'. Close the old or portable tray and retry."
    }
    return $process
}

function Ensure-OneBoundlessTray {
    param(
        [string]$TrayPath,
        [string]$InstallRoot,
        [int]$TimeoutSeconds = 15,
        [int]$StableMilliseconds = 2000
    )

    $expectedTrayPath = [IO.Path]::GetFullPath($TrayPath)
    $existing = Assert-SoleBoundlessTraySnapshot `
        -Processes @(Get-BoundlessTrayProcessesForCurrentSession) `
        -ExpectedTrayPath $expectedTrayPath `
        -Phase "before launch"
    $launchedProcess = $null
    if ($null -eq $existing) {
        $launchedProcess = Start-Process -FilePath $expectedTrayPath -WorkingDirectory $InstallRoot -PassThru
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $stableSince = $null
    $stableProcessId = $null
    do {
        $process = Assert-SoleBoundlessTraySnapshot `
            -Processes @(Get-BoundlessTrayProcessesForCurrentSession) `
            -ExpectedTrayPath $expectedTrayPath `
            -Phase "during readiness verification"
        if ($null -ne $process -and $process.responding) {
            if ($stableProcessId -ne $process.id) {
                $stableProcessId = $process.id
                $stableSince = Get-Date
            }
            $stableFor = [int]((Get-Date) - $stableSince).TotalMilliseconds
            if ($stableFor -ge $StableMilliseconds) {
                return [pscustomobject]@{
                    count = 1
                    process_id = $process.id
                    path = $process.path
                    path_matches = $true
                    responding = $true
                    stable_milliseconds = $stableFor
                }
            }
        }
        else {
            $stableProcessId = $null
            $stableSince = $null
            if ($null -ne $launchedProcess -and $launchedProcess.HasExited) {
                throw "Boundless tray exited before it remained ready for $($StableMilliseconds)ms; exit_code=$($launchedProcess.ExitCode)."
            }
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)

    throw "Boundless tray did not remain single, responsive, and path-correct for $($StableMilliseconds)ms within $($TimeoutSeconds)s."
}

function Test-ManifestVersionMatchesMsi {
    param(
        [string]$ManifestVersion,
        [string]$MsiVersion
    )

    return $ManifestVersion -eq $MsiVersion -or
        $ManifestVersion.StartsWith("$MsiVersion-") -or
        $ManifestVersion.StartsWith("$MsiVersion+")
}

function Assert-PostInstallEvidence {
    param([object]$Evidence)

    if (-not $Evidence.product_registered) {
        throw "Windows Installer did not register the MSI product after reporting success."
    }
    if ($Evidence.display_version -ne $Evidence.msi_version) {
        throw "Installed DisplayVersion '$($Evidence.display_version)' did not match MSI ProductVersion '$($Evidence.msi_version)'."
    }
    if (-not (Test-ManifestVersionMatchesMsi -ManifestVersion $Evidence.manifest_version -MsiVersion $Evidence.msi_version)) {
        throw "Installed package-manifest version '$($Evidence.manifest_version)' did not match MSI ProductVersion '$($Evidence.msi_version)'."
    }
    if ($Evidence.service_allowed_user_sid -ne $Evidence.expected_allowed_user_sid) {
        throw "BoundlessService allowed-user SID mismatch. Expected $($Evidence.expected_allowed_user_sid), got $($Evidence.service_allowed_user_sid)."
    }
    if (-not $Evidence.service_binary_path_matches) {
        throw "BoundlessService command line did not reference the installed Program Files service binary."
    }
    if ($Evidence.service_status -ne "Running") {
        throw "BoundlessService was not Running after install; current=$($Evidence.service_status)."
    }
    if (-not $Evidence.daemon_api_healthy) {
        throw "Boundless daemon API was not healthy after install."
    }
    if ($Evidence.daemon_runtime_version -ne $Evidence.expected_runtime_version) {
        throw "Boundless daemon runtime version '$($Evidence.daemon_runtime_version)' did not match installed version '$($Evidence.expected_runtime_version)'."
    }
    if (-not $Evidence.executable_versions_match) {
        throw "One or more installed Boundless executables did not report the installed package version."
    }
    if ($Evidence.input_injector_signature_status -notin @("Valid", "NotSigned")) {
        throw "Installed elevated input injector signature status '$($Evidence.input_injector_signature_status)' was neither Valid nor the explicit NotSigned dogfood exception."
    }
    $expectedUnsignedDogfood = $Evidence.input_injector_signature_status -eq "NotSigned"
    if ([bool]$Evidence.input_injector_unsigned_dogfood -ne $expectedUnsignedDogfood) {
        throw "Installed elevated input injector signature classification was inconsistent with '$($Evidence.input_injector_signature_status)'."
    }
    if ($Evidence.tray_verification -eq "passed" -and $Evidence.tray_count -ne 1) {
        throw "Expected exactly one Boundless tray after install, found $($Evidence.tray_count)."
    }
    if ($Evidence.tray_verification -eq "passed" -and -not $Evidence.tray_path_matches) {
        throw "The sole Boundless tray did not run from the installed Program Files path."
    }
    if ($Evidence.tray_verification -eq "passed" -and -not $Evidence.tray_responding) {
        throw "The sole Boundless tray did not remain responsive during readiness verification."
    }
    if ($Evidence.tray_verification -eq "passed" -and $Evidence.tray_stable_milliseconds -lt 2000) {
        throw "The sole Boundless tray was not stable for the required 2000ms readiness interval."
    }
    if ($Evidence.tray_verification -notin @("passed", "deferred_elevated_or_quiet")) {
        throw "Unexpected tray verification status '$($Evidence.tray_verification)'."
    }
    return $Evidence
}

function Invoke-PostInstallVerification {
    param(
        [object]$InstallerAnchor,
        [string]$ExpectedAllowedUserSid,
        [bool]$LaunchTray
    )

    if ($null -eq $InstallerAnchor) {
        throw "Post-install verification requires the pre-UAC MSI identity anchor."
    }
    $msiVersion = $InstallerAnchor.product_version
    $productCode = $InstallerAnchor.product_code
    $uninstallEntry = Get-BoundlessUninstallEntry -ProductCode $productCode
    $productRegistered = $null -ne $uninstallEntry
    if (-not $productRegistered) {
        throw "Windows Installer product $productCode was not registered after msiexec reported success."
    }

    $installRoot = if (
        $uninstallEntry.PSObject.Properties.Match("InstallLocation").Count -gt 0 -and
        -not [string]::IsNullOrWhiteSpace($uninstallEntry.InstallLocation)
    ) {
        $uninstallEntry.InstallLocation
    }
    else {
        Join-Path $env:ProgramFiles "Boundless"
    }
    $manifestPath = Join-Path $installRoot "package-manifest.json"
    if (-not (Test-Path -LiteralPath $manifestPath)) {
        throw "Installed package manifest was missing: $manifestPath"
    }
    $manifestVersion = (Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json).version

    $service = Wait-BoundlessServiceRunning
    $serviceConfig = Get-CimInstance -ClassName Win32_Service -Filter "Name='BoundlessService'" -ErrorAction Stop |
        Select-Object -First 1
    if ($null -eq $serviceConfig) {
        throw "BoundlessService configuration was unavailable after install."
    }
    $sidMatches = [regex]::Matches($serviceConfig.PathName, "--allowed-user-sid=([^\s]+)")
    if ($sidMatches.Count -ne 1) {
        throw "BoundlessService command line did not contain exactly one --allowed-user-sid argument. PathName=$($serviceConfig.PathName)"
    }
    $serviceAllowedUserSid = $sidMatches[0].Groups[1].Value.Trim('"')
    $expectedServicePath = Join-Path $installRoot "boundless-service.exe"
    $actualServicePath = Get-WindowsCommandExecutablePath -CommandLine $serviceConfig.PathName
    $serviceBinaryPathMatches = Test-WindowsPathEqual `
        -Left $actualServicePath `
        -Right $expectedServicePath

    $cliPath = Join-Path $installRoot "boundlessctl.exe"
    $trayPath = Join-Path $installRoot "boundlesstray.exe"
    $daemonPath = Join-Path $installRoot "boundlessd.exe"
    $servicePath = Join-Path $installRoot "boundless-service.exe"
    $inputInjectorPath = Join-Path $installRoot "boundless-input-injector.exe"
    foreach ($requiredPath in @($cliPath, $trayPath, $daemonPath, $servicePath, $inputInjectorPath)) {
        if (-not (Test-Path -LiteralPath $requiredPath)) {
            throw "Installed Boundless payload was missing: $requiredPath"
        }
    }

    $installedManifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    if (
        $installedManifest.executables.PSObject.Properties.Match("input_injector").Count -ne 1 -or
        $installedManifest.executables.input_injector -ne "boundless-input-injector.exe"
    ) {
        throw "Installed package manifest did not identify the elevated input injector payload."
    }
    $inputInjectorSignature = Get-AuthenticodeSignature -LiteralPath $inputInjectorPath

    $reportedExecutableVersions = [ordered]@{
        boundlessctl = Get-BoundlessExecutableVersion -Path $cliPath -ExecutableName "boundlessctl"
        boundlesstray = Get-BoundlessExecutableVersion -Path $trayPath -ExecutableName "boundlesstray"
        boundlessd = Get-BoundlessExecutableVersion -Path $daemonPath -ExecutableName "boundlessd"
        boundless_service = Get-BoundlessExecutableVersion -Path $servicePath -ExecutableName "boundless-service"
    }
    $executableVersionsMatch = @($reportedExecutableVersions.Values | Where-Object { $_ -ne $manifestVersion }).Count -eq 0

    $daemonApi = Wait-BoundlessDaemonApi -CliPath $cliPath -ExpectedVersion $manifestVersion
    if ($LaunchTray) {
        $trayEvidence = Ensure-OneBoundlessTray -TrayPath $trayPath -InstallRoot $installRoot
        $trayCount = $trayEvidence.count
        $trayPathMatches = $trayEvidence.path_matches
        $trayResponding = $trayEvidence.responding
        $trayStableMilliseconds = $trayEvidence.stable_milliseconds
        $trayVerification = "passed"
    }
    else {
        $existingTray = Assert-SoleBoundlessTraySnapshot `
            -Processes @(Get-BoundlessTrayProcessesForCurrentSession) `
            -ExpectedTrayPath $trayPath `
            -Phase "during deferred verification"
        $trayCount = if ($null -eq $existingTray) { 0 } else { 1 }
        $trayPathMatches = $null -ne $existingTray
        $trayResponding = $null -ne $existingTray -and $existingTray.responding
        $trayStableMilliseconds = 0
        $trayVerification = "deferred_elevated_or_quiet"
    }

    $evidence = [pscustomobject]@{
        product_registered = $productRegistered
        product_code = $productCode
        msi_version = $msiVersion
        display_version = $uninstallEntry.DisplayVersion
        manifest_version = $manifestVersion
        service_allowed_user_sid = $serviceAllowedUserSid
        expected_allowed_user_sid = $ExpectedAllowedUserSid
        service_binary_path_matches = $serviceBinaryPathMatches
        service_status = $service.Status.ToString()
        daemon_api_healthy = $daemonApi.healthy
        daemon_runtime_version = $daemonApi.reported_version
        expected_runtime_version = $manifestVersion
        executable_versions_match = $executableVersionsMatch
        executable_versions = $reportedExecutableVersions
        input_injector_path = $inputInjectorPath
        input_injector_signature_status = $inputInjectorSignature.Status.ToString()
        input_injector_unsigned_dogfood = $inputInjectorSignature.Status.ToString() -eq "NotSigned"
        tray_count = $trayCount
        tray_path_matches = $trayPathMatches
        tray_responding = $trayResponding
        tray_stable_milliseconds = $trayStableMilliseconds
        tray_verification = $trayVerification
    }
    return Assert-PostInstallEvidence -Evidence $evidence
}

function Invoke-BoundlessInstallerSupervisionFixture {
    param([string]$UserSid)

    $control = New-BoundlessInstallerControlEvent -UserSid $UserSid
    $completion = New-BoundlessInstallerCompletionEvent -UserSid $UserSid
    $serviceInitial = New-BoundlessInstallerPhaseEvent `
        -UserSid $UserSid `
        -Phase "ServiceInitialRunning"
    $msiMayHaveStarted = New-BoundlessInstallerPhaseEvent `
        -UserSid $UserSid `
        -Phase "MsiMayHaveStarted"
    $msiDefinitive = New-BoundlessInstallerPhaseEvent `
        -UserSid $UserSid `
        -Phase "MsiDefinitiveCompletion"
    $msiIdleProven = New-BoundlessInstallerPhaseEvent `
        -UserSid $UserSid `
        -Phase "MsiIdleProven"
    $recoverySignal = New-BoundlessSentinelOwnerEvent `
        -Prefix "Boundless.Test.ParentHardKillRecovery.v1" `
        -UserSid $UserSid
    $serviceStoppedSignal = New-BoundlessSentinelOwnerEvent `
        -Prefix "Boundless.Test.ParentHardKillServiceStopped.v1" `
        -UserSid $UserSid
    $treeJobName = "Local\Boundless.Installer.Tree.v1.$([guid]::NewGuid().ToString('N'))"
    $installer = $null
    $monitorProcess = $null
    $failedInstaller = $null
    $failedMonitorProcess = $null
    $failedControl = $null
    $failedCompletion = $null
    $failedHeartbeat = $null
    $failedLaunchAttempts = $null
    $failedBrokerReady = $null
    $failedBrokerServiceStart = $null
    $failedBrokerState = [pscustomobject]@{ process = $null }
    $heartbeat = [Threading.EventWaitHandle]::new(
        $true,
        [Threading.EventResetMode]::ManualReset
    )
    try {
        [void]$serviceInitial.event.Set()
        $stoppedPayload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes($serviceStoppedSignal.name)
        )
        $installerSource = @'
$name = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__EVENT__"))
$event = [Threading.EventWaitHandle]::OpenExisting($name)
try { [void]$event.Set() } finally { $event.Dispose() }
Start-Sleep -Seconds 30
'@.Replace("__EVENT__", $stoppedPayload)
        $monitorSource = 'Start-Sleep -Milliseconds 500; exit 17'
        $installer = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($installerSource))
            ) `
            -WindowStyle Hidden `
            -PassThru
        if (-not $serviceStoppedSignal.event.WaitOne(5000)) {
            throw "Installer supervision fixture wrapper did not publish its simulated service stop."
        }
        $monitorProcess = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($monitorSource))
            ) `
            -WindowStyle Hidden `
            -PassThru
        $monitor = [pscustomobject]@{
            process = $monitorProcess
            heartbeat_event = $heartbeat
        }
        $stopwatch = [Diagnostics.Stopwatch]::StartNew()
        $failure = $null
        $treeClosureState = [pscustomobject]@{ confirmed = $false }
        $recoveryPayload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes($recoverySignal.name)
        )
        $recoveryLauncherSource = @'
$name = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__EVENT__"))
$event = [Threading.EventWaitHandle]::OpenExisting($name)
try { [void]$event.Set() } finally { $event.Dispose() }
'@.Replace("__EVENT__", $recoveryPayload)
        $fixtureLease = [pscustomobject]@{
            expected_owner_sid = $UserSid
            service_initial_running_event = $serviceInitial.event
            msi_may_have_started_event = $msiMayHaveStarted.event
            msi_definitive_completion_event = $msiDefinitive.event
            msi_idle_proven_event = $msiIdleProven.event
        }
        try {
            Wait-BoundlessElevatedInstallSupervised `
                -InstallerProcess $installer `
                -Monitor $monitor `
                -CancellationEvent $control.event `
                -CompletionEvent $completion.event `
                -TreeJobName $treeJobName `
                -TreeClosureState $treeClosureState `
                -HardKillRecoveryAction {
                    Restore-BoundlessServiceAfterHardKilledElevatedInstall `
                        -QuiescenceLease $fixtureLease `
                        -StagedHelperPath "fixture" `
                        -TimeoutMilliseconds 5000 `
                        -FixtureLauncherSource $recoveryLauncherSource `
                        -ServiceStatusProbe {
                            if ($recoverySignal.event.WaitOne(0)) { return "Running" }
                            if ($serviceStoppedSignal.event.WaitOne(0)) { return "Stopped" }
                            return "Unknown"
                        }
                } `
                -TimeoutSeconds 15 `
                -CancellationGraceMilliseconds 500 | Out-Null
        }
        catch {
            $failure = $_
        }
        $stopwatch.Stop()
        if (
            $null -eq $failure -or
            $failure.Exception.Message -notmatch 'quiescence monitor exited' -or
            -not $control.event.WaitOne(0) -or
            -not $installer.HasExited -or
            -not $treeClosureState.confirmed -or
            -not $treeClosureState.hard_kill_used -or
            -not $treeClosureState.parent_service_recovery_reconciled -or
            $treeClosureState.parent_service_recovery_status -ne "restored" -or
            -not $recoverySignal.event.WaitOne(0) -or
            $stopwatch.ElapsedMilliseconds -gt 7000
        ) {
            throw (
                "Installer supervision fixture did not hard-kill its wrapper and reconcile parent-owned service recovery before returning. " +
                "failure=$($failure.Exception.Message);control=$($control.event.WaitOne(0));installer_exited=$($installer.HasExited);" +
                "tree=$($treeClosureState | ConvertTo-Json -Compress);recovery_signal=$($recoverySignal.event.WaitOne(0));" +
                "elapsed=$($stopwatch.ElapsedMilliseconds)"
            )
        }

        $failedControl = New-BoundlessInstallerControlEvent -UserSid $UserSid
        $failedCompletion = New-BoundlessInstallerCompletionEvent -UserSid $UserSid
        $failedHeartbeat = [Threading.EventWaitHandle]::new(
            $true,
            [Threading.EventResetMode]::ManualReset
        )
        $sleepCommand = [Convert]::ToBase64String(
            [Text.Encoding]::Unicode.GetBytes('Start-Sleep -Seconds 30')
        )
        $failedInstaller = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @("-NoProfile", "-EncodedCommand", $sleepCommand) `
            -WindowStyle Hidden `
            -PassThru
        $failedMonitorProcess = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String(
                    [Text.Encoding]::Unicode.GetBytes('Start-Sleep -Milliseconds 250; exit 19')
                )
            ) `
            -WindowStyle Hidden `
            -PassThru
        $failedTreeState = [pscustomobject]@{ confirmed = $false }
        $failedLaunchAttemptName = "Local\Boundless.Test.RecoveryLaunchAttempt.$([guid]::NewGuid().ToString('N'))"
        $failedLaunchCreated = $false
        $failedLaunchAttempts = [Threading.Semaphore]::new(
            0,
            2,
            $failedLaunchAttemptName,
            [ref]$failedLaunchCreated
        )
        if (-not $failedLaunchCreated) {
            throw "Recovery launch-hang fixture collided with a named semaphore."
        }
        $failedLaunchPayload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes($failedLaunchAttemptName)
        )
        $failedLauncherSource = @'
$name = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__SEMAPHORE__"))
$attempt = [Threading.Semaphore]::OpenExisting($name)
try {
    [void]$attempt.Release()
    Start-Sleep -Seconds 30
}
finally { $attempt.Dispose() }
'@.Replace("__SEMAPHORE__", $failedLaunchPayload)
        $failedBrokerReady = New-BoundlessSentinelOwnerEvent `
            -Prefix "Boundless.Test.RecoveryBrokerReady.v1" `
            -UserSid $UserSid
        $failedBrokerServiceStart = New-BoundlessSentinelOwnerEvent `
            -Prefix "Boundless.Test.RecoveryBrokerServiceStart.v1" `
            -UserSid $UserSid
        $failedError = $null
        $failedStopwatch = [Diagnostics.Stopwatch]::StartNew()
        try {
            Wait-BoundlessElevatedInstallSupervised `
                -InstallerProcess $failedInstaller `
                -Monitor ([pscustomobject]@{
                    process = $failedMonitorProcess
                    heartbeat_event = $failedHeartbeat
                }) `
                -CancellationEvent $failedControl.event `
                -CompletionEvent $failedCompletion.event `
                -TreeJobName "Local\Boundless.Installer.Tree.v1.$([guid]::NewGuid().ToString('N'))" `
                -TreeClosureState $failedTreeState `
                -HardKillRecoveryAction {
                    # The fixture must allow a cold hosted PowerShell process
                    # to enter its script before proving the bounded hang path.
                    # Production recovery retains its separate 60-second default.
                    Restore-BoundlessServiceAfterHardKilledElevatedInstall `
                        -QuiescenceLease $fixtureLease `
                        -StagedHelperPath "fixture" `
                        -TimeoutMilliseconds 3000 `
                        -FixtureLauncherSource $failedLauncherSource `
                        -BeforeFixtureLauncherAction {
                            param($authority)
                            $brokerPayload = [Convert]::ToBase64String(
                                [Text.Encoding]::UTF8.GetBytes(
                                    "$($authority.job_name)`n$($authority.revocation_event_name)`n$($failedBrokerReady.name)`n$($failedBrokerServiceStart.name)"
                                )
                            )
                            $brokerSource = @'
Add-Type -TypeDefinition @"
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class BoundlessRecoveryBrokerFixtureNative
{
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern IntPtr OpenJobObjectW(uint access, bool inherit, string name);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    public static void Join(string name) {
        IntPtr job=OpenJobObjectW(5,false,name); if(job==IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        try { if(!AssignProcessToJobObject(job,GetCurrentProcess())) throw new Win32Exception(Marshal.GetLastWin32Error()); }
        finally { CloseHandle(job); }
    }
}
"@
$names = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__PAYLOAD__")) -split "`n"
$revoked = [Threading.EventWaitHandle]::OpenExisting($names[1])
$ready = [Threading.EventWaitHandle]::OpenExisting($names[2])
$serviceStart = [Threading.EventWaitHandle]::OpenExisting($names[3])
try {
    [BoundlessRecoveryBrokerFixtureNative]::Join($names[0])
    if ($revoked.WaitOne(0)) { exit 85 }
    [void]$ready.Set()
    Start-Sleep -Seconds 30
    if (-not $revoked.WaitOne(0)) { [void]$serviceStart.Set() }
}
finally { $serviceStart.Dispose(); $ready.Dispose(); $revoked.Dispose() }
'@.Replace("__PAYLOAD__", $brokerPayload)
                            $failedBrokerState.process = Start-Process `
                                -FilePath (Resolve-CurrentPowerShellExecutable) `
                                -ArgumentList @(
                                    "-NoProfile", "-EncodedCommand",
                                    [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($brokerSource))
                                ) `
                                -WindowStyle Hidden `
                                -PassThru
                            if (-not $failedBrokerReady.event.WaitOne(5000)) {
                                throw "Recovery broker fixture did not join its authority job."
                            }
                        } `
                        -ServiceStatusProbe { "Stopped" }
                } `
                -TimeoutSeconds 15 `
                -CancellationGraceMilliseconds 250 | Out-Null
        }
        catch {
            $failedError = $_
        }
        $failedStopwatch.Stop()
        $firstLaunchObserved = $failedLaunchAttempts.WaitOne(0)
        $secondLaunchObserved = $failedLaunchAttempts.WaitOne(0)
        $failedBrokerExited = (
            $null -ne $failedBrokerState.process -and
            $failedBrokerState.process.WaitForExit(5000)
        )
        $failedBrokerServiceStartObserved = $failedBrokerServiceStart.event.WaitOne(0)
        $failedInstallerExited = $failedInstaller.HasExited
        if (
            $null -eq $failedError -or
            $failedError.Exception.Message -notmatch 'quiescence monitor exited' -or
            $failedError.Exception.Message -notmatch 'elevation launch/execution exceeded 3000' -or
            -not $firstLaunchObserved -or
            $secondLaunchObserved -or
            -not $failedBrokerExited -or
            $failedBrokerServiceStartObserved -or
            -not $failedInstallerExited -or
            -not $failedTreeState.confirmed -or
            -not $failedTreeState.hard_kill_used -or
            $failedTreeState.parent_service_recovery_reconciled -or
            $failedTreeState.parent_service_recovery_status -ne "failed" -or
            $failedStopwatch.ElapsedMilliseconds -gt 12000
        ) {
            throw (
                "Recovery launch-hang fixture did not preserve both errors and exit after one bounded launch attempt. " +
                "error=$($failedError.Exception.Message);first_launch=$firstLaunchObserved;second_launch=$secondLaunchObserved;" +
                "broker_exited=$failedBrokerExited;service_start=$failedBrokerServiceStartObserved;" +
                "installer_exited=$failedInstallerExited;tree=$($failedTreeState | ConvertTo-Json -Compress);" +
                "elapsed=$($failedStopwatch.ElapsedMilliseconds)"
            )
        }
    }
    finally {
        if ($null -ne $installer) {
            if (-not $installer.HasExited) {
                Stop-BoundlessProcessBoundary -Process $installer
            }
            $installer.Dispose()
        }
        if ($null -ne $monitorProcess) {
            if (-not $monitorProcess.HasExited) {
                Stop-BoundlessProcessBoundary -Process $monitorProcess
            }
            $monitorProcess.Dispose()
        }
        foreach ($process in @($failedInstaller, $failedMonitorProcess)) {
            if ($null -eq $process) { continue }
            if (-not $process.HasExited) {
                Stop-BoundlessProcessBoundary -Process $process
            }
            $process.Dispose()
        }
        if ($null -ne $failedControl) { $failedControl.event.Dispose() }
        if ($null -ne $failedCompletion) { $failedCompletion.event.Dispose() }
        if ($null -ne $failedHeartbeat) { $failedHeartbeat.Dispose() }
        if ($null -ne $failedLaunchAttempts) { $failedLaunchAttempts.Dispose() }
        if ($null -ne $failedBrokerState.process) {
            if (-not $failedBrokerState.process.HasExited) {
                Stop-BoundlessProcessBoundary -Process $failedBrokerState.process
            }
            $failedBrokerState.process.Dispose()
        }
        if ($null -ne $failedBrokerReady) { $failedBrokerReady.event.Dispose() }
        if ($null -ne $failedBrokerServiceStart) { $failedBrokerServiceStart.event.Dispose() }
        $control.event.Dispose()
        $completion.event.Dispose()
        $msiMayHaveStarted.event.Dispose()
        $msiDefinitive.event.Dispose()
        $msiIdleProven.event.Dispose()
        $serviceInitial.event.Dispose()
        $recoverySignal.event.Dispose()
        $serviceStoppedSignal.event.Dispose()
        $heartbeat.Dispose()
    }
}

function Invoke-BoundlessMsiStartedHardKillRecoveryFixture {
    param([string]$UserSid)

    $fixtureId = [guid]::NewGuid().ToString('N')
    $control = New-BoundlessInstallerControlEvent -UserSid $UserSid
    $completion = New-BoundlessInstallerCompletionEvent -UserSid $UserSid
    $serviceInitial = New-BoundlessInstallerPhaseEvent -UserSid $UserSid -Phase "ServiceInitialRunning"
    $mayHaveStarted = New-BoundlessInstallerPhaseEvent -UserSid $UserSid -Phase "MsiMayHaveStarted"
    $definitive = New-BoundlessInstallerPhaseEvent -UserSid $UserSid -Phase "MsiDefinitiveCompletion"
    $idleProven = New-BoundlessInstallerPhaseEvent -UserSid $UserSid -Phase "MsiIdleProven"
    $recoverySignal = New-BoundlessSentinelOwnerEvent `
        -Prefix "Boundless.Test.DeferredServiceRecovery.v1" `
        -UserSid $UserSid
    $idleRaceSignal = New-BoundlessSentinelOwnerEvent `
        -Prefix "Boundless.Test.DeferredIdleRace.v1" `
        -UserSid $UserSid
    $transactionReady = New-BoundlessSentinelOwnerEvent `
        -Prefix "Boundless.Test.DeferredTransactionReady.v1" `
        -UserSid $UserSid
    $transactionMutexName = "Global\Boundless.Test.DeferredRecovery.$fixtureId"
    $holder = $null
    $installer = $null
    $monitorProcess = $null
    $heartbeat = [Threading.EventWaitHandle]::new(
        $true,
        [Threading.EventResetMode]::ManualReset
    )
    try {
        [void]$serviceInitial.event.Set()
        [void]$mayHaveStarted.event.Set()
        $holderPayload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes(
                "$transactionMutexName`n$($transactionReady.name)"
            )
        )
        $holderSource = @'
$names = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD__")
) -split "`n"
$ready = [Threading.EventWaitHandle]::OpenExisting($names[1])
$created = $false
$mutex = [Threading.Mutex]::new($true, $names[0], [ref]$created)
try {
    if (-not $created) { exit 81 }
    [void]$ready.Set()
    Start-Sleep -Milliseconds 1000
    $mutex.ReleaseMutex()
}
finally {
    $mutex.Dispose()
    $ready.Dispose()
}
'@.Replace("__PAYLOAD__", $holderPayload)
        $holder = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($holderSource))
            ) `
            -WindowStyle Hidden `
            -PassThru
        if (-not $transactionReady.event.WaitOne(5000)) {
            throw "Deferred recovery fixture did not hold its transaction mutex."
        }

        $sleepCommand = [Convert]::ToBase64String(
            [Text.Encoding]::Unicode.GetBytes('Start-Sleep -Seconds 30')
        )
        $installer = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @("-NoProfile", "-EncodedCommand", $sleepCommand) `
            -WindowStyle Hidden `
            -PassThru
        $monitorProcess = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String(
                    [Text.Encoding]::Unicode.GetBytes('Start-Sleep -Milliseconds 250; exit 29')
                )
            ) `
            -WindowStyle Hidden `
            -PassThru
        $launcherPayload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes(
                "$($idleRaceSignal.name)`n$($recoverySignal.name)"
            )
        )
        $launcherSource = @'
$names = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD__")
) -split "`n"
$idle = [Threading.EventWaitHandle]::OpenExisting($names[0])
$recovered = [Threading.EventWaitHandle]::OpenExisting($names[1])
try {
    if ($recovered.WaitOne(0)) { exit 82 }
    if (-not $idle.WaitOne(0)) { exit 84 }
    [void]$recovered.Set()
}
finally {
    $recovered.Dispose()
    $idle.Dispose()
}
'@.Replace("__PAYLOAD__", $launcherPayload)
        $lease = [pscustomobject]@{
            expected_owner_sid = $UserSid
            service_initial_running_event = $serviceInitial.event
            msi_may_have_started_event = $mayHaveStarted.event
            msi_definitive_completion_event = $definitive.event
            msi_idle_proven_event = $idleProven.event
        }
        $treeState = [pscustomobject]@{ confirmed = $false }
        $failure = $null
        $stopwatch = [Diagnostics.Stopwatch]::StartNew()
        try {
            Wait-BoundlessElevatedInstallSupervised `
                -InstallerProcess $installer `
                -Monitor ([pscustomobject]@{
                    process = $monitorProcess
                    heartbeat_event = $heartbeat
                }) `
                -CancellationEvent $control.event `
                -CompletionEvent $completion.event `
                -TreeJobName "Local\Boundless.Installer.Tree.v1.$([guid]::NewGuid().ToString('N'))" `
                -TreeClosureState $treeState `
                -HardKillRecoveryAction {
                    Restore-BoundlessServiceAfterHardKilledElevatedInstall `
                        -QuiescenceLease $lease `
                        -StagedHelperPath "fixture" `
                        -TimeoutMilliseconds 5000 `
                        -FixtureLauncherSource $launcherSource `
                        -BeforeFixtureLauncherAction {
                            if (-not (Wait-BoundlessWindowsInstallerTransactionIdleProof `
                                -TimeoutMilliseconds 5000 `
                                -MutexName $transactionMutexName)) {
                                throw "Deferred recovery fixture could not prove transaction idle."
                            }
                            [void]$idleProven.event.Set()
                            [void]$idleRaceSignal.event.Set()
                        } `
                        -ServiceStatusProbe {
                            if ($recoverySignal.event.WaitOne(0)) { "Running" } else { "Stopped" }
                        }
                } `
                -TimeoutSeconds 15 `
                -CancellationGraceMilliseconds 250 | Out-Null
        }
        catch { $failure = $_ }
        $stopwatch.Stop()
        if (
            $null -eq $failure -or
            $failure.Exception.Message -notmatch 'quiescence monitor exited' -or
            -not $installer.HasExited -or
            -not $treeState.confirmed -or
            -not $treeState.hard_kill_used -or
            -not $treeState.parent_service_recovery_reconciled -or
            $treeState.parent_service_recovery_status -ne "restored_after_msi_boundary" -or
            -not $idleProven.event.WaitOne(0) -or
            -not $recoverySignal.event.WaitOne(0) -or
            $stopwatch.ElapsedMilliseconds -gt 7000
        ) {
            throw (
                "MSI-started hard-kill fixture did not defer recovery through idle proof and restore exactly once. " +
                "failure=$($failure.Exception.Message);installer_exited=$($installer.HasExited);" +
                "tree=$($treeState | ConvertTo-Json -Compress);idle=$($idleProven.event.WaitOne(0));" +
                "recovered=$($recoverySignal.event.WaitOne(0));elapsed=$($stopwatch.ElapsedMilliseconds)"
            )
        }
        if (-not $holder.WaitForExit(5000) -or $holder.ExitCode -ne 0) {
            throw "Deferred recovery fixture transaction holder did not exit normally."
        }
    }
    finally {
        foreach ($process in @($installer, $monitorProcess, $holder)) {
            if ($null -eq $process) { continue }
            if (-not $process.HasExited) { Stop-BoundlessProcessBoundary -Process $process }
            $process.Dispose()
        }
        $heartbeat.Dispose()
        $transactionReady.event.Dispose()
        $idleRaceSignal.event.Dispose()
        $recoverySignal.event.Dispose()
        $idleProven.event.Dispose()
        $definitive.event.Dispose()
        $mayHaveStarted.event.Dispose()
        $serviceInitial.event.Dispose()
        $completion.event.Dispose()
        $control.event.Dispose()
    }
}

function Invoke-BoundlessInstallerHeartbeatStallFixture {
    param([string]$UserSid)

    $control = New-BoundlessInstallerControlEvent -UserSid $UserSid
    $completion = New-BoundlessInstallerCompletionEvent -UserSid $UserSid
    $heartbeat = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset
    )
    $installer = $null
    $monitorProcess = $null
    try {
        $sleepCommand = [Convert]::ToBase64String(
            [Text.Encoding]::Unicode.GetBytes('Start-Sleep -Seconds 30')
        )
        $installer = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @("-NoProfile", "-EncodedCommand", $sleepCommand) `
            -WindowStyle Hidden `
            -PassThru
        $monitorProcess = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @("-NoProfile", "-EncodedCommand", $sleepCommand) `
            -WindowStyle Hidden `
            -PassThru
        $treeClosureState = [pscustomobject]@{ confirmed = $false }
        $failure = $null
        $stopwatch = [Diagnostics.Stopwatch]::StartNew()
        try {
            Wait-BoundlessElevatedInstallSupervised `
                -InstallerProcess $installer `
                -Monitor ([pscustomobject]@{
                    process = $monitorProcess
                    heartbeat_event = $heartbeat
                }) `
                -CancellationEvent $control.event `
                -CompletionEvent $completion.event `
                -TreeJobName "Local\Boundless.Installer.Tree.v1.$([guid]::NewGuid().ToString('N'))" `
                -TreeClosureState $treeClosureState `
                -TimeoutSeconds 15 `
                -CancellationGraceMilliseconds 500 `
                -HeartbeatTimeoutMilliseconds 300 | Out-Null
        }
        catch {
            $failure = $_
        }
        $stopwatch.Stop()
        if (
            $null -eq $failure -or
            $failure.Exception.Message -notmatch 'heartbeat stalled' -or
            -not $control.event.WaitOne(0) -or
            -not $installer.HasExited -or
            -not $treeClosureState.confirmed -or
            $stopwatch.ElapsedMilliseconds -gt 7000
        ) {
            throw "Installer heartbeat-stall fixture did not cancel and drain its root fail-closed."
        }
    }
    finally {
        foreach ($process in @($installer, $monitorProcess)) {
            if ($null -eq $process) { continue }
            if (-not $process.HasExited) { Stop-BoundlessProcessBoundary -Process $process }
            $process.Dispose()
        }
        $heartbeat.Dispose()
        $completion.event.Dispose()
        $control.event.Dispose()
    }
}

function Invoke-BoundlessOwnedProcessTreeFixture {
    $fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
        "BoundlessOwnedTreeFixture-$([guid]::NewGuid().ToString('N'))"
    )
    $childPidPath = Join-Path $fixtureRoot "child.pid"
    $boundary = $null
    $childPid = 0
    try {
        New-Item -ItemType Directory -Path $fixtureRoot -Force -ErrorAction Stop | Out-Null
        $pathPayload = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($childPidPath))
        $childSource = 'Start-Sleep -Seconds 30'
        $rootSource = @'
$pidPath = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__PID_PATH__"))
$hostPath = (Get-Process -Id $PID -ErrorAction Stop).Path
$child = Start-Process -FilePath $hostPath -ArgumentList @(
    "-NoProfile",
    "-EncodedCommand",
    "__CHILD_COMMAND__"
) -WindowStyle Hidden -PassThru
[IO.File]::WriteAllText($pidPath, [string]$child.Id)
Start-Sleep -Seconds 30
'@
        $rootSource = $rootSource.Replace("__PID_PATH__", $pathPayload).Replace(
            "__CHILD_COMMAND__",
            [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($childSource))
        )
        $boundary = Start-BoundlessOwnedProcessBoundary `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($rootSource))
            ) `
            -CreateNoWindow
        $deadline = (Get-Date).AddSeconds(10)
        while (-not (Test-Path -LiteralPath $childPidPath -PathType Leaf)) {
            if ($boundary.HasExited -or (Get-Date) -ge $deadline) {
                throw "Owned process-tree fixture did not publish its descendant PID."
            }
            Start-Sleep -Milliseconds 50
        }
        $childPid = [int](Get-Content -LiteralPath $childPidPath -Raw)
        if ($null -eq (Get-Process -Id $childPid -ErrorAction SilentlyContinue)) {
            throw "Owned process-tree fixture descendant exited before cancellation."
        }
        Stop-BoundlessProcessBoundary -Process $boundary -TimeoutMilliseconds 5000
        # Windows can keep a just-terminated process visible to Get-Process for
        # a short interval after the job has reported JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO.
        # Bound the convergence wait so the fixture still fails for a genuinely
        # live descendant without turning normal handle teardown into a CI flake.
        $convergenceDeadline = (Get-Date).AddSeconds(2)
        do {
            $descendant = Get-Process -Id $childPid -ErrorAction SilentlyContinue
            if ($boundary.ActiveProcessCount -eq 0 -and $null -eq $descendant) {
                break
            }
            Start-Sleep -Milliseconds 50
        } while ((Get-Date) -lt $convergenceDeadline)
        if (
            $boundary.ActiveProcessCount -ne 0 -or
            $null -ne $descendant
        ) {
            throw "Owned process-tree cancellation left descendant PID $childPid running."
        }
    }
    finally {
        if ($null -ne $boundary) {
            if ($boundary.ActiveProcessCount -gt 0) {
                Stop-BoundlessProcessBoundary -Process $boundary -TimeoutMilliseconds 5000
            }
            $boundary.Dispose()
        }
        if (Test-Path -LiteralPath $fixtureRoot) {
            Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

function Invoke-BoundlessKernelObjectAclFixture {
    param([string]$UserSid)

    $control = New-BoundlessInstallerControlEvent -UserSid $UserSid
    $completion = New-BoundlessInstallerCompletionEvent -UserSid $UserSid
    $phase = New-BoundlessInstallerPhaseEvent `
        -UserSid $UserSid `
        -Phase "MsiMayHaveStarted"
    $liveness = New-BoundlessPrivilegedLivenessMutex `
        -Name "Local\Boundless.Installer.Monitor.v1.$([guid]::NewGuid().ToString('N'))"
    try {
        $ownerReadControl = [uint32]0x00020000
        $genericAll = [uint32]0x10000000
        $synchronize = [uint32]0x00100000
        $privilegedRules = @(
            [pscustomobject]@{ sid = "S-1-3-4"; rights = $ownerReadControl },
            [pscustomobject]@{ sid = "S-1-5-18"; rights = $genericAll },
            [pscustomobject]@{ sid = "S-1-5-32-544"; rights = $genericAll }
        )
        $observableRules = @($privilegedRules) + @(
            [pscustomobject]@{ sid = $UserSid; rights = $synchronize }
        )
        $poisonSecurity = [Security.AccessControl.EventWaitHandleSecurity]::new()
        $poisonSecurity.SetSecurityDescriptorSddlForm(
            "D:P(A;;RC;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)" +
            "(A;;0x00100000;;;$UserSid)(A;ID;GA;;;WD)"
        )
        if (Test-BoundlessProtectedKernelObjectSecurity `
            -Security $poisonSecurity `
            -ExpectedRules $observableRules) {
            throw "Installer kernel-object ACL fixture accepted inherited Everyone full control."
        }
        foreach ($securityFixture in @(
                [pscustomobject]@{
                    name = "cancellation event"
                    security = $control.security
                    rules = $privilegedRules
                },
                [pscustomobject]@{
                    name = "completion event"
                    security = $completion.security
                    rules = $observableRules
                },
                [pscustomobject]@{
                    name = "phase event"
                    security = $phase.security
                    rules = $observableRules
                },
                [pscustomobject]@{
                    name = "liveness mutex"
                    security = $liveness.security
                    rules = $privilegedRules
                }
            )) {
            if (-not (Test-BoundlessProtectedKernelObjectSecurity `
                -Security $securityFixture.security `
                -ExpectedRules $securityFixture.rules)) {
                throw "Installer $($securityFixture.name) ACL fixture did not preserve its protected semantic rules."
            }
        }
        [void]$control.event.Set()
        [void]$completion.event.Set()
        $currentTokenSid = [Security.Principal.WindowsIdentity]::GetCurrent().User
        $currentTokenCanUsePrivilegedAcl = (
            (Test-IsAdministrator) -or
            $currentTokenSid.IsWellKnown(
                [Security.Principal.WellKnownSidType]::LocalSystemSid
            )
        )
        $negativeMutationProbeRequired = -not $currentTokenCanUsePrivilegedAcl
        $payload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes(
                "$($control.name)`n$($completion.name)`n$($liveness.name)`n" +
                "$($phase.name)`n$([int]$negativeMutationProbeRequired)"
            )
        )
        $source = @'
$names = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD__")
) -split "`n"
function Open-EventWithRights([string]$Name, [object]$Rights) {
    $type = "System.Threading.EventWaitHandleAcl" -as [type]
    if ($null -ne $type) {
        $method = $type.GetMethods() | Where-Object {
            $_.Name -eq "OpenExisting" -and $_.GetParameters().Count -eq 2
        } | Select-Object -First 1
        return $method.Invoke($null, [object[]]@($Name, $Rights))
    }
    return [Threading.EventWaitHandle]::OpenExisting($Name, $Rights)
}
function Open-MutexWithRights([string]$Name, [object]$Rights) {
    $type = "System.Threading.MutexAcl" -as [type]
    if ($null -ne $type) {
        $method = $type.GetMethods() | Where-Object {
            $_.Name -eq "OpenExisting" -and $_.GetParameters().Count -eq 2
        } | Select-Object -First 1
        return $method.Invoke($null, [object[]]@($Name, $Rights))
    }
    return [Threading.Mutex]::OpenExisting($Name, $Rights)
}
$sync = Open-EventWithRights `
    -Name $names[1] `
    -Rights ([Security.AccessControl.EventWaitHandleRights]::Synchronize)
$sync.Dispose()
$phaseSync = Open-EventWithRights `
    -Name $names[3] `
    -Rights ([Security.AccessControl.EventWaitHandleRights]::Synchronize)
$phaseSync.Dispose()
if ([int]$names[4] -eq 1) {
    foreach ($probe in @(
            { Open-EventWithRights -Name $names[0] -Rights ([Security.AccessControl.EventWaitHandleRights]::ChangePermissions) },
            { Open-EventWithRights -Name $names[1] -Rights ([Security.AccessControl.EventWaitHandleRights]::ChangePermissions) },
            { Open-MutexWithRights -Name $names[2] -Rights ([Security.AccessControl.MutexRights]::ChangePermissions) },
            { Open-EventWithRights -Name $names[3] -Rights ([Security.AccessControl.EventWaitHandleRights]::Modify) }
        )) {
        $opened = $null
        try { $opened = & $probe }
        catch { continue }
        finally { if ($null -ne $opened) { $opened.Dispose() } }
        exit 71
    }
}
exit 0
'@.Replace("__PAYLOAD__", $payload)
        $result = Invoke-BoundedProcess `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($source))
            ) `
            -TimeoutSeconds 10
        if ($result.exit_code -ne 0) {
            throw "Installer kernel-object ACL fixture failed; negative_probe_required=$negativeMutationProbeRequired exit=$($result.exit_code) stderr='$($result.stderr)'."
        }
        if ($negativeMutationProbeRequired) {
            return "passed"
        }
        return "skipped_privileged_token"
    }
    finally {
        try { $liveness.mutex.ReleaseMutex() } catch { }
        $liveness.mutex.Dispose()
        $completion.event.Dispose()
        $phase.event.Dispose()
        $control.event.Dispose()
    }
}

function Invoke-BoundlessElevatedJobSourceFixture {
    param(
        [string]$Source,
        [string]$UserSid
    )

    $match = [regex]::Match(
        $Source,
        '(?s)Add-Type -TypeDefinition @"\r?\n(?<code>.*?)\r?\n"@'
    )
    if (-not $match.Success) {
        throw "Elevated process-job fixture could not extract its in-memory native boundary."
    }
    if ($null -eq ("BoundlessElevatedJob" -as [type])) {
        Add-Type -TypeDefinition $match.Groups["code"].Value
    }

    $ownedTreeSddl = Get-BoundlessOwnedTreeSddl -UserSid $UserSid
    $exitCodeJob = $null
    $exitCodeProcess = $null
    try {
        $exitCodeJob = [BoundlessElevatedJob]::Create(
            "Local\Boundless.Installer.Tree.v1.$([guid]::NewGuid().ToString('N'))",
            $ownedTreeSddl
        )
        $hostPath = Resolve-CurrentPowerShellExecutable
        $encodedExit = [Convert]::ToBase64String(
            [Text.Encoding]::Unicode.GetBytes("exit 37")
        )
        $exitCommandLine = @(
            ConvertTo-ProcessArgument -Value $hostPath
            "-NoProfile"
            "-EncodedCommand"
            $encodedExit
        ) -join " "
        $exitCodeProcess = $exitCodeJob.StartOwned($hostPath, $exitCommandLine)
        if (
            -not $exitCodeProcess.WaitForExit(5000) -or
            $exitCodeJob.Active -ne 0 -or
            $exitCodeJob.RootExitCode -ne 37
        ) {
            throw "Elevated process-job fixture lost the native root exit code."
        }
    }
    finally {
        if ($null -ne $exitCodeProcess) { $exitCodeProcess.Dispose() }
        if ($null -ne $exitCodeJob) { $exitCodeJob.Dispose() }
    }

    $fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
        "BoundlessElevatedJobFixture-$([guid]::NewGuid().ToString('N'))"
    )
    $childPidPath = Join-Path $fixtureRoot "child.pid"
    $gateName = "Local\Boundless.Test.ElevatedJobGate.$([guid]::NewGuid().ToString('N'))"
    $gateCreated = $false
    $gate = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $gateName,
        [ref]$gateCreated
    )
    $job = $null
    $root = $null
    try {
        New-Item -ItemType Directory -Path $fixtureRoot -Force -ErrorAction Stop | Out-Null
        $payload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes("$gateName`n$childPidPath")
        )
        $rootSource = @'
$values = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__PAYLOAD__")) -split "`n"
$gate = [Threading.EventWaitHandle]::OpenExisting($values[0])
try {
    if (-not $gate.WaitOne(10000)) { exit 51 }
    $hostPath = (Get-Process -Id $PID -ErrorAction Stop).Path
    $child = Start-Process -FilePath $hostPath -ArgumentList @(
        "-NoProfile", "-EncodedCommand", "__CHILD__"
    ) -WindowStyle Hidden -PassThru
    [IO.File]::WriteAllText($values[1], [string]$child.Id)
    Start-Sleep -Seconds 30
}
finally { $gate.Dispose() }
'@.Replace("__PAYLOAD__", $payload).Replace(
            "__CHILD__",
            [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('Start-Sleep -Seconds 30'))
        )
        $jobName = "Local\Boundless.Installer.Tree.v1.$([guid]::NewGuid().ToString('N'))"
        $job = [BoundlessElevatedJob]::Create(
            $jobName,
            $ownedTreeSddl
        )
        $hostPath = Resolve-CurrentPowerShellExecutable
        $rootArguments = @(
            "-NoProfile",
            "-EncodedCommand",
            [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($rootSource))
        )
        $rootCommandLine = @(
            ConvertTo-ProcessArgument -Value $hostPath
            @($rootArguments | ForEach-Object { ConvertTo-ProcessArgument -Value $_ })
        ) -join " "
        $root = $job.StartOwned($hostPath, $rootCommandLine)
        [void]$gate.Set()
        $deadline = (Get-Date).AddSeconds(10)
        while (-not (Test-Path -LiteralPath $childPidPath -PathType Leaf)) {
            if ($root.HasExited -or (Get-Date) -ge $deadline) {
                throw "Elevated process-job fixture did not publish its descendant PID."
            }
            Start-Sleep -Milliseconds 50
        }
        $childPid = [int](Get-Content -LiteralPath $childPidPath -Raw)
        $job.Terminate()
        $deadline = (Get-Date).AddSeconds(5)
        while ($job.Active -gt 0 -and (Get-Date) -lt $deadline) {
            Start-Sleep -Milliseconds 20
        }
        $processDeadline = (Get-Date).AddSeconds(2)
        while (
            $null -ne (Get-Process -Id $childPid -ErrorAction SilentlyContinue) -and
            (Get-Date) -lt $processDeadline
        ) {
            Start-Sleep -Milliseconds 20
        }
        if (
            $job.Active -ne 0 -or
            -not $root.WaitForExit(5000) -or
            $null -ne (Get-Process -Id $childPid -ErrorAction SilentlyContinue)
        ) {
            throw "Elevated process-job fixture left a staged helper descendant running."
        }
    }
    finally {
        if ($null -ne $job) {
            if ($job.Active -gt 0) { $job.Terminate() }
            $job.Dispose()
        }
        if ($null -ne $root) {
            if (-not $root.HasExited) { Stop-BoundlessProcessBoundary -Process $root }
            $root.Dispose()
        }
        $gate.Dispose()
        if (Test-Path -LiteralPath $fixtureRoot) {
            Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

function Invoke-BoundlessCoordinatorDeathFixture {
    param([string]$UserSid)

    $fixtureId = [guid]::NewGuid().ToString('N')
    $sessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
    $fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) "BoundlessCoordinatorDeath-$fixtureId"
    $fakeTrayName = "BoundlessFixtureTray$($fixtureId.Substring(0, 8))"
    $fakeTrayPath = Join-Path $fixtureRoot "$fakeTrayName.exe"
    $fakeTrayReadyName = "Local\Boundless.Test.CoordinatorReplacementReady.$fixtureId"
    $fakeTrayReadyCreated = $false
    $fakeTrayReady = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $fakeTrayReadyName,
        [ref]$fakeTrayReadyCreated
    )
    $sentinelName = Get-BoundlessTrayQuiescenceSentinelName `
        -UserSid $UserSid `
        -SessionId $sessionId
    $readyName = "Local\Boundless.Test.CoordinatorReady.$fixtureId"
    $readyCreated = $false
    $ready = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $readyName,
        [ref]$readyCreated
    )
    $completion = New-BoundlessInstallerCompletionEvent -UserSid $UserSid
    $treeJobName = "Local\Boundless.Installer.Tree.v1.$fixtureId"
    $coordinator = $null
    $monitor = $null
    $tree = $null
    $fakeTray = $null
    try {
        if (-not $readyCreated -or -not $fakeTrayReadyCreated) {
            throw "Coordinator-death fixture ready event collided."
        }
        New-Item -ItemType Directory -Path $fixtureRoot -Force -ErrorAction Stop | Out-Null
        $windowsPowerShell = Join-Path `
            $env:SystemRoot `
            "System32\WindowsPowerShell\v1.0\powershell.exe"
        Copy-Item -LiteralPath $windowsPowerShell -Destination $fakeTrayPath -ErrorAction Stop
        $payload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes("$sentinelName`n$readyName")
        )
        $coordinatorSource = @'
$values = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__PAYLOAD__")) -split "`n"
$created = $false
$sentinel = [Threading.Mutex]::new($true, $values[0], [ref]$created)
$ready = [Threading.EventWaitHandle]::OpenExisting($values[1])
try {
    if (-not $created) { exit 41 }
    [void]$ready.Set()
    Start-Sleep -Seconds 30
}
finally {
    $ready.Dispose()
    try { $sentinel.ReleaseMutex() } catch { }
    $sentinel.Dispose()
}
'@.Replace("__PAYLOAD__", $payload)
        $coordinator = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($coordinatorSource))
            ) `
            -WindowStyle Hidden `
            -PassThru
        if (-not $ready.WaitOne(10000)) {
            throw "Coordinator-death fixture did not publish its sentinel."
        }

        $monitor = Start-BoundlessTrayQuiescenceMonitor `
            -ExpectedOwnerSid $UserSid `
            -ExpectedSessionId $sessionId `
            -SentinelName $sentinelName `
            -TreeJobName $treeJobName `
            -CompletionEventName $completion.name `
            -FixtureProcessName $fakeTrayName
        Wait-BoundlessTrayQuiescenceMonitorReady -Monitor $monitor -TimeoutSeconds 10

        $treeRootSource = @'
$hostPath = (Get-Process -Id $PID -ErrorAction Stop).Path
[void](Start-Process -FilePath $hostPath -ArgumentList @(
    "-NoProfile", "-EncodedCommand", "__CHILD__"
) -WindowStyle Hidden -PassThru)
Start-Sleep -Seconds 30
'@.Replace(
            "__CHILD__",
            [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('Start-Sleep -Seconds 30'))
        )
        $tree = Start-BoundlessOwnedProcessBoundary `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($treeRootSource))
            ) `
            -CreateNoWindow `
            -JobName $treeJobName `
            -JobSddl (Get-BoundlessOwnedTreeSddl -UserSid $UserSid)
        $treeDeadline = (Get-Date).AddSeconds(5)
        while ($tree.ActiveProcessCount -lt 2 -and (Get-Date) -lt $treeDeadline) {
            Start-Sleep -Milliseconds 50
        }
        if ($tree.ActiveProcessCount -lt 2) {
            throw "Coordinator-death fixture did not create a descendant tree."
        }

        Stop-BoundlessProcessBoundary -Process $coordinator -TimeoutMilliseconds 5000
        Start-Sleep -Milliseconds 500
        if ($monitor.process.HasExited) {
            throw "Quiescence monitor exited before the active installer tree drained after coordinator death."
        }
        $sentinelProbe = [Threading.Mutex]::OpenExisting($sentinelName)
        $sentinelProbe.Dispose()

        $fakeTrayReadyPayload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes($fakeTrayReadyName)
        )
        $fakeTraySource = @'
Add-Type -AssemblyName System.Windows.Forms
$name = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__READY__"))
$ready = [Threading.EventWaitHandle]::OpenExisting($name)
try {
    [void]$ready.Set()
    [Windows.Forms.Application]::Run()
}
finally { $ready.Dispose() }
'@.Replace("__READY__", $fakeTrayReadyPayload)
        $fakeTray = Start-Process `
            -FilePath $fakeTrayPath `
            -ArgumentList @(
                "-NoProfile",
                "-STA",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($fakeTraySource))
            ) `
            -WindowStyle Hidden `
            -PassThru
        if (-not $fakeTrayReady.WaitOne(10000)) {
            throw "Coordinator-death fixture replacement tray did not publish its message loop."
        }
        if (
            -not $fakeTray.WaitForExit(7000) -or
            $tree.ActiveProcessCount -lt 2 -or
            $monitor.process.HasExited
        ) {
            throw "Coordinator-death monitor did not suppress a legacy replacement tray throughout process-tree drain."
        }

        Stop-BoundlessProcessBoundary -Process $tree -TimeoutMilliseconds 5000
        [void]$completion.event.Set()
        if (-not $monitor.process.WaitForExit(7000) -or $monitor.process.ExitCode -ne 0) {
            throw "Quiescence monitor did not close after coordinator-death tree completion."
        }
    }
    finally {
        if ($null -ne $fakeTray) {
            if (-not $fakeTray.HasExited) { Stop-BoundlessProcessBoundary -Process $fakeTray }
            $fakeTray.Dispose()
        }
        if ($null -ne $tree) {
            if ($tree.ActiveProcessCount -gt 0) { Stop-BoundlessProcessBoundary -Process $tree }
            $tree.Dispose()
        }
        if ($null -ne $coordinator) {
            if (-not $coordinator.HasExited) { Stop-BoundlessProcessBoundary -Process $coordinator }
            $coordinator.Dispose()
        }
        if ($null -ne $monitor) {
            if (-not $monitor.process.HasExited) {
                [void]$completion.event.Set()
                Stop-BoundlessProcessBoundary -Process $monitor.process
            }
            $monitor.handoff_event.Dispose()
            $monitor.heartbeat_event.Dispose()
            $monitor.ready_event.Dispose()
            $monitor.process.Dispose()
        }
        $completion.event.Dispose()
        $ready.Dispose()
        $fakeTrayReady.Dispose()
        if (Test-Path -LiteralPath $fixtureRoot) {
            Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

function Invoke-BoundlessBlockingServiceStopFixture {
    $fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
        "BoundlessBlockingStopFixture-$([guid]::NewGuid().ToString('N'))"
    )
    $childPidPath = Join-Path $fixtureRoot "child.pid"
    $msiInvoked = $false
    $recoveryState = [pscustomobject]@{
        service = "Running"
        status_calls = 0
        start_requests = 0
    }
    try {
        New-Item -ItemType Directory -Path $fixtureRoot -Force -ErrorAction Stop | Out-Null
        $pathPayload = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($childPidPath))
        $workerSource = @'
$pidPath = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__PATH__"))
$hostPath = (Get-Process -Id $PID -ErrorAction Stop).Path
$child = Start-Process -FilePath $hostPath -ArgumentList @(
    "-NoProfile", "-EncodedCommand", "__CHILD__"
) -WindowStyle Hidden -PassThru
[IO.File]::WriteAllText($pidPath, [string]$child.Id)
Start-Sleep -Seconds 30
'@.Replace("__PATH__", $pathPayload).Replace(
            "__CHILD__",
            [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('Start-Sleep -Seconds 30'))
        )
        $statusError = $null
        try {
            Get-BoundlessServiceStatusBounded `
                -TimeoutSeconds 1 `
                -FixtureSource $workerSource | Out-Null
        }
        catch {
            $statusError = $_
        }
        if ($null -eq $statusError -or $statusError.Exception.Message -notmatch 'status query exceeded') {
            throw "Blocking service-status fixture did not bound its SCM query worker."
        }
        if (Test-Path -LiteralPath $childPidPath -PathType Leaf) {
            $statusChildPid = [int](Get-Content -LiteralPath $childPidPath -Raw)
            if ($null -ne (Get-Process -Id $statusChildPid -ErrorAction SilentlyContinue)) {
                throw "Blocking service-status fixture left worker descendant PID $statusChildPid running."
            }
            Remove-Item -LiteralPath $childPidPath -Force
        }
        $stopError = $null
        try {
            Stop-BoundlessServiceForUpgrade `
                -TimeoutSeconds 1 `
                -StatusProbe { "Running" } `
                -WorkerFactory {
                    Start-BoundlessServiceControlWorker `
                        -Action "stop" `
                        -FixtureSource $workerSource
                } `
                -SkipAdministratorCheck | Out-Null
            $msiInvoked = $true
        }
        catch {
            $stopError = $_
        }
        if (
            $null -eq $stopError -or
            $stopError.Exception.Message -notmatch 'MSI was not started' -or
            $msiInvoked
        ) {
            throw "Blocking service-stop fixture did not fail closed before MSI invocation."
        }
        if (Test-Path -LiteralPath $childPidPath -PathType Leaf) {
            $childPid = [int](Get-Content -LiteralPath $childPidPath -Raw)
            if ($null -ne (Get-Process -Id $childPid -ErrorAction SilentlyContinue)) {
                throw "Blocking service-stop fixture left worker descendant PID $childPid running."
            }
        }

        $recoveryError = $null
        try {
            Stop-BoundlessServiceBeforeMsi `
                -TimeoutSeconds 2 `
                -StatusProbe {
                    $recoveryState.status_calls += 1
                    if ($recoveryState.status_calls -eq 1) { return "Running" }
                    if ($recoveryState.status_calls -eq 2) {
                        $recoveryState.service = "Stopped"
                        throw "fixture late service-status failure"
                    }
                    return $recoveryState.service
                } `
                -WorkerFactory {
                    $recoveryState.service = "Stopped"
                    Start-BoundlessServiceControlWorker -Action "stop" -FixtureSource "exit 0"
                } `
                -RecoveryWorkerFactory {
                    $recoveryState.start_requests += 1
                    $recoveryState.service = "Running"
                    Start-BoundlessServiceControlWorker -Action "start" -FixtureSource "exit 0"
                } `
                -SkipAdministratorCheck | Out-Null
        }
        catch {
            $recoveryError = $_
        }
        if (
            $null -eq $recoveryError -or
            $recoveryError.Exception.Message -notmatch 'fixture late service-status failure' -or
            [string]$recoveryError.Exception.Data["BoundlessServiceRecovery"] -notmatch
                'start_requested=True;final_status=Running' -or
            $recoveryState.start_requests -ne 1 -or
            $recoveryState.service -ne "Running"
        ) {
            throw "Pre-MSI partial-stop fixture did not restore the originally running service exactly once while preserving the stop error."
        }

        $startPendingOrigin = [Threading.EventWaitHandle]::new(
            $false,
            [Threading.EventResetMode]::ManualReset
        )
        try {
            $startPendingState = [pscustomobject]@{
                service = "StartPending"
                status_calls = 0
                start_requests = 0
            }
            $startPendingError = $null
            try {
                Stop-BoundlessServiceBeforeMsi `
                    -TimeoutSeconds 2 `
                    -StatusProbe {
                        $startPendingState.status_calls += 1
                        if ($startPendingState.status_calls -eq 1) { return "StartPending" }
                        if ($startPendingState.status_calls -eq 2) {
                            throw "fixture StartPending post-stop failure"
                        }
                        return $startPendingState.service
                    } `
                    -WorkerFactory {
                        $startPendingState.service = "Stopped"
                        Start-BoundlessServiceControlWorker -Action "stop" -FixtureSource "exit 0"
                    } `
                    -RecoveryWorkerFactory {
                        $startPendingState.start_requests += 1
                        $startPendingState.service = "Running"
                        Start-BoundlessServiceControlWorker -Action "start" -FixtureSource "exit 0"
                    } `
                    -InitialRunningEvent $startPendingOrigin `
                    -SkipAdministratorCheck | Out-Null
            }
            catch {
                $startPendingError = $_
            }
            if (
                $null -eq $startPendingError -or
                $startPendingError.Exception.Message -notmatch 'StartPending post-stop failure' -or
                -not $startPendingOrigin.WaitOne(0) -or
                [string]$startPendingError.Exception.Data["BoundlessServiceRecovery"] -notmatch
                    'start_requested=True;final_status=Running' -or
                $startPendingState.start_requests -ne 1 -or
                $startPendingState.service -ne "Running"
            ) {
                throw "Pre-MSI StartPending fixture did not publish restart eligibility and restore the service after stopping it."
            }
        }
        finally {
            $startPendingOrigin.Dispose()
        }

        $pendingState = [pscustomobject]@{
            service = "Running"
            status_calls = 0
            start_requests = 0
        }
        $pendingError = $null
        try {
            Stop-BoundlessServiceBeforeMsi `
                -TimeoutSeconds 2 `
                -StatusProbe {
                    $pendingState.status_calls += 1
                    switch ($pendingState.status_calls) {
                        1 { return "Running" }
                        2 { throw "fixture post-request polling failure" }
                        3 { return "StopPending" }
                        4 { return "StopPending" }
                        5 {
                            $pendingState.service = "Stopped"
                            return "Stopped"
                        }
                        default { return $pendingState.service }
                    }
                } `
                -WorkerFactory {
                    $pendingState.service = "StopPending"
                    Start-BoundlessServiceControlWorker -Action "stop" -FixtureSource "exit 0"
                } `
                -RecoveryWorkerFactory {
                    $pendingState.start_requests += 1
                    $pendingState.service = "Running"
                    Start-BoundlessServiceControlWorker -Action "start" -FixtureSource "exit 0"
                } `
                -SkipAdministratorCheck | Out-Null
        }
        catch {
            $pendingError = $_
        }
        if (
            $null -eq $pendingError -or
            $pendingError.Exception.Message -notmatch 'fixture post-request polling failure' -or
            $pendingState.start_requests -ne 1 -or
            $pendingState.service -ne "Running"
        ) {
            throw "Pre-MSI StopPending fixture did not settle and restart the originally running service exactly once."
        }

        foreach ($neverRunningStatus in @("Stopped", "Missing")) {
            $neverRunningStarts = [pscustomobject]@{ count = 0 }
            $result = Stop-BoundlessServiceBeforeMsi `
                -StatusProbe { $neverRunningStatus } `
                -WorkerFactory { throw "stop worker must not run" } `
                -RecoveryWorkerFactory {
                    $neverRunningStarts.count += 1
                    throw "recovery worker must not run"
                } `
                -SkipAdministratorCheck
            if ($neverRunningStarts.count -ne 0) {
                throw "Initially $neverRunningStatus service fixture attempted an unsolicited restart."
            }
            $expectedInitial = if ($neverRunningStatus -eq "Missing") {
                "NotInstalled"
            }
            else {
                "Stopped"
            }
            if ($result.initial_status -ne $expectedInitial) {
                throw "Initially $neverRunningStatus service fixture returned malformed shutdown evidence."
            }
        }

        foreach ($postFailureStatus in @("Running", "Missing")) {
            $exclusionState = [pscustomobject]@{
                status_calls = 0
                start_requests = 0
            }
            $exclusionError = $null
            try {
                Stop-BoundlessServiceBeforeMsi `
                    -TimeoutSeconds 2 `
                    -StatusProbe {
                        $exclusionState.status_calls += 1
                        if ($exclusionState.status_calls -eq 1) { return "Running" }
                        if ($exclusionState.status_calls -eq 2) {
                            throw "fixture excluded stop failure"
                        }
                        return $postFailureStatus
                    } `
                    -WorkerFactory {
                        Start-BoundlessServiceControlWorker -Action "stop" -FixtureSource "exit 0"
                    } `
                    -RecoveryWorkerFactory {
                        $exclusionState.start_requests += 1
                        Start-BoundlessServiceControlWorker -Action "start" -FixtureSource "exit 0"
                    } `
                    -SkipAdministratorCheck | Out-Null
            }
            catch {
                $exclusionError = $_
            }
            $expectedReason = if ($postFailureStatus -eq "Missing") {
                "service_missing_or_uninstall_policy"
            }
            else {
                "stop_not_observed"
            }
            if (
                $null -eq $exclusionError -or
                $exclusionError.Exception.Message -notmatch 'fixture excluded stop failure' -or
                [string]$exclusionError.Exception.Data["BoundlessServiceRecovery"] -notmatch
                    [regex]::Escape("reason=$expectedReason") -or
                $exclusionState.start_requests -ne 0
            ) {
                throw "Pre-MSI recovery exclusion fixture for $postFailureStatus attempted an unsafe service restart or lost diagnostics."
            }
        }
    }
    finally {
        if (Test-Path -LiteralPath $fixtureRoot) {
            Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

function Invoke-BoundlessFailedDrainQuiescenceFixture {
    param([string]$UserSid)

    $fixtureId = [guid]::NewGuid().ToString('N')
    $sessionId = 2147482002
    $owner = New-BoundlessNamedMutex `
        -Name "Local\Boundless.Test.FailedDrain.Owner.$fixtureId" `
        -UserSid $UserSid `
        -InitiallyOwned $true
    $sentinelOwner = Start-BoundlessTrayQuiescenceSentinelOwner `
        -UserSid $UserSid `
        -SessionId $sessionId
    $sentinelName = $sentinelOwner.sentinel_name
    $completion = New-BoundlessInstallerCompletionEvent -UserSid $UserSid
    $treeJobName = "Local\Boundless.Installer.Tree.v1.$fixtureId"
    $monitor = $null
    $tree = $null
    $transferred = $false
    $completionDisposed = $false
    try {
        if (-not $owner.created_new) {
            throw "Failed-drain quiescence fixture collided with an existing mutex."
        }
        $monitor = Start-BoundlessTrayQuiescenceMonitor `
            -ExpectedOwnerSid $UserSid `
            -ExpectedSessionId $sessionId `
            -SentinelName $sentinelName `
            -TreeJobName $treeJobName `
            -CompletionEventName $completion.name
        Wait-BoundlessTrayQuiescenceMonitorReady -Monitor $monitor -TimeoutSeconds 10
        $monitorPid = $monitor.process.Id
        $tree = Start-BoundlessOwnedProcessBoundary `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String(
                    [Text.Encoding]::Unicode.GetBytes('Start-Sleep -Seconds 30')
                )
            ) `
            -CreateNoWindow `
            -JobName $treeJobName `
            -JobSddl (Get-BoundlessOwnedTreeSddl -UserSid $UserSid)
        $lease = [pscustomobject]@{
            mutex = $owner.mutex
            sentinel_mutex = $null
            sentinel_owner = $sentinelOwner
            monitor = $monitor
            completion_event = $completion.event
            completion_event_name = $completion.name
            tree_job_name = $treeJobName
            elevated_process = $null
            expected_owner_sid = $UserSid
            expected_session_id = $sessionId
            msi_may_have_started_event_name = ""
            msi_definitive_completion_event_name = ""
            msi_idle_proven_event_name = ""
            installer_transaction_mutex_name = "Global\_MSIExecute"
            evidence = [pscustomobject]@{
                sentinel_name = $sentinelName
                installer_tree_closed = $false
                quiescence_abandoned_to_monitor = $false
                quiescence_guardian_process_id = $null
            }
        }
        Resolve-BoundlessUnconfirmedTreeAndQuiescence -Lease $lease
        $transferred = $true
        $completionDisposed = $true
        $monitorPid = [int]$lease.evidence.quiescence_guardian_process_id
        if (
            -not $lease.evidence.quiescence_abandoned_to_monitor -or
            $tree.ActiveProcessCount -lt 1 -or
            $null -eq (Get-Process -Id $monitorPid -ErrorAction SilentlyContinue)
        ) {
            throw "Failed-drain quiescence fixture released supervision before its active tree drained."
        }
        Stop-BoundlessProcessBoundary -Process $tree -TimeoutMilliseconds 5000
        $tree.Dispose()
        $tree = $null
        $deadline = (Get-Date).AddSeconds(7)
        while (
            $null -ne (Get-Process -Id $monitorPid -ErrorAction SilentlyContinue) -and
            (Get-Date) -lt $deadline
        ) {
            Start-Sleep -Milliseconds 50
        }
        if ($null -ne (Get-Process -Id $monitorPid -ErrorAction SilentlyContinue)) {
            throw "Failed-drain quiescence monitor did not exit after the installer tree disappeared."
        }
    }
    finally {
        if ($null -ne $tree) {
            if ($tree.ActiveProcessCount -gt 0) { Stop-BoundlessProcessBoundary -Process $tree }
            $tree.Dispose()
        }
        if (-not $transferred) {
            if ($owner.created_new) {
                try { $owner.mutex.ReleaseMutex() } catch { }
                $owner.mutex.Dispose()
            }
            Stop-BoundlessTrayQuiescenceSentinelOwner -Owner $sentinelOwner
            if ($null -ne $monitor) {
                if (-not $monitor.process.HasExited) { [void]$completion.event.Set() }
                $monitor.handoff_event.Dispose()
                $monitor.heartbeat_event.Dispose()
                $monitor.ready_event.Dispose()
                if (-not $monitor.process.HasExited) { Stop-BoundlessProcessBoundary -Process $monitor.process }
                $monitor.process.Dispose()
            }
        }
        if (-not $completionDisposed) { $completion.event.Dispose() }
    }
}

function Invoke-BoundlessHardCancelBeforeMsiRecoveryFixture {
    param(
        [string]$Source,
        [string]$UserSid
    )

    $tokens = $null
    $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseInput(
        $Source,
        [ref]$tokens,
        [ref]$errors
    )
    if ($errors.Count -ne 0) {
        throw "Hard-cancel recovery fixture could not parse the elevated bootstrap source."
    }
    foreach ($functionName in @("Wait-JobEmpty", "Restore-BootstrapServiceBeforeMsiFailure")) {
        $definition = $ast.FindAll(
            {
                param($node)
                $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
                    $node.Name -eq $functionName
            },
            $true
        ) | Select-Object -First 1
        if ($null -eq $definition) {
            throw "Hard-cancel recovery fixture could not find bootstrap function $functionName."
        }
        Invoke-Expression $definition.Extent.Text
    }
    function Quote-Argument {
        param([string]$Value)
        return ConvertTo-ProcessArgument -Value $Value
    }

    $initialEventName = "Local\Boundless.Test.ServiceStoppedBeforeMsi.$([guid]::NewGuid().ToString('N'))"
    $initialCreated = $false
    $initialRunning = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $initialEventName,
        [ref]$initialCreated
    )
    $mayHaveStarted = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset
    )
    $definitive = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset
    )
    $recoveryEventName = "Local\Boundless.Test.BootstrapRecoveryRan.$([guid]::NewGuid().ToString('N'))"
    $recoveryCreated = $false
    $recoveryRan = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $recoveryEventName,
        [ref]$recoveryCreated
    )
    $canceledJob = $null
    $canceledRoot = $null
    try {
        if (-not $initialCreated -or -not $recoveryCreated) {
            throw "Hard-cancel recovery fixture collided with its phase event."
        }
        $eventPayload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes($initialEventName)
        )
        $canceledSource = @'
$name = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__EVENT__"))
$event = [Threading.EventWaitHandle]::OpenExisting($name)
try {
    [void]$event.Set()
    Start-Sleep -Seconds 30
}
finally { $event.Dispose() }
'@.Replace("__EVENT__", $eventPayload)
        $canceledJob = [BoundlessElevatedJob]::Create(
            "Local\Boundless.Installer.Tree.v1.$([guid]::NewGuid().ToString('N'))",
            (Get-BoundlessOwnedTreeSddl -UserSid $UserSid)
        )
        $hostPath = Resolve-CurrentPowerShellExecutable
        $rootArgs = @(
            "-NoProfile",
            "-EncodedCommand",
            [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($canceledSource))
        )
        $rootLine = @(
            ConvertTo-ProcessArgument -Value $hostPath
            @($rootArgs | ForEach-Object { ConvertTo-ProcessArgument -Value $_ })
        ) -join " "
        $canceledRoot = $canceledJob.StartOwned($hostPath, $rootLine)
        if (-not $initialRunning.WaitOne(10000)) {
            throw "Hard-cancel recovery fixture did not publish its originally-running service evidence."
        }
        $canceledJob.Terminate()
        if (-not (Wait-JobEmpty -Job $canceledJob -TimeoutMilliseconds 5000)) {
            throw "Hard-cancel recovery fixture did not drain the canceled pre-MSI helper."
        }
        if (-not $canceledRoot.WaitForExit(5000)) {
            throw "Hard-cancel recovery fixture root did not terminate with its job."
        }

        $recoveryPayload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes($recoveryEventName)
        )
        $recoverySource = @'
$name = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__EVENT__"))
$event = [Threading.EventWaitHandle]::OpenExisting($name)
try { [void]$event.Set() } finally { $event.Dispose() }
'@.Replace("__EVENT__", $recoveryPayload)
        $recovery = Restore-BootstrapServiceBeforeMsiFailure `
            -TreeJobSddl (Get-BoundlessOwnedTreeSddl -UserSid $UserSid) `
            -TimeoutMilliseconds 5000 `
            -FixtureWorkerSource $recoverySource
        if (
            -not $recoveryRan.WaitOne(0) -or
            $recovery -match 'status=(timeout|tree_not_drained|error)'
        ) {
            throw "Hard-cancel recovery fixture did not restore after proven pre-MSI cancellation: $recovery"
        }

        [void]$mayHaveStarted.Set()
        [void]$definitive.Set()
        [void]$mayHaveStarted.Reset()
        [void]$recoveryRan.Reset()
        if (-not $definitive.WaitOne(0) -or $mayHaveStarted.WaitOne(0)) {
            throw "Hard-cancel recovery fixture lost returned non-success evidence."
        }
        $retry = Restore-BootstrapServiceBeforeMsiFailure `
            -TreeJobSddl (Get-BoundlessOwnedTreeSddl -UserSid $UserSid) `
            -TimeoutMilliseconds 5000 `
            -FixtureWorkerSource $recoverySource
        if (-not $recoveryRan.WaitOne(0) -or $retry -match 'status=(timeout|tree_not_drained|error)') {
            throw "Hard-cancel recovery fixture did not restore after a definitive non-success: $retry"
        }
        [void]$mayHaveStarted.Set()
        if (-not $mayHaveStarted.WaitOne(0)) {
            throw "Hard-cancel recovery fixture lost the successful/uncertain MSI exclusion."
        }
    }
    finally {
        if ($null -ne $canceledJob) {
            if ($canceledJob.Active -gt 0) { $canceledJob.Terminate() }
            $canceledJob.Dispose()
        }
        if ($null -ne $canceledRoot) {
            if (-not $canceledRoot.HasExited) { Stop-BoundlessProcessBoundary -Process $canceledRoot }
            $canceledRoot.Dispose()
        }
        $mayHaveStarted.Dispose()
        $definitive.Dispose()
        $recoveryRan.Dispose()
        $initialRunning.Dispose()
    }
}

function Invoke-BoundlessUncertainTransactionGuardianFixture {
    param([string]$UserSid)

    $fixtureId = [guid]::NewGuid().ToString('N')
    $sessionId = 2147482003
    $transactionMutexName = "Global\Boundless.Test.MsiExecute.$fixtureId"
    $holderReadyName = "Local\Boundless.Test.MsiExecuteReady.$fixtureId"
    $holderReleaseName = "Local\Boundless.Test.MsiExecuteRelease.$fixtureId"
    $holderReadyCreated = $false
    $holderReleaseCreated = $false
    $holderReady = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $holderReadyName,
        [ref]$holderReadyCreated
    )
    $holderRelease = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $holderReleaseName,
        [ref]$holderReleaseCreated
    )
    $owner = New-BoundlessNamedMutex `
        -Name "Local\Boundless.Test.UncertainTransaction.Owner.$fixtureId" `
        -UserSid $UserSid `
        -InitiallyOwned $true
    $sentinelOwner = Start-BoundlessTrayQuiescenceSentinelOwner `
        -UserSid $UserSid `
        -SessionId $sessionId
    $completion = New-BoundlessInstallerCompletionEvent -UserSid $UserSid
    $serviceInitial = New-BoundlessInstallerPhaseEvent `
        -UserSid $UserSid `
        -Phase "ServiceInitialRunning"
    $mayHaveStarted = New-BoundlessInstallerPhaseEvent `
        -UserSid $UserSid `
        -Phase "MsiMayHaveStarted"
    $definitive = New-BoundlessInstallerPhaseEvent `
        -UserSid $UserSid `
        -Phase "MsiDefinitiveCompletion"
    $idleProven = New-BoundlessInstallerPhaseEvent `
        -UserSid $UserSid `
        -Phase "MsiIdleProven"
    $monitor = $null
    $holder = $null
    $transferred = $false
    try {
        if (-not $holderReadyCreated -or -not $holderReleaseCreated -or -not $owner.created_new) {
            throw "Uncertain-transaction guardian fixture collided with a kernel object."
        }
        [void]$serviceInitial.event.Set()
        [void]$mayHaveStarted.event.Set()
        [void]$completion.event.Set()
        if (Test-BoundlessNormalQuiescenceReleaseAllowed `
            -InstallerTreeClosed $true `
            -CompletionState "uncertain" `
            -MsiTransactionIdleProven $false) {
            throw "Uncertain-transaction fixture treated client-tree drain as normal completion."
        }

        $holderPayload = [Convert]::ToBase64String(
            [Text.Encoding]::UTF8.GetBytes(
                "$transactionMutexName`n$holderReadyName`n$holderReleaseName"
            )
        )
        $holderSource = @'
$names = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD__")
) -split "`n"
$ready = [Threading.EventWaitHandle]::OpenExisting($names[1])
$release = [Threading.EventWaitHandle]::OpenExisting($names[2])
$created = $false
$mutex = [Threading.Mutex]::new($true, $names[0], [ref]$created)
try {
    if (-not $created) { exit 61 }
    [void]$ready.Set()
    if (-not $release.WaitOne(30000)) { exit 62 }
    $mutex.ReleaseMutex()
}
finally {
    $mutex.Dispose()
    $release.Dispose()
    $ready.Dispose()
}
'@.Replace("__PAYLOAD__", $holderPayload)
        $holder = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($holderSource))
            ) `
            -WindowStyle Hidden `
            -PassThru
        if (-not $holderReady.WaitOne(10000)) {
            throw "Uncertain-transaction fixture did not acquire its transaction mutex."
        }

        $treeJobName = "Local\Boundless.Installer.Tree.v1.$fixtureId"
        $stalledProcess = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-EncodedCommand",
                [Convert]::ToBase64String(
                    [Text.Encoding]::Unicode.GetBytes('Start-Sleep -Seconds 30')
                )
            ) `
            -WindowStyle Hidden `
            -PassThru
        $monitor = [pscustomobject]@{
            process = $stalledProcess
            ready_event = [Threading.EventWaitHandle]::new(
                $false,
                [Threading.EventResetMode]::ManualReset
            )
            handoff_event = [Threading.EventWaitHandle]::new(
                $false,
                [Threading.EventResetMode]::ManualReset
            )
            heartbeat_event = [Threading.EventWaitHandle]::new(
                $false,
                [Threading.EventResetMode]::ManualReset
            )
            stable_milliseconds = 500
        }
        $stalledMonitorPid = $monitor.process.Id
        $lease = [pscustomobject]@{
            mutex = $owner.mutex
            sentinel_mutex = $null
            sentinel_owner = $sentinelOwner
            monitor = $monitor
            completion_event = $completion.event
            completion_event_name = $completion.name
            tree_job_name = $treeJobName
            elevated_process = $null
            expected_owner_sid = $UserSid
            expected_session_id = $sessionId
            service_initial_running_event = $serviceInitial.event
            service_initial_running_event_name = $serviceInitial.name
            msi_may_have_started_event = $mayHaveStarted.event
            msi_may_have_started_event_name = $mayHaveStarted.name
            msi_definitive_completion_event = $definitive.event
            msi_definitive_completion_event_name = $definitive.name
            msi_idle_proven_event = $idleProven.event
            msi_idle_proven_event_name = $idleProven.name
            installer_transaction_mutex_name = $transactionMutexName
            evidence = [pscustomobject]@{
                sentinel_name = $sentinelOwner.sentinel_name
                installer_tree_closed = $true
                installer_completion_state = "uncertain"
                msi_transaction_idle_proven = $false
                quiescence_abandoned_to_monitor = $false
                quiescence_guardian_process_id = $null
            }
        }
        Resolve-BoundlessUnconfirmedTreeAndQuiescence -Lease $lease
        $transferred = $true
        $monitorPid = [int]$lease.evidence.quiescence_guardian_process_id
        Start-Sleep -Milliseconds 500
        if (
            -not $lease.evidence.quiescence_abandoned_to_monitor -or
            $monitorPid -le 0 -or
            $null -ne (Get-Process -Id $stalledMonitorPid -ErrorAction SilentlyContinue) -or
            $null -eq (Get-Process -Id $monitorPid -ErrorAction SilentlyContinue)
        ) {
            throw "Uncertain-transaction takeover did not replace the stalled monitor with an acknowledged guardian."
        }
        $sentinelProbe = [Threading.Mutex]::OpenExisting($sentinelOwner.sentinel_name)
        try {
            $probeAcquired = $false
            try {
                $probeAcquired = $sentinelProbe.WaitOne(0)
            }
            catch [Threading.AbandonedMutexException] {
                throw "Uncertain-transaction takeover left the tray sentinel abandoned."
            }
            if ($probeAcquired) {
                $sentinelProbe.ReleaseMutex()
                throw "Uncertain-transaction takeover returned before the independent guardian owned the tray sentinel."
            }
        }
        finally {
            $sentinelProbe.Dispose()
        }
        [void]$holderRelease.Set()
        if (-not $holder.WaitForExit(5000) -or $holder.ExitCode -ne 0) {
            throw "Uncertain-transaction fixture transaction holder did not release normally."
        }
        $deadline = (Get-Date).AddSeconds(7)
        while (
            $null -ne (Get-Process -Id $monitorPid -ErrorAction SilentlyContinue) -and
            (Get-Date) -lt $deadline
        ) {
            Start-Sleep -Milliseconds 50
        }
        if ($null -ne (Get-Process -Id $monitorPid -ErrorAction SilentlyContinue)) {
            throw "Uncertain-transaction guardian did not release after authoritative transaction-idle proof."
        }
        if (-not (Test-BoundlessNormalQuiescenceReleaseAllowed `
            -InstallerTreeClosed $true `
            -CompletionState "uncertain" `
            -MsiTransactionIdleProven $true)) {
            throw "Uncertain-transaction fixture rejected authoritative transaction-idle proof."
        }
    }
    finally {
        if ($null -ne $holder) {
            if (-not $holder.HasExited) {
                [void]$holderRelease.Set()
                if (-not $holder.WaitForExit(1000)) {
                    Stop-BoundlessProcessBoundary -Process $holder
                }
            }
            $holder.Dispose()
        }
        if (-not $transferred) {
            if ($owner.created_new) {
                try { $owner.mutex.ReleaseMutex() } catch { }
                $owner.mutex.Dispose()
            }
            Stop-BoundlessTrayQuiescenceSentinelOwner -Owner $sentinelOwner
            if ($null -ne $monitor) {
                if (-not $monitor.process.HasExited) { Stop-BoundlessProcessBoundary -Process $monitor.process }
                $monitor.handoff_event.Dispose()
                $monitor.heartbeat_event.Dispose()
                $monitor.ready_event.Dispose()
                $monitor.process.Dispose()
            }
            $completion.event.Dispose()
            foreach ($phase in @($serviceInitial, $mayHaveStarted, $definitive, $idleProven)) {
                $phase.event.Dispose()
            }
        }
        $holderRelease.Dispose()
        $holderReady.Dispose()
    }
}

function Invoke-BoundlessFailedMsiServiceRecoveryFixture {
    $state = [pscustomobject]@{ service = "Stopped"; start_requests = 0 }
    $serviceShutdown = [pscustomobject]@{ initial_status = "Running" }
    $restartAction = {
        Start-BoundlessServiceAfterFailedInstall `
            -TimeoutSeconds 2 `
            -StatusProbe { $state.service } `
            -WorkerFactory {
                $state.start_requests += 1
                $state.service = "Running"
                Start-BoundlessServiceControlWorker -Action "start" -FixtureSource "exit 0"
            } `
            -SkipAdministratorCheck
    }
    $definitiveError = $null
    try {
        Invoke-BoundlessMsiWithServiceRecovery `
            -ServiceShutdown $serviceShutdown `
            -MsiAction {
                Throw-BoundlessMsiFailure `
                    -Message "fixture definitive MSI failure" `
                    -CompletionState "definitive_failure"
            } `
            -RestartAction $restartAction | Out-Null
    }
    catch {
        $definitiveError = $_
    }
    if (
        $null -eq $definitiveError -or
        $definitiveError.Exception.Message -notmatch 'fixture definitive MSI failure' -or
        $state.start_requests -ne 1 -or
        $state.service -ne "Running"
    ) {
        throw "Definitive MSI failure fixture did not restart the originally running service exactly once while preserving the error."
    }

    $state.service = "Stopped"
    $notStartedError = $null
    try {
        Invoke-BoundlessMsiWithServiceRecovery `
            -ServiceShutdown $serviceShutdown `
            -MsiAction {
                Throw-BoundlessMsiFailure `
                    -Message "fixture MSI did not start" `
                    -CompletionState "not_started"
            } `
            -RestartAction $restartAction | Out-Null
    }
    catch {
        $notStartedError = $_
    }
    if (
        $null -eq $notStartedError -or
        $notStartedError.Exception.Message -notmatch 'fixture MSI did not start' -or
        $state.start_requests -ne 2 -or
        $state.service -ne "Running"
    ) {
        throw "Prelaunch MSI failure fixture did not restore the originally running service while preserving the error."
    }

    $state.service = "Stopped"
    try {
        Invoke-BoundlessMsiWithServiceRecovery `
            -ServiceShutdown $serviceShutdown `
            -MsiAction {
                Throw-BoundlessMsiFailure `
                    -Message "fixture uncertain MSI failure" `
                    -CompletionState "uncertain"
            } `
            -RestartAction $restartAction | Out-Null
    }
    catch { }
    if ($state.start_requests -ne 2 -or $state.service -ne "Stopped") {
        throw "Uncertain MSI failure fixture attempted an unsafe service restart."
    }

    $initiallyStopped = [pscustomobject]@{ initial_status = "Stopped" }
    try {
        Invoke-BoundlessMsiWithServiceRecovery `
            -ServiceShutdown $initiallyStopped `
            -MsiAction {
                Throw-BoundlessMsiFailure `
                    -Message "fixture stopped-service MSI failure" `
                    -CompletionState "definitive_failure"
            } `
            -RestartAction $restartAction | Out-Null
    }
    catch { }
    if ($state.start_requests -ne 2) {
        throw "Initially stopped service fixture attempted an unsolicited restart."
    }

    $initiallyMissing = [pscustomobject]@{ initial_status = "NotInstalled" }
    try {
        Invoke-BoundlessMsiWithServiceRecovery `
            -ServiceShutdown $initiallyMissing `
            -MsiAction {
                Throw-BoundlessMsiFailure `
                    -Message "fixture missing-service MSI failure" `
                    -CompletionState "definitive_failure"
            } `
            -RestartAction $restartAction | Out-Null
    }
    catch { }
    if ($state.start_requests -ne 2) {
        throw "Initially missing service fixture attempted an unsolicited restart."
    }
}

function Invoke-BoundlessRecoveryActionFenceFixture {
    param([string]$UserSid)

    $revokerSource = @'
$names = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD__")
) -split "`n"
$mode = $names[0]
$revoked = [Threading.EventWaitHandle]::OpenExisting($names[1])
$fence = [Threading.Mutex]::OpenExisting($names[2])
$committed = [Threading.EventWaitHandle]::OpenExisting($names[3])
$trigger = [Threading.EventWaitHandle]::OpenExisting($names[4])
$release = [Threading.EventWaitHandle]::OpenExisting($names[5])
$returned = [Threading.EventWaitHandle]::OpenExisting($names[6])
$owned = $false
try {
    if ($mode -eq "parent_wins") {
        [void]$revoked.Set()
        try { $owned = $fence.WaitOne(5000) }
        catch [Threading.AbandonedMutexException] { $owned = $true }
        if (-not $owned) { exit 81 }
        [void]$trigger.Set()
        if (-not $release.WaitOne(5000)) { exit 82 }
    }
    else {
        if (-not $trigger.WaitOne(5000)) { exit 83 }
        [void]$revoked.Set()
        try { $owned = $fence.WaitOne(5000) }
        catch [Threading.AbandonedMutexException] { exit 84 }
        if (-not $owned -or -not $committed.WaitOne(0)) { exit 85 }
    }
}
finally {
    if ($owned) { try { $fence.ReleaseMutex() } catch { } }
    [void]$returned.Set()
    $returned.Dispose()
    $release.Dispose()
    $trigger.Dispose()
    $committed.Dispose()
    $fence.Dispose()
    $revoked.Dispose()
}
'@

    foreach ($mode in @("parent_wins", "child_committed")) {
        $authority = $null
        $client = $null
        $revoker = $null
        $trigger = $null
        $release = $null
        $returned = $null
        $startMarker = $null
        try {
            $authority = New-BoundlessRecoveryAuthority -UserSid $UserSid
            $client = [pscustomobject]@{
                revocation_event = [Threading.EventWaitHandle]::OpenExisting(
                    $authority.revocation_event_name
                )
                action_fence = [Threading.Mutex]::OpenExisting(
                    $authority.action_fence_name
                )
                action_committed_event = [Threading.EventWaitHandle]::OpenExisting(
                    $authority.action_committed_event_name
                )
            }
            $trigger = New-BoundlessSentinelOwnerEvent `
                -Prefix "Boundless.Test.RecoveryFenceTrigger.v1" `
                -UserSid $UserSid
            $release = New-BoundlessSentinelOwnerEvent `
                -Prefix "Boundless.Test.RecoveryFenceRelease.v1" `
                -UserSid $UserSid
            $returned = New-BoundlessSentinelOwnerEvent `
                -Prefix "Boundless.Test.RecoveryFenceReturned.v1" `
                -UserSid $UserSid
            $startMarker = New-BoundlessSentinelOwnerEvent `
                -Prefix "Boundless.Test.RecoveryFenceStart.v1" `
                -UserSid $UserSid
            $payload = [Convert]::ToBase64String(
                [Text.Encoding]::UTF8.GetBytes(
                    "$mode`n$($authority.revocation_event_name)`n" +
                    "$($authority.action_fence_name)`n" +
                    "$($authority.action_committed_event_name)`n" +
                    "$($trigger.name)`n$($release.name)`n$($returned.name)"
                )
            )
            $source = $revokerSource.Replace("__PAYLOAD__", $payload)
            $revoker = Start-Process `
                -FilePath (Resolve-CurrentPowerShellExecutable) `
                -ArgumentList @(
                    "-NoProfile",
                    "-EncodedCommand",
                    [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($source))
                ) `
                -WindowStyle Hidden `
                -PassThru

            $state = [pscustomobject]@{ service = "Stopped"; starts = 0 }
            if ($mode -eq "parent_wins") {
                if (-not $trigger.event.WaitOne(5000)) {
                    throw "Recovery action-fence parent-wins fixture did not acquire the fence."
                }
                [void]$release.event.Set()
                if (-not $returned.event.WaitOne(5000)) {
                    throw "Recovery action-fence parent-wins fixture did not return."
                }
                if (-not $revoker.WaitForExit(5000) -or $revoker.ExitCode -ne 0) {
                    throw "Recovery action-fence parent-wins revoker failed."
                }
                $startError = $null
                try {
                    Start-BoundlessServiceAfterFailedInstall `
                        -TimeoutSeconds 2 `
                        -StatusProbe { $state.service } `
                        -WorkerFactory {
                            $state.starts += 1
                            [void]$startMarker.event.Set()
                            $state.service = "Running"
                        } `
                        -RecoveryAuthority $client `
                        -SkipAdministratorCheck | Out-Null
                }
                catch {
                    $startError = $_
                }
                Start-Sleep -Milliseconds 250
                if (
                    $null -eq $startError -or
                    $startError.Exception.Message -notmatch 'revoked before the start request' -or
                    $state.starts -ne 0 -or
                    $startMarker.event.WaitOne(0)
                ) {
                    throw "Recovery action-fence parent-wins ordering crossed the SCM mutation boundary."
                }
            }
            else {
                $result = Start-BoundlessServiceAfterFailedInstall `
                    -TimeoutSeconds 2 `
                    -StatusProbe { $state.service } `
                    -BeforeServiceStartAction {
                        [void]$trigger.event.Set()
                        if (-not $client.revocation_event.WaitOne(5000)) {
                            throw "Recovery action-fence committed fixture did not observe parent revocation."
                        }
                    } `
                    -WorkerFactory {
                        $state.starts += 1
                        [void]$startMarker.event.Set()
                        $state.service = "Running"
                    } `
                    -RecoveryAuthority $client `
                    -SkipAdministratorCheck
                if (-not $returned.event.WaitOne(5000)) {
                    throw "Recovery action-fence committed parent did not synchronize after settlement."
                }
                if (-not $revoker.WaitForExit(5000) -or $revoker.ExitCode -ne 0) {
                    throw "Recovery action-fence committed revoker failed."
                }
                if (
                    $result.final_status -ne "Running" -or
                    $state.starts -ne 1 -or
                    -not $startMarker.event.WaitOne(0) -or
                    -not $authority.action_committed_event.WaitOne(0)
                ) {
                    throw "Recovery action-fence committed ordering did not settle before parent return."
                }
            }

            $synchronization = Revoke-BoundlessRecoveryAuthorityAndSynchronizeAction `
                -Authority $authority `
                -SettlementTimeoutMilliseconds 2000
            Close-BoundlessRecoveryAuthority `
                -Authority $authority `
                -Revoke `
                -ActionFenceOwned:([bool]$synchronization.fence_owned)
            $authority = $null
        }
        finally {
            if ($null -ne $revoker) {
                if (-not $revoker.HasExited) { $revoker.Kill() }
                $revoker.Dispose()
            }
            if ($null -ne $authority) {
                try {
                    $cleanupSynchronization = Revoke-BoundlessRecoveryAuthorityAndSynchronizeAction `
                        -Authority $authority `
                        -SettlementTimeoutMilliseconds 2000
                    Close-BoundlessRecoveryAuthority `
                        -Authority $authority `
                        -Revoke `
                        -ActionFenceOwned:([bool]$cleanupSynchronization.fence_owned)
                }
                catch { }
            }
            if ($null -ne $client) {
                Close-BoundlessRecoveryAuthorityClient -Authority $client
            }
            foreach ($eventOwner in @($startMarker, $returned, $release, $trigger)) {
                if ($null -ne $eventOwner) { $eventOwner.event.Dispose() }
            }
        }
    }
}

function Invoke-BoundlessRecoveryAuthorityDrainFailureFixture {
    param([string]$UserSid)

    $authority = New-BoundlessRecoveryAuthority -UserSid $UserSid
    $closeError = $null
    try {
        $synchronization = Revoke-BoundlessRecoveryAuthorityAndSynchronizeAction `
            -Authority $authority `
            -SettlementTimeoutMilliseconds 2000
        try {
            Close-BoundlessRecoveryAuthority `
                -Authority $authority `
                -Revoke `
                -ActionFenceOwned:([bool]$synchronization.fence_owned) `
                -DrainTimeoutMilliseconds 1 `
                -DrainProof { $false }
        }
        catch {
            $closeError = $_
        }
        if (
            $null -eq $closeError -or
            $closeError.Exception.Message -notmatch 'did not drain' -or
            $authority.drained
        ) {
            throw "Recovery authority drain-failure fixture did not retain uncertain evidence."
        }
    }
    finally {
        # Close-BoundlessRecoveryAuthority disposes all authority handles even
        # when its injected drain proof fails.
    }

    $failClosedState = [pscustomobject]@{ invoked = $false; reason = "" }
    $lease = [pscustomobject]@{
        mutex = [pscustomobject]@{}
        evidence = [pscustomobject]@{
            installer_tree_closed = $true
            installer_completion_state = "not_started"
            msi_transaction_idle_proven = $false
            recovery_authority_drained = $false
            recovery_action_settled = $true
            recovery_authority_job_name = $authority.job_name
        }
    }
    if (Test-BoundlessNormalQuiescenceReleaseAllowed `
        -InstallerTreeClosed $true `
        -CompletionState "not_started" `
        -MsiTransactionIdleProven $false `
        -RecoveryAuthorityDrained $false `
        -RecoveryActionSettled $true) {
        throw "Recovery authority drain-failure fixture allowed normal quiescence release."
    }
    Resolve-BoundlessUnconfirmedTreeAndQuiescence `
        -Lease $lease `
        -RecoveryAuthorityActiveProcessProbe { 1 } `
        -RecoveryAuthorityDrainTimeoutMilliseconds 100 `
        -FailClosedAction {
            param($fixtureLease, $reason)
            $failClosedState.invoked = $true
            $failClosedState.reason = $reason
        }
    if (
        -not $failClosedState.invoked -or
        $failClosedState.reason -notmatch 'authority drain remained unproven'
    ) {
        throw "Recovery authority drain-failure fixture did not enter the fail-closed resolver."
    }
}

function Invoke-BoundlessReplacementTrayWindowFixture {
    param([string]$UserSid)

    $sessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
    $sentinelName = Get-BoundlessTrayQuiescenceSentinelName `
        -UserSid $UserSid `
        -SessionId $sessionId
    $sentinel = New-BoundlessNamedMutex `
        -Name $sentinelName `
        -UserSid $UserSid `
        -InitiallyOwned $true
    if (-not $sentinel.created_new) {
        $sentinel.mutex.Dispose()
        throw "Replacement tray fixture collided with an existing sentinel."
    }
    $completion = New-BoundlessInstallerCompletionEvent -UserSid $UserSid
    $treeJobName = "Local\Boundless.Installer.Tree.v1.$([guid]::NewGuid().ToString('N'))"
    $readyName = "Local\Boundless.Test.ReplacementTrayReady.$([guid]::NewGuid().ToString('N'))"
    $readyCreated = $false
    $ready = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $readyName,
        [ref]$readyCreated
    )
    $fakeTray = $null
    $monitor = $null
    $sentinelReleased = $false
    $monitorCompleted = $false
    try {
        if (-not $readyCreated) {
            throw "Replacement tray fixture could not create its ready event."
        }
        $readyPayload = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($readyName))
        $fakeTraySource = @'
Add-Type -AssemblyName System.Windows.Forms
$name = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String("__READY__"))
$ready = [Threading.EventWaitHandle]::OpenExisting($name)
try {
    [void]$ready.Set()
    [Windows.Forms.Application]::Run()
}
finally { $ready.Dispose() }
'@.Replace("__READY__", $readyPayload)
        $fakeTray = Start-Process `
            -FilePath (Resolve-CurrentPowerShellExecutable) `
            -ArgumentList @(
                "-NoProfile",
                "-STA",
                "-EncodedCommand",
                [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($fakeTraySource))
            ) `
            -WindowStyle Hidden `
            -PassThru
        if (-not $ready.WaitOne(10000)) {
            throw "Replacement tray fixture did not publish its message loop."
        }
        $monitor = Start-BoundlessTrayQuiescenceMonitor `
            -ExpectedOwnerSid $UserSid `
            -ExpectedSessionId $sessionId `
            -SentinelName $sentinelName `
            -TreeJobName $treeJobName `
            -CompletionEventName $completion.name `
            -FixtureProcessId $fakeTray.Id
        Wait-BoundlessTrayQuiescenceMonitorReady -Monitor $monitor -TimeoutSeconds 10
        if (-not $fakeTray.WaitForExit(5000) -or $monitor.process.HasExited) {
            throw "Replacement tray fixture did not close the replacement window and retain supervision."
        }
        $monitorExitedEarly = $monitor.process.HasExited
        try {
            $sentinel.mutex.ReleaseMutex()
        }
        finally {
            $sentinel.mutex.Dispose()
            $sentinelReleased = $true
        }
        Complete-BoundlessTrayQuiescenceMonitor `
            -Monitor $monitor `
            -ExitedBeforeSentinelRelease $monitorExitedEarly | Out-Null
        $monitorCompleted = $true
    }
    finally {
        $ready.Dispose()
        if ($null -ne $fakeTray) {
            if (-not $fakeTray.HasExited) {
                Stop-BoundlessProcessBoundary -Process $fakeTray
            }
            $fakeTray.Dispose()
        }
        if (-not $sentinelReleased) {
            try { $sentinel.mutex.ReleaseMutex() } finally { $sentinel.mutex.Dispose() }
        }
        if ($null -ne $monitor -and -not $monitorCompleted) {
            $monitorExitedEarly = $monitor.process.HasExited
            Complete-BoundlessTrayQuiescenceMonitor `
                -Monitor $monitor `
                -ExitedBeforeSentinelRelease $monitorExitedEarly | Out-Null
        }
        $completion.event.Dispose()
    }
}

function Invoke-BoundlessElevatedErrorTrapFixture {
    $fixtureRoot = Join-Path `
        ([IO.Path]::GetTempPath()) `
        "BoundlessElevatedError-$([guid]::NewGuid().ToString('N'))"
    try {
        New-Item -ItemType Directory -Path $fixtureRoot -ErrorAction Stop | Out-Null
        foreach ($mode in @("try", "finally")) {
            $stagedLog = Join-Path $fixtureRoot "$mode.log"
            $expected = "fixture $mode failure"
            [IO.File]::WriteAllText($stagedLog, "fixture", [Text.Encoding]::Unicode)
            try {
                & {
                    param($Path, $Mode, $Message)
                    $stagedLog = $Path
                    trap {
                        try {
                            $encoded = [uri]::EscapeDataString("$_")
                            [IO.File]::AppendAllText(
                                $stagedLog,
                                "`nBE=$encoded`n",
                                [Text.Encoding]::Unicode
                            )
                        }
                        catch { }
                        break
                    }
                    try {
                        if ($Mode -eq "try") { throw $Message }
                    }
                    finally {
                        if ($Mode -eq "finally") { throw $Message }
                    }
                } $stagedLog $mode $expected
            }
            catch { }
            $actual = Get-BoundlessElevatedInstallErrorFromLog -Path $stagedLog
            if ($actual -ne $expected) {
                throw "Elevated installer $mode error trap fixture lost the original failure."
            }
        }
    }
    finally {
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Invoke-BoundlessLogHandoffFixture {
    param([string]$SelectedUserSid)

    $fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
        "BoundlessLogHandoffFixture-$([guid]::NewGuid().ToString('N'))"
    )
    $stageRoot = Join-Path $fixtureRoot "BoundlessInstaller-$([guid]::NewGuid().ToString('N'))"
    $stagedLog = Join-Path $stageRoot "Boundless-install.log"
    $destination = Join-Path $fixtureRoot "caller\requested.log"
    $expectedElevatedError = "fixture elevated failure`r`nwith reserved characters: % | ?"
    $encodedElevatedError = [uri]::EscapeDataString($expectedElevatedError)
    try {
        New-Item -ItemType Directory -Path $stageRoot -Force -ErrorAction Stop | Out-Null
        [IO.File]::WriteAllText(
            $stagedLog,
            "boundless-log-handoff-fixture`r`nBE=$encodedElevatedError`r`n",
            [Text.Encoding]::Unicode
        )
        $result = Copy-BoundlessInstallerLogHandoff `
            -StageRoot $stageRoot `
            -StagedLogPath $stagedLog `
            -DestinationPath $destination `
            -ProgramDataRoot $fixtureRoot
        if (
            -not $result.copied -or
            -not (Test-Path -LiteralPath $destination -PathType Leaf) -or
            (Test-Path -LiteralPath $stageRoot)
        ) {
            throw "Installer log handoff fixture did not copy and close the one-file stage."
        }
        $elevatedError = Get-BoundlessElevatedInstallErrorFromLog -Path $destination
        if ($elevatedError -ne $expectedElevatedError) {
            throw "Installer log handoff fixture did not preserve the elevated error detail."
        }
        $directorySddl = Get-BoundlessLogHandoffSddl -UserSid $SelectedUserSid
        $fileSddl = Get-BoundlessLogHandoffFileSddl -UserSid $SelectedUserSid
        foreach ($sddl in @($directorySddl, $fileSddl)) {
            if (
                $sddl -notmatch [regex]::Escape(";;;$SelectedUserSid)") -or
                $sddl -notmatch ';;;BA\)' -or
                $sddl -notmatch ';;;SY\)' -or
                $sddl -match ';;;WD\)' -or
                $sddl -match ';;;BU\)'
            ) {
                throw "Installer log handoff OTS fixture did not retain the selected-user/admin boundary."
            }
        }
    }
    finally {
        if (Test-Path -LiteralPath $fixtureRoot) {
            Remove-Item -LiteralPath $fixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

function Invoke-InstallHelperSelfTest {
    Assert-WindowsServiceExecutablePathFixtures
    $validSid = "S-1-5-21-1-2-3-1001"
    $valid = [pscustomobject]@{
        product_registered = $true
        msi_version = "5.0.13"
        display_version = "5.0.13"
        manifest_version = "5.0.13-dogfood.1"
        service_allowed_user_sid = $validSid
        expected_allowed_user_sid = $validSid
        service_binary_path_matches = $true
        service_status = "Running"
        daemon_api_healthy = $true
        daemon_runtime_version = "5.0.13-dogfood.1"
        expected_runtime_version = "5.0.13-dogfood.1"
        executable_versions_match = $true
        input_injector_signature_status = "NotSigned"
        input_injector_unsigned_dogfood = $true
        tray_count = 1
        tray_path_matches = $true
        tray_responding = $true
        tray_stable_milliseconds = 2000
        tray_verification = "passed"
    }
    Assert-PostInstallEvidence -Evidence $valid | Out-Null

    $boundedProcess = Invoke-BoundedProcess `
        -FilePath $env:ComSpec `
        -ArgumentList @("/d", "/c", "echo running=true") `
        -TimeoutSeconds 5
    if ($boundedProcess.exit_code -ne 0 -or $boundedProcess.stdout -notmatch "running=true") {
        throw "Bounded process fixture did not capture a successful command. exit=$($boundedProcess.exit_code) stdout='$($boundedProcess.stdout)' stderr='$($boundedProcess.stderr)'"
    }

    $currentDaemon = ConvertFrom-BoundlessDaemonStatusOutput `
        -Output "running=true daemon_version=5.0.13-dogfood.1 peers=1" `
        -ExpectedVersion "5.0.13-dogfood.1"
    $staleDaemon = ConvertFrom-BoundlessDaemonStatusOutput `
        -Output "running=true daemon_version=5.0.12 peers=1" `
        -ExpectedVersion "5.0.13-dogfood.1"
    if (-not $currentDaemon.healthy -or $staleDaemon.healthy) {
        throw "Daemon status version fixture did not reject a stale running service."
    }

    $parsedTrayVersion = Get-BoundlessVersionFromOutput `
        -Output "boundlesstray 5.0.13-dogfood.1" `
        -ExecutableName "boundlesstray"
    if ($parsedTrayVersion -ne "5.0.13-dogfood.1") {
        throw "Executable version fixture parsed '$parsedTrayVersion'."
    }

    $expectedTrayPath = "C:\Program Files\Boundless\boundlesstray.exe"
    $expectedInjectorPath = "C:\Program Files\Boundless\boundless-input-injector.exe"
    $validInjectorTargets = @(
        [pscustomobject]@{
            id = 901
            session_id = 7
            owner_sid = $validSid
            path = $expectedInjectorPath
        },
        [pscustomobject]@{
            id = 902
            session_id = 7
            owner_sid = $validSid
            path = $expectedInjectorPath
        }
    )
    $acceptedInjectorTargets = @(
        Assert-BoundlessInputInjectorTargets `
            -Processes $validInjectorTargets `
            -ExpectedOwnerSid $validSid `
            -ExpectedSessionId 7 `
            -ExpectedPath $expectedInjectorPath
    )
    if ($acceptedInjectorTargets.Count -ne 2) {
        throw "Input injector target fixture did not preserve every validated process."
    }
    foreach ($mutation in @(
        @{ property = "owner_sid"; value = "S-1-5-21-9-9-9-1002" },
        @{ property = "session_id"; value = 8 },
        @{ property = "path"; value = "C:\Portable\boundless-input-injector.exe" }
    )) {
        $invalidTarget = $validInjectorTargets[0].PSObject.Copy()
        $invalidTarget.($mutation.property) = $mutation.value
        $rejected = $false
        try {
            Assert-BoundlessInputInjectorTargets `
                -Processes @($invalidTarget) `
                -ExpectedOwnerSid $validSid `
                -ExpectedSessionId 7 `
                -ExpectedPath $expectedInjectorPath | Out-Null
        }
        catch {
            $rejected = $true
        }
        if (-not $rejected) {
            throw "Input injector target fixture accepted an invalid $($mutation.property)."
        }
    }
    $correctTray = [pscustomobject]@{
        id = 123
        path = $expectedTrayPath
        responding = $true
    }
    $acceptedTray = Assert-SoleBoundlessTraySnapshot `
        -Processes @($correctTray) `
        -ExpectedTrayPath $expectedTrayPath `
        -Phase "in self-test"
    if ($acceptedTray.id -ne 123) {
        throw "Tray path fixture did not accept the installed path."
    }
    $wrongTrayRejected = $false
    try {
        Assert-SoleBoundlessTraySnapshot `
            -Processes @([pscustomobject]@{
                id = 456
                path = "C:\Portable\Boundless\boundlesstray.exe"
                responding = $true
            }) `
            -ExpectedTrayPath $expectedTrayPath `
            -Phase "in self-test" | Out-Null
    }
    catch {
        $wrongTrayRejected = $true
    }
    if (-not $wrongTrayRejected) {
        throw "Tray path fixture accepted an old or portable executable path."
    }

    $shutdownTarget = [pscustomobject]@{
        id = 789
        session_id = 7
        owner_sid = $validSid
        path = $expectedTrayPath
        responding = $true
    }
    $shutdownTargetArgs = @{
        Processes = @($shutdownTarget)
        ExpectedOwnerSid = $validSid
        ExpectedSessionId = 7
    }
    $acceptedShutdownTargets = @(
        Assert-BoundlessTrayShutdownTargets @shutdownTargetArgs
    )
    if ($acceptedShutdownTargets.Count -ne 1) {
        throw "Tray shutdown target fixture did not retain the proven same-user target."
    }
    $wrongOwnerRejected = $false
    try {
        $wrongOwnerTarget = $shutdownTarget.PSObject.Copy()
        $wrongOwnerTarget.owner_sid = "S-1-5-21-9-9-9-1002"
        $wrongOwnerArgs = @{
            Processes = @($wrongOwnerTarget)
            ExpectedOwnerSid = $validSid
            ExpectedSessionId = 7
        }
        Assert-BoundlessTrayShutdownTargets @wrongOwnerArgs | Out-Null
    }
    catch {
        $wrongOwnerRejected = $true
    }
    if (-not $wrongOwnerRejected) {
        throw "Tray shutdown target fixture accepted another Windows user."
    }
    $legacyNativeTypeCreated = $false
    if ($null -eq ("BoundlessInstallNativeMethods" -as [type])) {
        Add-Type -TypeDefinition @"
using System;
public static class BoundlessInstallNativeMethods
{
    public static bool PostThreadMessage(uint threadId, uint message, UIntPtr wParam, IntPtr lParam)
    {
        return false;
    }
}
"@
        $legacyNativeTypeCreated = $true
    }
    if (
        $legacyNativeTypeCreated -and
        $null -ne [BoundlessInstallNativeMethods].GetMethod("GetProcessOwnerSid")
    ) {
        throw "Legacy native-type fixture unexpectedly exposed the new owner lookup."
    }
    $currentProcessOwnerSid = Get-ProcessOwnerSid -ProcessId $PID
    $currentIdentitySid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    if ($currentProcessOwnerSid -ne $currentIdentitySid) {
        throw "Live process-owner fixture returned $currentProcessOwnerSid; expected $currentIdentitySid."
    }
    Initialize-BoundlessInstallNativeMethods
    if (
        $null -eq [BoundlessInstallNativeMethodsV2].GetMethod("PostThreadMessage") -or
        $null -eq [BoundlessInstallNativeMethodsV2].GetMethod("GetProcessOwnerSid")
    ) {
        throw "Versioned native-type fixture did not load the new owner lookup alongside the legacy type."
    }

    $directSignalSession = 2147483000
    $directSignalName = Get-BoundlessTrayShutdownEventName `
        -UserSid $validSid `
        -SessionId $directSignalSession
    $directSignalCreated = $false
    $directSignalEvent = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset,
        $directSignalName,
        [ref]$directSignalCreated
    )
    if (-not $directSignalCreated) {
        $directSignalEvent.Dispose()
        throw "Direct tray shutdown signal fixture collided with an existing event."
    }
    try {
        if (-not (Request-BoundlessTrayShutdownSignal `
            -ExpectedOwnerSid $validSid `
            -ExpectedSessionId $directSignalSession)) {
            throw "Direct tray shutdown signal fixture did not open the trusted named event."
        }
        if (-not $directSignalEvent.WaitOne(0)) {
            throw "Direct tray shutdown signal fixture did not signal the trusted named event."
        }
    }
    finally {
        $directSignalEvent.Dispose()
    }
    if (Request-BoundlessTrayShutdownSignal `
        -ExpectedOwnerSid $validSid `
        -ExpectedSessionId $directSignalSession) {
        throw "Direct tray shutdown signal fixture reported a missing event as present."
    }
    $shutdownFunctionDefinition = (Get-Command Stop-BoundlessTrayForUpgrade).Definition
    if (
        $shutdownFunctionDefinition -match 'Invoke-BoundedProcess' -or
        $shutdownFunctionDefinition -match '--quit'
    ) {
        throw "Tray shutdown function still executed a user-discovered process image."
    }

    $mutexSecurity = New-BoundlessTrayOwnerMutexSecurity -UserSid $currentIdentitySid
    if (-not (Test-BoundlessTrayOwnerMutexSecurity `
        -Security $mutexSecurity `
        -UserSid $currentIdentitySid)) {
        throw "Tray quiescence mutex fixture did not preserve its ownership DACL."
    }
    $accountAdministratorMutexFixture = "skipped"
    $accountDomainSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.AccountDomainSid
    if ($null -ne $accountDomainSid) {
        $accountAdministratorSid = [Security.Principal.SecurityIdentifier]::new(
            [Security.Principal.WellKnownSidType]::AccountAdministratorSid,
            $accountDomainSid
        ).Value
        $accountAdministratorSecurity = New-BoundlessTrayOwnerMutexSecurity `
            -UserSid $accountAdministratorSid
        if (-not (Test-BoundlessTrayOwnerMutexSecurity `
            -Security $accountAdministratorSecurity `
            -UserSid $accountAdministratorSid)) {
            throw "Tray quiescence mutex fixture did not preserve an account-administrator DACL."
        }
        $accountAdministratorMutexFixture = "passed"
    }
    $selectedSidMutexName = Get-BoundlessTrayOwnerMutexName -UserSid $validSid -SessionId 7
    if ($selectedSidMutexName -ne "Local\Boundless.Tray.SingleInstance.v1.$validSid.7.Owner") {
        throw "Tray quiescence identity fixture did not retain the selected desktop SID and current session."
    }
    $quiescenceFixtureName = "Local\Boundless.Test.UpgradeLease.$PID.$([guid]::NewGuid().ToString('N'))"
    $firstLeaseArgs = @{
        Name = $quiescenceFixtureName
        UserSid = $currentIdentitySid
        InitiallyOwned = $true
    }
    $firstLease = New-BoundlessNamedMutex @firstLeaseArgs
    try {
        if (-not $firstLease.created_new) {
            throw "First tray quiescence fixture did not create the owner mutex."
        }
        $secondLeaseArgs = @{
            Name = $quiescenceFixtureName
            UserSid = $currentIdentitySid
            InitiallyOwned = $false
        }
        $secondLease = New-BoundlessNamedMutex @secondLeaseArgs
        try {
            if ($secondLease.created_new) {
                throw "Second tray quiescence fixture bypassed the held owner mutex."
            }
        }
        finally {
            $secondLease.mutex.Dispose()
        }
    }
    finally {
        if ($firstLease.created_new) {
            $firstLease.mutex.ReleaseMutex()
        }
        $firstLease.mutex.Dispose()
    }

    $monitorFixtureSession = 2147483001
    $monitorFixtureSentinelName = Get-BoundlessTrayQuiescenceSentinelName `
        -UserSid $currentIdentitySid `
        -SessionId $monitorFixtureSession
    $monitorFixtureSentinel = New-BoundlessNamedMutex `
        -Name $monitorFixtureSentinelName `
        -UserSid $currentIdentitySid `
        -InitiallyOwned $true
    if (-not $monitorFixtureSentinel.created_new) {
        $monitorFixtureSentinel.mutex.Dispose()
        throw "Tray quiescence monitor fixture collided with an existing sentinel."
    }
    $monitorFixtureCompletion = New-BoundlessInstallerCompletionEvent -UserSid $currentIdentitySid
    $monitorFixtureTreeJob = "Local\Boundless.Installer.Tree.v1.$([guid]::NewGuid().ToString('N'))"
    $monitorFixture = $null
    $monitorFixtureSentinelReleased = $false
    $monitorFixtureCompleted = $false
    $monitorFixtureError = $null
    try {
        $monitorFixture = Start-BoundlessTrayQuiescenceMonitor `
            -ExpectedOwnerSid $currentIdentitySid `
            -ExpectedSessionId $monitorFixtureSession `
            -SentinelName $monitorFixtureSentinelName `
            -TreeJobName $monitorFixtureTreeJob `
            -CompletionEventName $monitorFixtureCompletion.name
        # Hosted Windows runners can spend well over 10 seconds cold-starting
        # the hidden PowerShell monitor. This is fixture readiness time, not a
        # product shutdown or installer-quiescence budget.
        Wait-BoundlessTrayQuiescenceMonitorReady -Monitor $monitorFixture -TimeoutSeconds 30
        if ($monitorFixture.process.HasExited) {
            throw "Tray quiescence monitor fixture did not remain active after its stable-zero handshake."
        }
        $monitorFixtureExitedEarly = $monitorFixture.process.HasExited
        try {
            $monitorFixtureSentinel.mutex.ReleaseMutex()
        }
        finally {
            $monitorFixtureSentinel.mutex.Dispose()
            $monitorFixtureSentinelReleased = $true
        }
        $monitorFixtureResult = Complete-BoundlessTrayQuiescenceMonitor `
            -Monitor $monitorFixture `
            -ExitedBeforeSentinelRelease $monitorFixtureExitedEarly
        $monitorFixtureCompleted = $true
        if (-not $monitorFixtureResult.completed -or $monitorFixtureResult.exit_code -ne 0) {
            throw "Tray quiescence monitor fixture did not span and close its sentinel window."
        }
    }
    catch {
        $monitorFixtureError = $_
    }
    finally {
        if (-not $monitorFixtureSentinelReleased) {
            try {
                $monitorFixtureSentinel.mutex.ReleaseMutex()
            }
            finally {
                $monitorFixtureSentinel.mutex.Dispose()
                $monitorFixtureSentinelReleased = $true
            }
        }
        if ($null -ne $monitorFixture -and -not $monitorFixtureCompleted) {
            try {
                $monitorFixtureExitedEarly = $monitorFixture.process.HasExited
                Complete-BoundlessTrayQuiescenceMonitor `
                    -Monitor $monitorFixture `
                    -ExitedBeforeSentinelRelease $monitorFixtureExitedEarly | Out-Null
            }
            catch {
                if ($null -eq $monitorFixtureError) {
                    $monitorFixtureError = $_
                }
            }
        }
        $monitorFixtureCompletion.event.Dispose()
    }
    if ($null -ne $monitorFixtureError) {
        throw $monitorFixtureError
    }
    Invoke-BoundlessReplacementTrayWindowFixture -UserSid $currentIdentitySid
    Invoke-BoundlessInstallerSupervisionFixture -UserSid $currentIdentitySid
    Invoke-BoundlessMsiStartedHardKillRecoveryFixture -UserSid $currentIdentitySid
    Invoke-BoundlessInstallerHeartbeatStallFixture -UserSid $currentIdentitySid
    Invoke-BoundlessOwnedProcessTreeFixture
    $kernelObjectAclNegativeProbe = Invoke-BoundlessKernelObjectAclFixture `
        -UserSid $currentIdentitySid
    Invoke-BoundlessCoordinatorDeathFixture -UserSid $currentIdentitySid
    Invoke-BoundlessFailedDrainQuiescenceFixture -UserSid $currentIdentitySid
    Invoke-BoundlessUncertainTransactionGuardianFixture -UserSid $currentIdentitySid
    Invoke-BoundlessRecoveryActionFenceFixture -UserSid $currentIdentitySid
    Invoke-BoundlessRecoveryAuthorityDrainFailureFixture -UserSid $currentIdentitySid
    Invoke-BoundlessBlockingServiceStopFixture
    Invoke-BoundlessFailedMsiServiceRecoveryFixture
    Invoke-BoundlessLogHandoffFixture -SelectedUserSid $validSid
    Invoke-BoundlessElevatedErrorTrapFixture

    $stageSddl = Get-BoundlessAdminOnlyStageSddl
    if (
        $stageSddl -notmatch '\(A;OICI;FA;;;SY\)' -or
        $stageSddl -notmatch '\(A;OICI;FA;;;BA\)' -or
        $stageSddl -match ';;;BU\)' -or
        $stageSddl -match 'S:'
    ) {
        throw "Installer staging security fixture was not an admin-only protected DACL."
    }
    $knownProgramData = Get-BoundlessProgramDataRoot
    $originalProgramDataEnvironment = $env:ProgramData
    try {
        $env:ProgramData = "C:\Users\Public\BoundlessProgramDataPoison"
        $knownProgramDataWithPoisonedEnvironment = Get-BoundlessProgramDataRoot
    }
    finally {
        $env:ProgramData = $originalProgramDataEnvironment
    }
    if (-not (Test-WindowsPathEqual -Left $knownProgramData -Right $knownProgramDataWithPoisonedEnvironment)) {
        throw "Installer staging known-folder fixture trusted the inherited ProgramData environment variable."
    }
    $safeStageFixture = Join-Path $knownProgramData (
        "BoundlessInstaller-" + ("a" * 32)
    )
    $nestedStageFixture = Join-Path $knownProgramData (
        "Boundless\BoundlessInstaller-" + ("a" * 32)
    )
    if (
        -not (Test-BoundlessInstallerStagePath -Path $safeStageFixture) -or
        (Test-BoundlessInstallerStagePath -Path $nestedStageFixture)
    ) {
        throw "Installer staging path fixture accepted an unsafe cleanup boundary."
    }
    $stagingProbeHosts = @(
        Invoke-BoundlessStagingChildProbes -SourcePath $PSCommandPath
    )

    if (
        (Get-BoundlessServiceStopDecision -Status "Stopped" -StopRequested $false) -ne "complete" -or
        (Get-BoundlessServiceStopDecision -Status "StopPending" -StopRequested $false) -ne "wait" -or
        (Get-BoundlessServiceStopDecision -Status "Running" -StopRequested $false) -ne "request_stop" -or
        (Get-BoundlessServiceStopDecision -Status "Running" -StopRequested $true) -ne "wait"
    ) {
        throw "Bounded service-stop state fixture returned an unexpected action."
    }

    $validElevatedResult = [pscustomobject]@{
        status = "passed"
        msi_exit_code = 0
        service_shutdown = [pscustomobject]@{
            force_kill_used = $false
        }
        input_injector_shutdown = $null
        installer_stage = [pscustomobject]@{
            admin_only = $true
            hash_verified = $true
        }
    }
    $validatedElevatedResult = Assert-ElevatedInstallResult -Result $validElevatedResult
    if ($null -ne $validatedElevatedResult.input_injector_shutdown) {
        throw "Elevated install fixture did not preserve the null parent input injector evidence boundary."
    }
    $rebootElevatedResult = $validElevatedResult.PSObject.Copy()
    $rebootElevatedResult.msi_exit_code = 3010
    Assert-ElevatedInstallResult -Result $rebootElevatedResult | Out-Null
    $missingInputInjectorEvidenceRejected = $false
    try {
        $missingInputInjectorEvidence = $validElevatedResult.PSObject.Copy()
        $missingInputInjectorEvidence.PSObject.Properties.Remove("input_injector_shutdown")
        Assert-ElevatedInstallResult -Result $missingInputInjectorEvidence | Out-Null
    }
    catch {
        if ($_.Exception.Message -eq "Elevated Boundless install result omitted the input_injector_shutdown field.") {
            $missingInputInjectorEvidenceRejected = $true
        }
        else {
            throw
        }
    }
    if (-not $missingInputInjectorEvidenceRejected) {
        throw "Elevated install fixture accepted a result with an incomplete input injector shutdown schema."
    }
    $detailedInputInjectorResult = $validElevatedResult.PSObject.Copy()
    $detailedInputInjectorResult.input_injector_shutdown = [pscustomobject]@{
        initial_count = 1
        elapsed_milliseconds = 10
        force_kill_used = $false
    }
    Assert-ElevatedInstallResult -Result $detailedInputInjectorResult | Out-Null
    foreach ($missingMember in @("initial_count", "elapsed_milliseconds", "force_kill_used")) {
        $members = [ordered]@{
            initial_count = 1
            elapsed_milliseconds = 10
            force_kill_used = $false
        }
        $members.Remove($missingMember)
        $malformedInputInjectorResult = $validElevatedResult.PSObject.Copy()
        $malformedInputInjectorResult.input_injector_shutdown = [pscustomobject]$members
        $malformedInputInjectorRejected = $false
        try {
            Assert-ElevatedInstallResult -Result $malformedInputInjectorResult | Out-Null
        }
        catch {
            if ($_.Exception.Message -eq "Elevated Boundless install result input_injector_shutdown omitted '$missingMember'.") {
                $malformedInputInjectorRejected = $true
            }
            else {
                throw
            }
        }
        if (-not $malformedInputInjectorRejected) {
            throw "Elevated install fixture accepted input injector evidence without '$missingMember'."
        }
    }
    $serviceForceKillRejected = $false
    try {
        $invalidElevatedResult = $validElevatedResult.PSObject.Copy()
        $invalidElevatedResult.service_shutdown = [pscustomobject]@{
            force_kill_used = $true
        }
        Assert-ElevatedInstallResult -Result $invalidElevatedResult | Out-Null
    }
    catch {
        $serviceForceKillRejected = $true
    }
    if (-not $serviceForceKillRejected) {
        throw "Elevated install fixture accepted a service force-kill."
    }
    $expectedSourcePackageName = "Boundless-5.0.15-windows-x64.msi"
    $safeSourcePackagePath = Join-Path ([IO.Path]::GetTempPath()) $expectedSourcePackageName
    if (
        (Get-BoundlessInstallerSourcePackageName -Path $safeSourcePackagePath) -cne
        $expectedSourcePackageName
    ) {
        throw "Installer source package fixture did not preserve the canonical release filename."
    }
    foreach ($invalidSourcePackagePath in @(
            "",
            "C:\Temp\Boundless.exe",
            "C:\Temp\Boundless?.msi",
            "C:\Temp\Boundless.msi:stream"
        )) {
        $invalidSourcePackageRejected = $false
        try {
            Get-BoundlessInstallerSourcePackageName `
                -Path $invalidSourcePackagePath | Out-Null
        }
        catch {
            $invalidSourcePackageRejected = $true
        }
        if (-not $invalidSourcePackageRejected) {
            throw "Installer source package fixture accepted an unsafe path '$invalidSourcePackagePath'."
        }
    }

    $sourcePackageFixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
        "BoundlessSourcePackageFixture-$([guid]::NewGuid().ToString('N'))"
    )
    $selfTestInstallerPath = Join-Path $sourcePackageFixtureRoot $expectedSourcePackageName
    New-Item -ItemType Directory -Path $sourcePackageFixtureRoot -Force -ErrorAction Stop | Out-Null
    Copy-Item -LiteralPath $PSCommandPath -Destination $selfTestInstallerPath -ErrorAction Stop
    try {
    $selfTestInstallerItem = Get-Item -LiteralPath $selfTestInstallerPath -Force
    $selfTestInstallerAnchor = [pscustomobject]@{
        path = $selfTestInstallerPath
        sha256 = (Get-FileHash -LiteralPath $selfTestInstallerPath -Algorithm SHA256).Hash
        length = [int64]$selfTestInstallerItem.Length
        last_write_utc_ticks = [int64]$selfTestInstallerItem.LastWriteTimeUtc.Ticks
        product_version = "5.0.13"
        product_code = "{00000000-0000-0000-0000-000000000013}"
    }
    $selfTestPhaseId = [guid]::NewGuid().ToString('N')
    $elevatedCommandArgs = @{
        ResolvedInstallerPath = $selfTestInstallerPath
        Sid = $validSid
        InstallerAnchor = $selfTestInstallerAnchor
        CancellationEventName = ""
        CoordinatorProcessId = $PID
        CoordinatorStartTicks = (
            [Diagnostics.Process]::GetCurrentProcess().StartTime.ToUniversalTime().Ticks
        )
        MonitorMutexName = "Local\Boundless.Installer.Monitor.v1.$([guid]::NewGuid().ToString('N'))"
        TreeJobName = "Local\Boundless.Installer.Tree.v1.$([guid]::NewGuid().ToString('N'))"
        CompletionEventName = "Local\Boundless.Installer.TreeComplete.v1.$([guid]::NewGuid().ToString('N'))"
        ServiceInitialRunningEventName = "Local\Boundless.Installer.ServiceInitialRunning.v1.$selfTestPhaseId"
        MsiMayHaveStartedEventName = "Local\Boundless.Installer.MsiMayHaveStarted.v1.$selfTestPhaseId"
        MsiDefinitiveCompletionEventName = "Local\Boundless.Installer.MsiDefinitiveCompletion.v1.$selfTestPhaseId"
        MsiIdleProvenEventName = "Local\Boundless.Installer.MsiIdleProven.v1.$selfTestPhaseId"
        LogRequested = $true
    }
    $selfTestControlEvent = New-BoundlessInstallerControlEvent -UserSid $currentIdentitySid
    try {
        $elevatedCommandArgs.CancellationEventName = $selfTestControlEvent.name
        $elevatedCommand = New-BoundlessElevatedInstallCommand @elevatedCommandArgs
    }
    finally {
        $selfTestControlEvent.event.Dispose()
    }
    if (
        $elevatedCommand.helper_sha256 -ne $script:BoundlessHelperStartupAnchor.sha256 -or
        $elevatedCommand.installer_sha256 -ne $selfTestInstallerAnchor.sha256 -or
        $elevatedCommand.installer_source_package_name -cne $expectedSourcePackageName -or
        -not $elevatedCommand.log_requested -or
        [IO.Path]::GetFileName($elevatedCommand.staged_log_path) -ne "Boundless-install.log"
    ) {
        throw "Elevated command did not retain startup-anchored helper/MSI hashes."
    }
    $assertedStartupAnchor = Assert-BoundlessHelperStartupAnchor
    if ($assertedStartupAnchor.sha256 -ne $script:BoundlessHelperStartupAnchor.sha256) {
        throw "Helper startup identity fixture did not preserve its anchored hash."
    }
    $installerMutationFixturePath = Join-Path `
        ([IO.Path]::GetTempPath()) `
        "Boundless-InstallerAnchor-$([guid]::NewGuid().ToString('N')).bin"
    Copy-Item -LiteralPath $PSCommandPath -Destination $installerMutationFixturePath -ErrorAction Stop
    try {
        $installerMutationItem = Get-Item -LiteralPath $installerMutationFixturePath -Force
        $installerMutationAnchor = [pscustomobject]@{
            path = $installerMutationFixturePath
            sha256 = (Get-FileHash -LiteralPath $installerMutationFixturePath -Algorithm SHA256).Hash
            length = [int64]$installerMutationItem.Length
            last_write_utc_ticks = [int64]$installerMutationItem.LastWriteTimeUtc.Ticks
        }
        [IO.File]::AppendAllText($installerMutationFixturePath, "changed")
        $installerMutationRejected = $false
        try {
            Assert-BoundlessInstallerAnchor `
                -Anchor $installerMutationAnchor `
                -ResolvedInstallerPath $installerMutationFixturePath | Out-Null
        }
        catch {
            $installerMutationRejected = $true
        }
        if (-not $installerMutationRejected) {
            throw "MSI identity anchor fixture accepted a post-anchor mutation."
        }
    }
    finally {
        Remove-Item -LiteralPath $installerMutationFixturePath -Force -ErrorAction SilentlyContinue
    }
    $decodedElevatedCommand = $elevatedCommand.source
    $commandTokens = $null
    $commandErrors = $null
    [void][System.Management.Automation.Language.Parser]::ParseInput(
        $decodedElevatedCommand,
        [ref]$commandTokens,
        [ref]$commandErrors
    )
    if ($commandErrors.Count -ne 0) {
        throw "Elevated in-memory command fixture did not parse: $($commandErrors[0].Message)"
    }
    if (
        $decodedElevatedCommand -match '\$PSCommandPath' -or
        $decodedElevatedCommand -match '\$env:ProgramData' -or
        $decodedElevatedCommand -match 'payload\.log_path' -or
        $decodedElevatedCommand -match 'S:\(ML;' -or
        $decodedElevatedCommand -notmatch 'BoundlessInstaller-' -or
        $decodedElevatedCommand -notmatch 'PSObject\.BaseObject' -or
        $decodedElevatedCommand -notmatch 'Staged helper hash mismatch' -or
        $decodedElevatedCommand -notmatch 'Join-Path \$stageRoot \$sourcePackageName' -or
        $decodedElevatedCommand -match 'Join-Path \$stageRoot "Boundless\.msi"' -or
        $decodedElevatedCommand -notmatch 'BE='
    ) {
        throw "Elevated command fixture did not enforce immutable helper/MSI staging."
    }
    Invoke-BoundlessElevatedJobSourceFixture `
        -Source $decodedElevatedCommand `
        -UserSid $currentIdentitySid
    Invoke-BoundlessHardCancelBeforeMsiRecoveryFixture `
        -Source $decodedElevatedCommand `
        -UserSid $currentIdentitySid
    }
    finally {
        if (Test-Path -LiteralPath $sourcePackageFixtureRoot) {
            Remove-Item `
                -LiteralPath $sourcePackageFixtureRoot `
                -Recurse `
                -Force `
                -ErrorAction SilentlyContinue
        }
    }

    $msiPropertyFixture = "skipped"
    if (-not [string]::IsNullOrWhiteSpace($InstallerPath)) {
        $resolvedSelfTestInstaller = (Resolve-Path -LiteralPath $InstallerPath).Path
        $selfTestVersion = Get-MsiProperty -Path $resolvedSelfTestInstaller -Property "ProductVersion"
        $selfTestProductCode = Get-MsiProperty -Path $resolvedSelfTestInstaller -Property "ProductCode"
        if ($selfTestVersion -notmatch '^\d+\.\d+\.\d+$' -or $selfTestProductCode -notmatch '^\{[0-9A-Fa-f-]+\}$') {
            throw "MSI property fixture returned unexpected values. ProductVersion=$selfTestVersion ProductCode=$selfTestProductCode"
        }
        $msiPropertyFixture = "passed"
    }

    foreach ($mutation in @(
        @{ name = "registration"; property = "product_registered"; value = $false },
        @{ name = "display_version"; property = "display_version"; value = "5.0.12" },
        @{ name = "version"; property = "manifest_version"; value = "5.0.12" },
        @{ name = "sid"; property = "service_allowed_user_sid"; value = "S-1-5-21-9" },
        @{ name = "service_path"; property = "service_binary_path_matches"; value = $false },
        @{ name = "service"; property = "service_status"; value = "Stopped" },
        @{ name = "api"; property = "daemon_api_healthy"; value = $false },
        @{ name = "daemon_runtime_version"; property = "daemon_runtime_version"; value = "5.0.12" },
        @{ name = "executable_versions"; property = "executable_versions_match"; value = $false },
        @{ name = "input_injector_invalid_signature"; property = "input_injector_signature_status"; value = "HashMismatch" },
        @{ name = "input_injector_signed_mislabeled"; property = "input_injector_signature_status"; value = "Valid" },
        @{ name = "input_injector_unsigned_unlabeled"; property = "input_injector_unsigned_dogfood"; value = $false },
        @{ name = "tray_count"; property = "tray_count"; value = 2 },
        @{ name = "tray_path"; property = "tray_path_matches"; value = $false },
        @{ name = "tray_responsive"; property = "tray_responding"; value = $false },
        @{ name = "tray_stability"; property = "tray_stable_milliseconds"; value = 250 }
    )) {
        $fixture = $valid.PSObject.Copy()
        $fixture.($mutation.property) = $mutation.value
        $failed = $false
        try {
            Assert-PostInstallEvidence -Evidence $fixture | Out-Null
        }
        catch {
            $failed = $true
        }
        if (-not $failed) {
            throw "Post-install verification fixture '$($mutation.name)' was expected to fail."
        }
    }

    [pscustomobject]@{
        status = "passed"
        helper = "Boundless-Install.ps1"
        post_install_fixtures = 16
        bounded_process_fixture = "passed"
        daemon_version_fixture = "passed"
        executable_version_fixture = "passed"
        service_executable_path_fixture = "passed"
        tray_path_fixture = "passed"
        tray_shutdown_identity_fixture = "passed"
        input_injector_target_fixture = "passed"
        native_type_upgrade_compatibility_fixture = "passed"
        legacy_quit_bridge_fixture = "passed"
        direct_shutdown_signal_fixture = "passed"
        tray_quiescence_lease_fixture = "passed"
        account_administrator_mutex_dacl_fixture = $accountAdministratorMutexFixture
        tray_quiescence_monitor_fixture = "passed"
        replacement_tray_window_fixture = "passed"
        supervised_installer_cancellation_fixture = "passed"
        hard_kill_parent_service_recovery_fixture = "passed"
        hard_kill_recovery_failure_fixture = "passed"
        bounded_recovery_elevation_launch_fixture = "passed"
        msi_started_deferred_recovery_fixture = "passed"
        deferred_recovery_idle_race_fixture = "passed"
        stalled_monitor_heartbeat_fixture = "passed"
        coordinator_death_cancellation_fixture = "passed"
        failed_drain_quiescence_fixture = "passed"
        uncertain_transaction_guardian_fixture = "passed"
        stalled_monitor_takeover_fixture = "passed"
        owned_process_tree_fixture = "passed"
        kernel_object_acl_fixture = "passed"
        kernel_object_acl_negative_probe = $kernelObjectAclNegativeProbe
        elevated_process_job_fixture = "passed"
        hard_cancel_before_msi_recovery_fixture = "passed"
        admin_only_stage_fixture = "passed"
        program_data_known_folder_fixture = "passed"
        staging_child_process_probe_hosts = $stagingProbeHosts
        bounded_service_stop_fixture = "passed"
        blocking_service_stop_fixture = "passed"
        recovery_action_fence_fixture = "passed"
        recovery_authority_drain_failure_fixture = "passed"
        start_pending_service_recovery_fixture = "passed"
        failed_msi_service_recovery_fixture = "passed"
        elevated_install_result_fixture = "passed"
        elevated_in_memory_command_fixture = "passed"
        elevated_command_length = $elevatedCommand.encoded_command.Length
        elevated_error_trap_fixture = "passed"
        helper_startup_anchor_fixture = "passed"
        installer_anchor_fixture = "passed"
        installer_source_package_name_fixture = "passed"
        caller_privilege_log_handoff_fixture = "passed"
        msi_property_fixture = $msiPropertyFixture
    } | ConvertTo-Json -Depth 3
}

if ($SelfTest) {
    Invoke-InstallHelperSelfTest
    return
}

if (
    $ElevatedBootstrapServiceRecovery -or
    $ElevatedBootstrapMsiIdleProof -or
    $ElevatedBootstrapMsiIdleServiceRecovery
) {
    $serviceInitialRunning = $null
    $msiMayHaveStarted = $null
    $msiDefinitiveCompletion = $null
    $msiIdleProven = $null
    $recoveryAuthority = $null
    try {
        if (-not (Test-IsAdministrator)) {
            throw "Bootstrap service recovery did not receive an elevated token."
        }
        $stageRoot = Split-Path -Parent $PSCommandPath
        if (-not (Test-BoundlessInstallerStagePath -Path $stageRoot)) {
            throw "Bootstrap service recovery was not running from an immutable installer stage."
        }
        Assert-BoundlessAdminOnlyAcl -Path $PSCommandPath | Out-Null
        if ($ElevatedBootstrapServiceRecovery -or $ElevatedBootstrapMsiIdleServiceRecovery) {
            $recoveryAuthority = Join-BoundlessRecoveryAuthority `
                -JobName $ElevatedBootstrapRecoveryJob `
                -RevocationEventName $ElevatedBootstrapRecoveryRevocationEvent `
                -ActionFenceName $ElevatedBootstrapRecoveryActionFence `
                -ActionCommittedEventName $ElevatedBootstrapRecoveryActionCommittedEvent
        }
        $serviceInitialRunning = Open-BoundlessInstallerPhaseEvent `
            -Name $ElevatedInstallServiceInitialRunningEvent `
            -Phase "ServiceInitialRunning"
        $msiMayHaveStarted = Open-BoundlessInstallerPhaseEvent `
            -Name $ElevatedInstallMsiMayHaveStartedEvent `
            -Phase "MsiMayHaveStarted"
        $msiDefinitiveCompletion = Open-BoundlessInstallerPhaseEvent `
            -Name $ElevatedInstallMsiDefinitiveCompletionEvent `
            -Phase "MsiDefinitiveCompletion"
        $msiIdleProven = Open-BoundlessInstallerPhaseEvent `
            -Name $ElevatedInstallMsiIdleProvenEvent `
            -Phase "MsiIdleProven"
        $bootstrapModeCount = @(
            @(
                $ElevatedBootstrapServiceRecovery,
                $ElevatedBootstrapMsiIdleProof,
                $ElevatedBootstrapMsiIdleServiceRecovery
            ) | Where-Object { [bool]$_ }
        ).Count
        if ($bootstrapModeCount -ne 1) {
            throw "Bootstrap recovery requires exactly one worker mode."
        }
        if ($ElevatedBootstrapServiceRecovery) {
            if (
                -not $serviceInitialRunning.WaitOne(0) -or
                (
                    $msiMayHaveStarted.WaitOne(0) -and
                    -not $msiDefinitiveCompletion.WaitOne(0) -and
                    -not $msiIdleProven.WaitOne(0)
                )
            ) {
                throw "Bootstrap service recovery evidence no longer permits a service start."
            }
            $recovery = Start-BoundlessServiceAfterFailedInstall `
                -TimeoutSeconds 10 `
                -RecoveryAuthority $recoveryAuthority
            Write-Host "boundless_install_bootstrap_recovery_start_requested=$($recovery.start_requested)"
            Write-Host "boundless_install_bootstrap_recovery_final_status=$($recovery.final_status)"
        }
        elseif ($ElevatedBootstrapMsiIdleServiceRecovery) {
            if (
                -not $serviceInitialRunning.WaitOne(0) -or
                -not $msiMayHaveStarted.WaitOne(0)
            ) {
                throw "Bootstrap deferred service recovery was requested without an uncertain transaction."
            }
            if (
                -not $msiDefinitiveCompletion.WaitOne(0) -and
                -not $msiIdleProven.WaitOne(0)
            ) {
                if (-not (Wait-BoundlessWindowsInstallerTransactionIdleProof)) {
                    throw "Windows Installer transaction idle could not be proved before service recovery."
                }
                [void]$msiIdleProven.Set()
            }
            $recovery = Start-BoundlessServiceAfterFailedInstall `
                -TimeoutSeconds 10 `
                -RecoveryAuthority $recoveryAuthority
            Write-Host "boundless_install_bootstrap_recovery_start_requested=$($recovery.start_requested)"
            Write-Host "boundless_install_bootstrap_recovery_final_status=$($recovery.final_status)"
        }
        else {
            if (
                -not $msiMayHaveStarted.WaitOne(0) -or
                $msiDefinitiveCompletion.WaitOne(0)
            ) {
                throw "Bootstrap MSI-idle proof was requested without an uncertain transaction."
            }
            if (-not (Wait-BoundlessWindowsInstallerTransactionIdleProof)) {
                throw "Windows Installer transaction idle could not be proved within the bounded window."
            }
            [void]$msiIdleProven.Set()
        }
        exit 0
    }
    catch {
        Write-Error $_
        exit 1
    }
    finally {
        if ($null -ne $recoveryAuthority) {
            Close-BoundlessRecoveryAuthorityClient -Authority $recoveryAuthority
        }
        if ($null -ne $msiIdleProven) { $msiIdleProven.Dispose() }
        if ($null -ne $msiDefinitiveCompletion) { $msiDefinitiveCompletion.Dispose() }
        if ($null -ne $msiMayHaveStarted) { $msiMayHaveStarted.Dispose() }
        if ($null -ne $serviceInitialRunning) { $serviceInitialRunning.Dispose() }
    }
}

if ($ElevatedInstall) {
    $validatedResultPath = ""
    try {
        if (-not (Test-IsAdministrator)) {
            throw "Internal immutable install phase did not receive an elevated token."
        }
        Assert-AllowedUserSid -Sid $AllowedUserSid
        if ($ExpectedInstallerSha256 -notmatch '^[0-9A-Fa-f]{64}$') {
            throw "Internal immutable install phase received an invalid MSI hash."
        }
        if ($ElevatedInstallTimeoutSeconds -lt 1 -or $ElevatedInstallTimeoutSeconds -gt 3600) {
            throw "Internal immutable install phase received an invalid bounded timeout."
        }
        if ($ElevatedInstallStartGate -notmatch '^Local\\Boundless\.Installer\.StartGate\.v1\.[0-9a-f]{32}$') {
            throw "Internal immutable install phase received an invalid start gate."
        }
        $startGate = [Threading.EventWaitHandle]::OpenExisting($ElevatedInstallStartGate)
        try {
            if (-not $startGate.WaitOne(10000)) {
                throw "Internal immutable install phase was not admitted to its owned process job."
            }
        }
        finally {
            $startGate.Dispose()
        }
        $lifetimeBoundary = Open-BoundlessInstallerCancellationBoundary `
            -EventName $ElevatedInstallCancelEvent `
            -CoordinatorProcessId $ElevatedInstallCoordinatorProcessId `
            -CoordinatorStartTicks $ElevatedInstallCoordinatorStartTicks `
            -MonitorMutexName $ElevatedInstallMonitorMutex
        $initialCancellation = Get-BoundlessInstallerCancellationReason -Boundary $lifetimeBoundary
        if (-not [string]::IsNullOrWhiteSpace($initialCancellation)) {
            Close-BoundlessInstallerCancellationBoundary -Boundary $lifetimeBoundary
            throw "Internal immutable install phase was canceled because $initialCancellation."
        }
        $resolvedElevatedInstallerPath = Resolve-InstallerPath
        $stageRoot = Split-Path -Parent $resolvedElevatedInstallerPath
        if (
            [string]::IsNullOrWhiteSpace($PSCommandPath) -or
            -not (Test-BoundlessInstallerStagePath -Path $stageRoot) -or
            -not (Test-WindowsPathEqual -Left (Split-Path -Parent $PSCommandPath) -Right $stageRoot)
        ) {
            throw "Internal immutable install phase was not running from its verified stage."
        }
        Assert-BoundlessAdminOnlyAcl -Path $PSCommandPath | Out-Null
        $expectedResultPath = Join-Path $stageRoot "Boundless-install-result.txt"
        if (
            [string]::IsNullOrWhiteSpace($ElevatedInstallResultPath) -or
            -not (Test-WindowsPathEqual `
                -Left ([IO.Path]::GetFullPath($ElevatedInstallResultPath)) `
                -Right ([IO.Path]::GetFullPath($expectedResultPath)))
        ) {
            throw "Internal immutable install phase received an invalid result handoff path."
        }
        $validatedResultPath = $expectedResultPath
        $elevatedPhaseArgs = @{
            ResolvedInstallerPath = $resolvedElevatedInstallerPath
            Sid = $AllowedUserSid
            ExpectedInstallerSha256 = $ExpectedInstallerSha256
            CancellationEventName = $ElevatedInstallCancelEvent
            CoordinatorProcessId = $ElevatedInstallCoordinatorProcessId
            CoordinatorStartTicks = $ElevatedInstallCoordinatorStartTicks
            MonitorMutexName = $ElevatedInstallMonitorMutex
            ServiceInitialRunningEventName = $ElevatedInstallServiceInitialRunningEvent
            MsiMayHaveStartedEventName = $ElevatedInstallMsiMayHaveStartedEvent
            MsiDefinitiveCompletionEventName = $ElevatedInstallMsiDefinitiveCompletionEvent
            MsiIdleProvenEventName = $ElevatedInstallMsiIdleProvenEvent
            TimeoutSeconds = $ElevatedInstallTimeoutSeconds
        }
        try {
            $elevatedResult = Invoke-ElevatedInstallPhase @elevatedPhaseArgs
        }
        finally {
            Close-BoundlessInstallerCancellationBoundary -Boundary $lifetimeBoundary
        }
        Write-Host "boundless_install_service_stop_initial=$($elevatedResult.service_shutdown.initial_status)"
        Write-Host "boundless_install_service_stop_final=$($elevatedResult.service_shutdown.final_status)"
        Write-Host "boundless_install_service_stop_elapsed_ms=$($elevatedResult.service_shutdown.elapsed_milliseconds)"
        exit $elevatedResult.msi_exit_code
    }
    catch {
        $originalError = $_
        if (-not [string]::IsNullOrWhiteSpace($validatedResultPath)) {
            try {
                $detail = @("message=$($originalError.Exception.Message)")
                foreach ($key in @($originalError.Exception.Data.Keys)) {
                    $detail += "$key=$($originalError.Exception.Data[$key])"
                }
                [IO.File]::WriteAllText($validatedResultPath, ($detail -join "`r`n"))
                Assert-BoundlessAdminOnlyAcl -Path $validatedResultPath | Out-Null
            }
            catch {
                Write-Warning "Could not persist the staged installer error handoff: $($_.Exception.Message)"
            }
        }
        Write-Error $originalError
        exit 1
    }
}

$selection = Resolve-AllowedUser
Assert-AllowedUserSid -Sid $selection.sid

$summary = [ordered]@{
    selected_user_sid = $selection.sid
    selected_user_account = $selection.account
    selected_user_source = $selection.source
    elevated_process = Test-IsAdministrator
}

if ($ResolveOnly) {
    $summary.status = "resolved"
    $summary | ConvertTo-Json -Depth 3
    return
}

$resolvedInstallerPath = Resolve-InstallerPath
$installerAnchor = New-BoundlessInstallerAnchor -ResolvedInstallerPath $resolvedInstallerPath
$summary.installer_path = $resolvedInstallerPath
$summary.installer_anchor = $installerAnchor

Write-Host "boundless_install_selected_user_sid=$($selection.sid)"
if (-not [string]::IsNullOrWhiteSpace($selection.account)) {
    Write-Host "boundless_install_selected_user_account=$($selection.account)"
}
Write-Host "boundless_install_selected_user_source=$($selection.source)"

$currentSessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
$quiescenceArgs = @{
    # The selected SID is the intended desktop identity captured before UAC.
    # Using the helper process token here breaks over-the-shoulder elevation by
    # leasing an administrator-owned mutex while the real desktop tray remains
    # free to relaunch in this same session.
    ExpectedOwnerSid = $selection.sid
    ExpectedSessionId = $currentSessionId
}
$trayQuiescence = Enter-BoundlessTrayQuiescence @quiescenceArgs
$trayShutdown = $trayQuiescence.evidence.shutdown
Write-Host "boundless_install_tray_shutdown_count=$($trayShutdown.initial_count)"
Write-Host "boundless_install_tray_shutdown_elapsed_ms=$($trayShutdown.elapsed_milliseconds)"
Write-Host "boundless_install_tray_quiescence_acquired=$($trayQuiescence.evidence.acquired)"
try {
    $installResult = Invoke-BoundlessMsi `
        -ResolvedInstallerPath $resolvedInstallerPath `
        -Sid $selection.sid `
        -InstallerAnchor $installerAnchor `
        -QuiescenceLease $trayQuiescence
}
finally {
    $completionState = Update-BoundlessInstallerPhaseEvidence -Lease $trayQuiescence
    $normalQuiescenceReleaseAllowed = Test-BoundlessNormalQuiescenceReleaseAllowed `
        -InstallerTreeClosed $trayQuiescence.evidence.installer_tree_closed `
        -CompletionState $completionState `
        -MsiTransactionIdleProven $trayQuiescence.evidence.msi_transaction_idle_proven `
        -RecoveryAuthorityDrained $trayQuiescence.evidence.recovery_authority_drained `
        -RecoveryActionSettled $trayQuiescence.evidence.recovery_action_settled
    if ($normalQuiescenceReleaseAllowed) {
        Exit-BoundlessTrayQuiescence -Lease $trayQuiescence
    }
    else {
        Resolve-BoundlessUnconfirmedTreeAndQuiescence -Lease $trayQuiescence
    }
}
$exitCode = $installResult.msi_exit_code
Write-Host "boundless_install_exit_code=$exitCode"
Write-Host "boundless_install_service_stop_initial=$($installResult.service_shutdown.initial_status)"
Write-Host "boundless_install_service_stop_final=$($installResult.service_shutdown.final_status)"
if ($null -ne $installResult.service_shutdown.elapsed_milliseconds) {
    Write-Host "boundless_install_service_stop_elapsed_ms=$($installResult.service_shutdown.elapsed_milliseconds)"
}
if ($null -ne $installResult.input_injector_shutdown) {
    Write-Host "boundless_install_input_injector_shutdown_count=$($installResult.input_injector_shutdown.initial_count)"
    Write-Host "boundless_install_input_injector_shutdown_elapsed_ms=$($installResult.input_injector_shutdown.elapsed_milliseconds)"
    Write-Host "boundless_install_input_injector_force_kill=$($installResult.input_injector_shutdown.force_kill_used)"
}
$verification = Invoke-PostInstallVerification `
    -InstallerAnchor $installerAnchor `
    -ExpectedAllowedUserSid $selection.sid `
    -LaunchTray:(-not $Quiet -and -not (Test-IsAdministrator))
$summary.pre_install_tray_shutdown = $trayShutdown
$summary.pre_install_tray_quiescence = $trayQuiescence.evidence
$summary.elevated_install = $installResult
$summary.post_install_verification = $verification
$summary.status = if ($verification.tray_verification -eq "passed") {
    "installed_and_verified"
}
else {
    "installed_core_verified_tray_deferred"
}
Write-Host "boundless_install_core_verified=true"
Write-Host "boundless_install_tray_verification=$($verification.tray_verification)"
$summary | ConvertTo-Json -Depth 5
