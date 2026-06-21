# Single Elevated Installer Spike

Status: BND-NEXT-9B-1 spike plan.

This records the first implementation slice for the Windows one-install,
full-capability path. It is intentionally not a release claim and does not
change the shipping MSI yet.

## Decision

Use one elevated machine-wide MSI as the primary Windows installer route.

Current repo evidence does not require a bootstrapper fallback. WiX exposes the
needed package scope and service lifecycle primitives, and the existing
`Boundless.Installer.wixproj` already builds with WiX 6 plus the util
extension. Keep a single bootstrapper as a fallback only if the machine-wide MSI
cannot safely resolve the intended desktop user SID or tray startup ownership
without custom UI.

Do not ship a user-facing app MSI plus service MSI flow for this path. That
would preserve the current split between LocalAppData payload updates and an
admin-owned service binary, which is the state BND-NEXT-9B is meant to remove.

## Current Evidence

`packaging/windows/installer/Package.wxs` is a per-user package:

- `Scope="perUser"`.
- `INSTALLDIR` is under `LocalAppDataFolder\Programs\Boundless`.
- `boundless-service.exe` is included as payload, but no `ServiceInstall` or
  `ServiceControl` owns `BoundlessService`.
- Tray startup is a shortcut in `StartupFolder`.
- Component key paths are HKCU registry values, which match the current per-user
  install but not a machine-wide service-owning package.

`scripts/dev/installer-smoke.ps1` is aligned with that current shape:

- install root is `%LocalAppData%\Programs\Boundless`;
- uninstall is expected to leave no registered `BoundlessService`;
- service payload version evidence is collected, but service registration is
  explicitly outside the installer smoke.

`scripts/dev/service-smoke.ps1` validates a separate admin workflow:

- copies `boundless-service.exe` into `%ProgramFiles%\Boundless`;
- installs `BoundlessService` with the CLI;
- validates start/status/daemon API health/stop/uninstall.

`docs/user/service-mode.md` and `docs/release/v5-release-hardening.md` honestly
record the present limit: service mode is admin/CLI-owned, MSI-owned payload
updates are supported, and the active admin-registered service binary is not yet
owned by the installer.

## Target Installer Shape

The full-capability MSI should:

- require elevation with `Scope="perMachine"`;
- install payloads under `%ProgramFiles%\Boundless`;
- register `BoundlessService` against the MSI-owned
  `%ProgramFiles%\Boundless\boundless-service.exe`;
- set the service account to LocalSystem and start type to AutoStart;
- pass exactly one allowed desktop user SID to the service command line;
- stop the service during upgrade/uninstall and remove it on uninstall;
- start the service on install/repair/upgrade;
- start the tray at sign-in for the selected desktop user;
- keep service/tray update application MSI-owned, with no service or tray
  self-updater;
- avoid lock-screen, secure desktop, or elevated-app claims until BND-NEXT-9C
  proves them on Windows.

## WiX Delta

The production `Package.wxs` should not be mutated directly until 9B-2 because
the following changes are coupled and would invalidate the existing installer
smoke in one step.

Required package and directory changes:

```xml
<Package ... Scope="perMachine" InstallerVersion="500" Compressed="yes">
  ...
</Package>

<StandardDirectory Id="ProgramFiles64Folder">
  <Directory Id="INSTALLDIR" Name="Boundless" />
</StandardDirectory>
```

Required component split:

- put `boundless-service.exe` in its own component;
- mark the service executable file as `KeyPath="yes"` so the registered service
  image path points at the MSI-owned Program Files file;
- move machine-wide key paths to HKLM instead of HKCU;
- keep tray/daemon/CLI payloads in separate components or component groups if
  needed for repair and future feature ownership.

Service registration skeleton:

