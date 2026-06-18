# Boundless V5 Topology Contract

This document defines the v5 topology behavior behind the Mouse Without Borders parity matrix.

## Supported Layout Size

Boundless v5 stores and validates layouts with This PC plus up to four paired peers in a single applied layout.

The Mouse Without Borders parity target remains four computers total. Any public claim that Boundless supports a five-device runtime topology requires its own release evidence. Until then, This PC plus four peers is a configuration limit and unit-tested validation contract, not a release-validated runtime claim.

## Layout Grammar

Layouts are stored as a matrix string:

- rows are separated by `;`,
- columns are separated by `,`,
- empty cells are allowed,
- This PC may be written as `self`, `local`, `this`, `me`, the local machine id, or the local display name where that display name is available,
- peers may be written as full peer id, unique peer-id prefix, or exact display name.

Examples:

```text
self,right
left,self,right
,up,;left,self,right;,down,
```

## Apply-Time Validation

Daemon, CLI, and tray Apply use the same rules:

- exactly one local cell is required,
- every non-empty peer token must resolve to a known paired peer,
- ambiguous peer tokens are rejected,
- a peer may appear only once,
- no more than four remote peers may appear,
- every placed device must be part of one cardinally connected group.

The connected-group rule intentionally rejects diagonal-only or isolated devices. Users can leave empty cells around the group, but a placed device cannot be reachable only by a diagonal or by an implied scan across blank space.

## Disconnected Peers

Disconnected paired peers may remain in the layout. Tray layout tiles should surface their offline state, switch-all target selection skips them until connected, and edge handoff treats their occupied cells as blockers rather than pass-through empty space.

This preserves user intent during transient reconnects while preventing disconnected peers from silently becoming active capture targets.

## Current Validation Evidence

Milestone V5-2 added targeted unit coverage for:

- four remote peers plus This PC passing shared validation,
- unknown, ambiguous, duplicate, isolated, and oversized layouts failing,
- non-canonical display-name and peer-prefix tokens being persisted as canonical peer IDs,
- daemon `set_layout` enforcing the shared contract,
- switch-all ordering across a four-peer grid while skipping disconnected peers,
- edge handoff stopping at offline or stale occupied cells,
- tray layout rebuild behavior when peers change,
- tray hydration for peer-id prefix and case-insensitive display-name tokens.

Release validation still needs runtime evidence for real two-, three-, and four-machine handoff and reconnect behavior before the matrix row can be marked `validated`. Five-device runtime support must stay unclaimed unless the readiness packet adds explicit five-device evidence.
