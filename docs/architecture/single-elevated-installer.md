# Single Elevated Installer Spike

Status: BND-NEXT-9B-3 MSI-owned service lifecycle.

This records the first implementation slice for the Windows one-install,
full-capability path. It is intentionally not a complete service-mode release
claim.

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

`packaging/windows/installer/Package.wxs` now has the 9B-3 machine-wide
service lifecycle path:

- `Scope="perMachine"`.
- `INSTALLDIR` is under `ProgramFiles64Folder\Boundless`.
- `boundless-service.exe` is in its own component and is registered by
  `ServiceInstall` as `BoundlessService`.
- `ServiceControl` starts the service on install, stops it around
  install/uninstall lifecycle actions, and removes it on uninstall.
- `BOUNDLESS_ALLOWED_USER_SID` is a secure public MSI property and install/repair
  fail closed when it is missing or malformed.
- Tray startup is deferred; the skeleton does not create a Startup-folder
  shortcut because selected desktop-user ownership is unresolved.
- Component key paths and installer evidence are under HKLM.

`scripts/dev/installer-smoke.ps1` is aligned with the 9B-3 service lifecycle:

- install root is `%ProgramFiles%\Boundless`;
- HKLM uninstall and `HKLM\Software\Boundless\Installer` evidence are
  validated;
- install is expected to register and start `BoundlessService` from
  `%ProgramFiles%\Boundless\boundless-service.exe`;
- service path, LocalSystem account, AutoStart, selected SID argument, daemon
  API health, and uninstall removal are validated when the smoke runs elevated.

`scripts/dev/service-smoke.ps1` remains a manual/developer fallback workflow:

- copies `boundless-service.exe` into `%ProgramFiles%\Boundless`;
- installs `BoundlessService` with the CLI;
- validates start/status/daemon API health/stop/uninstall.

`docs/user/service-mode.md` and `docs/release/v5-release-hardening.md` record
the present limit: service lifecycle is MSI-owned, while tray sign-in startup,
automatic intended-user SID selection, deeper N-1/repair evidence, and
interactive-desktop parity remain follow-up work. BND-NEXT-44 owns ordinary
elevated applications; secure-desktop and lock-screen claims remain under 9C.

## v5.0.15 One-User Dogfood Exception

BND-NEXT-44 is planned, not implemented. For the first v5.0.15 dogfood build,
the canonical MSI may install an unsigned `requireAdministrator` input injector
under `%ProgramFiles%\Boundless` when all of these constraints are enforced:

- elevated-app input is disabled until the configured allowed user explicitly
  enables or launches it;
- Windows displays the expected **Unknown Publisher** UAC prompt, and the tray
  reports `unsigned dogfood` rather than signed, trusted, or production-ready;
- the allowed user and elevated target applications belong to the same
  split-token administrator account;
- the injector exposes only incoming input injection and held-input release;
- cancellation, sign-in, tray relaunch, injector crash, service restart, repair,
  upgrade, and automatic retry never launch it or generate another UAC prompt;
  only another explicit user action may do so; and
- repair, upgrade, and uninstall preserve or remove the injector through normal
  MSI component ownership without leaving an elevated process or orphaned file.

This is an explicit one-user dogfood exception, not a signing substitute.
Unsigned code cannot use the UIAccess path and cannot support a trusted-publisher
claim. Trusted Authenticode signing plus the UIAccess product-policy decision
remain prerequisites before setting `uiAccess=true` or presenting elevated-app
input as polished/trusted. UAC consent or credential desktops, lock screen,
Winlogon, other sessions, and standard-user-to-alternate-admin control remain
unsupported.

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
- install any BND-NEXT-44 injector as a separately owned Program Files component
  with explicit launch semantics, at-most-one-per-session lifecycle, and honest
  signing/capability status;
- keep service/tray update application MSI-owned, with no service or tray
  self-updater;
- avoid secure-desktop or lock-screen claims until BND-NEXT-9C proves them on
  Windows; ordinary elevated-app support is the narrower BND-NEXT-44 slice.

## WiX Delta

9B-2 applied the package scope, directory, payload split, and HKLM evidence.
9B-3 adds MSI-owned `ServiceInstall`/`ServiceControl` rows for
`BoundlessService`.

Applied package and directory changes:

```xml
<Package ... Scope="perMachine" InstallerVersion="500" Compressed="yes">
  ...
</Package>

<StandardDirectory Id="ProgramFiles64Folder">
  <Directory Id="INSTALLDIR" Name="Boundless" />
</StandardDirectory>
```

Applied component split and service lifecycle:

- `boundless-service.exe` is in its own component;
- the service executable file is `KeyPath="yes"` so SCM registration resolves
  to the MSI-owned Program Files file;
- `BoundlessService` is LocalSystem, AutoStart, and receives exactly one
  `--allowed-user-sid=[BOUNDLESS_ALLOWED_USER_SID]` argument;
- `ServiceControl` owns start, stop, and removal for the service component;
- machine-wide key paths moved to HKLM instead of HKCU;
- tray/daemon/CLI payloads remain in a separate payload component.

