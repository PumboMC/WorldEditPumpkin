# Pumpkin 0.2.1 API audit

Target: stable `0.2.1+26.3-26.51` (`224f9f814`), WIT **v0.1**, not v0.2. This audits interfaces relevant to this plugin, not every unrelated SDK feature.

| Area | Result |
| --- | --- |
| Player lookup | Online name/UUID lookup is available; the plugin already resolves online command actors. No offline profile service was found. |
| Block states and registry | Used for edits, masks, patterns and suggestions. Block-tag enumeration is not exposed. |
| Block entities | Capture, paste and history now carry NBT through the host adapter. Pumpkin uses unnamed-root NBT; the engine uses named-root NBT. Sponge separates the entity ID from its data. |
| Replacement | The host NBT setter does not evict a live entity. The adapter resets it through air with no-drop/callback flags and suppresses empty-entity creation before restoring NBT. |
| NBT codec | Added long-array tags. Other codec limits, including Java modified-UTF-8 compatibility and parser hardening, still need a separate audit. |
| Chunk access | `world.get-chunk` only returns already-loaded chunks. It does not preload edit destinations. Writes outside loaded chunks remain a limitation. |
| World bounds | Minimum Y is exposed; maximum Y is not. The plugin retains configured height. Generation-buffer height is not a live-world height API. |
| Scheduling | Uses the host repeating-task API for the edit queue and cancels it on unload. |
| Commands | Uses host suggestions; handler requirements remain a host stub. Fine-grained checks stay in command handlers. |
| Protection | Matching WorldGuard builds authorize actual edit/paste/brush/history writes through authenticated IPC; an activated but unavailable provider refuses writes. |
| Entities and biomes | Some world/entity and biome-read APIs exist, but entity/biome clipboard editing is not implemented. |

## Verification

Workspace tests include fixed NBT bytes, framing conversion and schematic ID restoration. Release Wasm builds are tested separately from native tests.

A scratch Wasm probe imported the actual adapter and checked chest loot-table NBT capture, paste, overwrite of a live chest, restoration, clearing, and copying three diamonds in an ordinary chest on the official 0.2.1 binary. The final probe passed seven consecutive boots. One earlier restart lost a loot-table seed during replacement; its cause remains unresolved. The host's pending/live entity handling is not atomic across plugin calls, so do not treat the reset workaround as a reliability guarantee. Every block entity, rotated data, client updates and persisted paste results remain unverified.

In-game checks: copy a chest with items and a sign, paste over existing copies, undo/redo, save/load a schematic, and restart. Check contents, text and facing. Use a scratch world; matching builds now check WorldGuard but this does not establish general protection parity.

### Weighted `//set` verification — October 31, 2026

`//set` now accepts a greedy pattern: the public release host's quotable-string parser stops at `%` before a weighted pattern reaches the engine. Outer single/double quotes remain supported.

On the public 0.2.1 Windows binary (SHA-256 `13daca2a4ee351ca4ad91c9ed94ffc8e7d9e3eb41cdc96e107accf9d1f522812`), an isolated protocol-777 client selected a loaded 8×8×8 volume, filled it with stone, ran `//set 50%air,50%stone`, and counted **258 air + 254 stone = 512 blocks**. Undo completed. The probe waits for the destination chunk to load; this does not remove the unloaded-chunk limitation above. It does not establish graphical-client rendering or general edit/history reliability.

### WorldGuard bridge — November 2, 2026

WorldGuard activates a sticky write-authorization bridge during load. Edit/paste/brush/replay writes are checked against the online actor, exact level/dimension and block position, with break/place flags chosen from the replacement. Failed transport, malformed replies and offline/unknown actors refuse writes once activated. Refused writes consume queue budget and are excluded from counts/history. A native regression covers all three job types; wire tests reject invalid/truncated authorization requests.

The scratch protocol-777 test refused all 512 nonmember writes and preserved stone, allowed the same edit after membership, and refused writes in a redefined region's new chunk immediately. Weighted edits/undo still passed with operator bypass. The guard has not been performance-benchmarked or independently audited after this change; console writes without an online player are refused while active. WorldGuard's remaining gameplay gaps still apply.

### History and queue follow-up — November 3, 2026

Undo/redo now uses a finite-budget FIFO queue shared with edits and history clearing. History entries retain exact level/dimension identity rather than one mutable session world. Successfully replayed blocks move to the opposite history stack using their actual before/after values; refused changes remain on the source stack for retry and are not reported as successful. Partial-history stacks remain bounded by the configured history size. Session identities prevent queued work from attaching to a later login with the same name.

Skipped/masked/identical positions consume budget, and edits/pastes stopped by the block limit report partial completion. The locked native workspace suite passes 320 tests; regressions cover skips, mixed-dimension metadata, multi-tick replay, partial refusal/retry/redo and history bounds. These tests do not remove the host's unloaded-chunk limitation or prove all live-server behavior.
