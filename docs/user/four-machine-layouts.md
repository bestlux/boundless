# Four-Machine Layouts

Boundless v5 is designed to exceed the old one-row/two-by-two Mouse Without Borders layout model by validating one-row, grid, and cardinal arrangements. The configuration/unit-test limit is four remote peers plus the local machine; release-grade runtime parity is still measured against the four-computer Mouse Without Borders target until the readiness packet includes broader physical or deterministic runtime evidence.

## Rules

- A layout must include `This PC` exactly once.
- Devices must form one connected cardinal group.
- Diagonal-only or isolated devices are rejected.
- More than four remote peers are rejected for v5.
- Tokens can be `self`, `local`, `me`, machine id, peer id, or unambiguous peer display names.

## Examples

One row:

```powershell
$BoundlessCtl = "$env:ProgramFiles\Boundless\boundlessctl.exe"
& $BoundlessCtl layout set "left,self,right"
```

Cross layout:

```powershell
& $BoundlessCtl layout set ",up,;left,self,right;,down,"
```

Preview before changing active behavior:

```powershell
& $BoundlessCtl layout preview
```

## Validation Expectations

The release readiness packet must include deterministic topology validation and, for release candidates, runtime evidence from multi-node smoke or equivalent deterministic harnesses.

Mixed-DPI and multi-monitor behavior must be validated with input trace evidence before claiming complete real-world parity.
