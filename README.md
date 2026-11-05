# WorldEdit for Pumpkin

A WorldEdit-style plugin for Pumpkin, modeled on [FastAsyncWorldEdit](https://github.com/Intellectualsites/FastAsyncWorldEdit).

## Installation

Built for Pumpkin **0.2.1+26.3-26.51**. Install `worldedit_pumpkin.wasm` in the server's plugin directory.

## Features

- Select regions with `//wand`, `//pos1` and `//pos2`.
- Edit blocks with commands such as `//set`, `//replace`, `//walls`, `//move` and `//stack`.
- Use shapes, masks, patterns, brushes, clipboard operations, undo and redo.
- Load Sponge schematic v2 and v3 files and save Sponge v3 files.
- Share selections and check edits against compatible WorldGuard for Pumpkin builds.

Commands use `WorldEdit:` permission nodes and default to operator access. Edits run through a tick-budgeted queue.

When upgrading from `WorldEditPumpkin`, stop the server and copy `plugins/data/WorldEditPumpkin` to `plugins/data/WorldEdit` to preserve configuration and schematics. Update permission grants and plugin overrides to use `WorldEdit`, and upgrade WorldGuard to a build that supports the renamed plugin.

## Limitations

The command and brush selection is a subset of FastAsyncWorldEdit. Copy and paste do not include entities or biomes, and rotation is limited to the Y axis. FAWE-specific compressed schematics are not supported. Test edits, schematics and protection in a scratch world before relying on them.

## Build

```sh
cargo test --workspace
cargo build --locked --offline --release --target wasm32-wasip2 -p worldedit-pumpkin
```

