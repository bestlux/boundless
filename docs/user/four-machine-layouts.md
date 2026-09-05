# Four-Machine Layouts

Boundless can validate one-row, grid, and cardinal arrangements. The configuration limit is four remote peers plus the local machine. That is a parser/policy limit, not a validated five-PC product claim. The immediate product qualification target is two Windows PCs; broader layouts remain in the [roadmap](../v5-roadmap.md).

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

The release readiness packet must distinguish layout validation, actual multi-daemon transport tests, and physical multi-PC input tests. A layout validator test cannot establish runtime topology or input handoff behavior.

Mixed-DPI and multi-monitor behavior must be validated with input trace evidence before claiming complete real-world parity.