`Boundless.Installer.wixproj` intentionally suppresses ICE43 and ICE57 for the
9B-2 skeleton. Those ICEs expect non-advertised shortcuts to use an HKCU key
path, but this MSI creates machine-wide common Start Menu/Desktop shortcuts with
HKLM component key paths and no Startup shortcut. Installer smoke and MSI table
inspection both keep Startup shortcut creation as a negative assertion until
selected-user tray startup is implemented separately.

Applied service registration:

```xml
<Property Id="BOUNDLESS_ALLOWED_USER_SID" Secure="yes" />

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

`BOUNDLESS_ALLOWED_USER_SID` must be declared as a secure public property before
it is used from the elevated execute/server side of the install. Without
`Secure="yes"` / `SecureCustomProperties`, an explicit command-line fallback can
be lost during managed elevation and the service can be installed with an empty
or unintended SID.

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

The remaining installer-design blocker is safely identifying the intended
desktop user from an elevated install without user-supplied MSI properties.

9B-5 adds a small PowerShell helper as the preferred user-facing launch path.
Run from the intended desktop user's normal, non-elevated PowerShell session,
`Boundless-<version>-windows-x64-install.ps1` captures that user's SID before
UAC and launches the MSI with the secure `BOUNDLESS_ALLOWED_USER_SID` property.
If the helper is already elevated, it refuses to infer the current user unless
the caller passes an explicit SID/account or uses an explicit current-user
override. This keeps same-user elevation supported without silently authorizing
a different admin account.

The MSI still keeps the explicit SID property path as the enforcement boundary:

- installers must pass a numeric SID-shaped `BOUNDLESS_ALLOWED_USER_SID=S-...`;
- the property is secure and survives into the elevated execute context;
- MSI launch conditions reject missing and obviously malformed SID values;
- service runtime validates the same SID shape before reporting `Running`;
- installer-smoke records the selected SID without logging unrelated local
  identities.

The MSI must not silently authorize the elevation account when it differs from
the intended desktop user.

## Validation Plan

9B-2 introduced a machine-wide skeleton without service registration and
validated:

- `dotnet build packaging/windows/installer/Boundless.Installer.wixproj`;
- install root under `%ProgramFiles%\Boundless`;
- HKLM uninstall/install evidence;
- tray/daemon/CLI/service payload presence;
- uninstall removes the Program Files install root.

9B-3 enables service ownership and should validate from an elevated shell:

- install registers `BoundlessService`;
- service image path is `%ProgramFiles%\Boundless\boundless-service.exe`;
- start type is AutoStart;
- service command line includes exactly one non-empty intended
  `--allowed-user-sid`;
- install/repair/upgrade stops and restarts the service as expected;
- failed install or upgrade after service stop, service install, or service
  start failure leaves either the previous service restored/running or a
  documented fail-closed state with no orphaned LocalSystem service or active
  Program Files payload drift;
- uninstall removes service registration and installed payloads;
- `scripts/dev/service-smoke.ps1` or an installer-owned variant proves pipe ACL,
  daemon API health, version parity, process cleanup, and service removal.

Interactive-desktop parity remains separate from MSI service ownership:

- BND-NEXT-44 owns ordinary elevated-app behavior through a minimal user-session
  input injector installed under Program Files; physical capture and clipboard
  handling remain unelevated;
- the experimental `requireAdministrator` path must validate canonical MSI
  ownership, the same split-token administrator SID/session, full administrator
  token, one explicit **Unknown Publisher** UAC prompt, `unsigned dogfood`
  status, minimal injection/release surface, at-most-one lifecycle, no automatic
  prompts, and clean repair/upgrade/uninstall;
- a UIAccess or polished/trusted-publisher path additionally requires trusted
  Authenticode validation and, for UIAccess, proof of `TokenUIAccess=1` plus the
  recorded product-policy decision;
- cold-boot service availability remains independent lifecycle evidence;
- BND-NEXT-9C retains lock-screen, Winlogon, and secure-desktop behavior after
  the ordinary elevated-window slice is proven.

## Follow-Up Slices

9B-2: add a machine-wide installer skeleton installed under Program Files, keep
service registration disabled, and update installer smoke for machine-wide
payload install/uninstall evidence.

9B-3: add MSI-owned `BoundlessService` registration/autostart, allowed-user SID
selection, service lifecycle validation, and release-readiness evidence.

BND-NEXT-44: implement and validate ordinary elevated-application control through
a minimal-surface Program Files input injector without elevating the tray or its
capture and clipboard responsibilities. The one-user dogfood fallback may be an
explicitly enabled unsigned `requireAdministrator` binary with an **Unknown
Publisher** UAC prompt and `unsigned dogfood` status; it carries a full
administrator token despite its narrow interface. UIAccess remains available
only after trusted signing, policy approval, and proof.

9C: separately prove or falsify lock-screen, Winlogon, and secure-desktop
control on real Windows desktops before changing those parity claims.

## References

- FireGiant WiX `Package` scope documentation:
  https://docs.firegiant.com/wix/schema/wxs/packagescopetype/
- FireGiant WiX `ServiceInstall` documentation:
  https://docs.firegiant.com/wix/schema/wxs/serviceinstall/
- FireGiant WiX `ServiceControl` documentation:
  https://docs.firegiant.com/wix3/xsd/wix/servicecontrol/