```xml
<Component Id="BoundlessServiceComponent" Directory="INSTALLDIR" Guid="PUT-GUID-HERE">
  <File
    Id="ServiceBinaryFile"
    Source="$(var.PayloadDir)\boundless-service.exe"
    KeyPath="yes" />
  <ServiceInstall
    Id="BoundlessServiceInstall"
    Name="BoundlessService"
    DisplayName="Boundless Service"
    Description="Boundless service-mode daemon host."
    Type="ownProcess"
    Start="auto"
    ErrorControl="normal"
    Account="LocalSystem"
    Arguments="--allowed-user-sid=[BOUNDLESS_ALLOWED_USER_SID]" />
  <ServiceControl
    Id="BoundlessServiceControl"
    Name="BoundlessService"
    Start="install"
    Stop="both"
    Remove="uninstall"
    Wait="yes" />
  <RegistryValue
    Root="HKLM"
    Key="Software\Boundless\Installer"
    Name="ServiceInstalled"
    Type="integer"
    Value="1"
    KeyPath="no" />
</Component>
```

The `ServiceInstall` element supports service command-line arguments and
AutoStart, and the installed service executable resolves from the parent
component key-path file. `ServiceControl` covers start, stop, and uninstall
removal for the service component.

## Tray Startup

The first full installer should configure tray startup for the selected
installing desktop user, not every user on the machine.

Preferred first route for 9B-2/9B-3:

- keep a per-user Startup shortcut only for the selected desktop user, or
- create an installer-owned scheduled task scoped to that user at logon if the
  elevated MSI cannot reliably write that user's Startup folder.

Do not use Common Startup as the default until multi-user behavior is designed.
A common shortcut would launch the tray for every logging-in user while the
service ACL authorizes only one user SID.

## Allowed User SID

The blocker for a direct `Package.wxs` conversion is not service registration;
it is safely identifying the intended desktop user from an elevated install.

9B-2 should add one explicit SID resolution path:

- if the installer is launched elevated by the same interactive user, set
  `BOUNDLESS_ALLOWED_USER_SID` from that user's token;
- if UAC elevation switches to another admin account, fail closed with a clear
  message or require an explicit MSI property such as
  `BOUNDLESS_ALLOWED_USER_SID=S-...`;
- record the selected SID in installer-smoke evidence without logging unrelated
  local identities.

The MSI must not silently authorize the elevation account when it differs from
the intended desktop user.

## Validation Plan

9B-2 should introduce a machine-wide skeleton without service registration
first, then validate:

- `dotnet build packaging/windows/installer/Boundless.Installer.wixproj`;
- install root under `%ProgramFiles%\Boundless`;
- HKLM uninstall/install evidence;
- tray/daemon/CLI/service payload presence;
- uninstall removes the Program Files install root.

9B-3 should enable service ownership, then validate from an elevated shell:

- install registers `BoundlessService`;
- service image path is `%ProgramFiles%\Boundless\boundless-service.exe`;
- start type is AutoStart;
- service command line includes the intended `--allowed-user-sid`;
- install/repair/upgrade stops and restarts the service as expected;
- uninstall removes service registration and installed payloads;
- `scripts/dev/service-smoke.ps1` or an installer-owned variant proves pipe ACL,
  daemon API health, version parity, process cleanup, and service removal.

9C remains separate:

- cold-boot service availability;
- elevated-app behavior;
- lock-screen and secure-desktop behavior;
- any service-to-session broker or UIAccess helper required by Windows session
  isolation.

## Follow-Up Slices

9B-2: add a machine-wide installer skeleton installed under Program Files, keep
service registration disabled, and update installer smoke for machine-wide
payload install/uninstall evidence.

9B-3: add MSI-owned `BoundlessService` registration/autostart, allowed-user SID
selection, service lifecycle validation, and release-readiness evidence.

9C: prove or falsify lock-screen, secure desktop, and elevated-app control on
real Windows desktops before changing parity claims.

## References

- FireGiant WiX `Package` scope documentation:
  https://docs.firegiant.com/wix/schema/wxs/packagescopetype/
- FireGiant WiX `ServiceInstall` documentation:
  https://docs.firegiant.com/wix/schema/wxs/serviceinstall/
- FireGiant WiX `ServiceControl` documentation:
  https://docs.firegiant.com/wix3/xsd/wix/servicecontrol/
