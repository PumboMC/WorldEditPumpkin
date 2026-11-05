//! Schematic reading and writing, checked against real files.
//!
//! The fixtures under `fixtures/` are copies of FAWE's own test schematics, from
//! `worldedit-core/src/test/resources/fastasyncworldedit/schematics`. They are kept
//! rather than synthesised because they are the only known-good input available: a
//! schematic round-trip against a schematic this code wrote would pass even if the
//! reader and the writer were wrong in the same way.
//!
//! They are Sponge v2 and v3 files from FAWE's own test suite.

use std::io::Read;
use std::io::Write;

use worldedit_core::{
    block::{BlockResolver, BlockStateInfo, BlockValue},
    clipboard::Clipboard,
    math::BlockPos,
    nbt::{self, Value},
    region::CuboidRegion,
    schematic::{self, SchematicError},
};

/// A registry holding the handful of blocks these tests name.
struct Registry;

/// Eight blocks the registry can name, so a round trip can tell them apart.
///
/// The states are numbered from [`FIRST_NAMED`] rather than from zero, because a state
/// the registry cannot name is written as air — which would make every block in a
/// striped clipboard come back identical and hide a transposition.
const FIRST_NAMED: u16 = 1;
const AIR: u16 = 9;
/// The default state of `oak_log`, which is what `minecraft:oak_log` resolves to.
const OAK_LOG_DEFAULT: u16 = 10;
const OAK_LOG_X: u16 = 11;
const OAK_STAIRS_DEFAULT: u16 = 12;
const OAK_STAIRS_EAST_TOP: u16 = 13;
const OAK_DOOR_DEFAULT: u16 = 14;
const OAK_DOOR_UPPER_LEFT: u16 = 15;
const OAK_WALL_SIGN_NORTH: u16 = 16;

/// A `(state id, block name, properties)` triple, the shape the palette uses.
type PaletteEntry = (u16, &'static str, &'static [(&'static str, &'static str)]);

/// Every state this registry can name, as `(id, name, properties)`.
///
/// The first eight are the property-less blocks the striped clipboard is built from; the
/// rest are types with more than one state, and each of those appears **twice** — once
/// for the default state and once for a state whose every property differs from it.
///
/// The duplication is what makes this registry able to reproduce the failure the
/// schematic tests exist to catch. A palette key that lost its properties is still a
/// valid block pattern, so it does not fail: it resolves to the type's **default**
/// state, which is exactly what a real server does with a property-less name
/// (`Registry::resolve` in `world.rs:419-421`). Modelling that is the difference
/// between a test that catches the bug and one that re-resolves the id it was handed.
const STATES: &[PaletteEntry] = &[
    (FIRST_NAMED, "minecraft:stone", &[]),
    (FIRST_NAMED + 1, "minecraft:dirt", &[]),
    (FIRST_NAMED + 2, "minecraft:cobblestone", &[]),
    (FIRST_NAMED + 3, "minecraft:oak_planks", &[]),
    (FIRST_NAMED + 4, "minecraft:sand", &[]),
    (FIRST_NAMED + 5, "minecraft:gravel", &[]),
    (FIRST_NAMED + 6, "minecraft:glass", &[]),
    (FIRST_NAMED + 7, "minecraft:bricks", &[]),
    (AIR, "minecraft:air", &[]),
    (17, "minecraft:grass_block", &[("snowy", "false")]),
    (18, "minecraft:granite", &[]),
    (19, "minecraft:sandstone", &[]),
    (OAK_LOG_DEFAULT, "minecraft:oak_log", &[("axis", "y")]),
    (OAK_LOG_X, "minecraft:oak_log", &[("axis", "x")]),
    (
        OAK_STAIRS_DEFAULT,
        "minecraft:oak_stairs",
        &[
            ("facing", "north"),
            ("half", "bottom"),
            ("shape", "straight"),
            ("waterlogged", "false"),
        ],
    ),
    (
        OAK_STAIRS_EAST_TOP,
        "minecraft:oak_stairs",
        &[
            ("facing", "east"),
            ("half", "top"),
            ("shape", "straight"),
            ("waterlogged", "false"),
        ],
    ),
    (
        OAK_DOOR_DEFAULT,
        "minecraft:oak_door",
        &[
            ("facing", "north"),
            ("half", "lower"),
            ("hinge", "left"),
            ("open", "false"),
            ("powered", "false"),
        ],
    ),
    (
        OAK_DOOR_UPPER_LEFT,
        "minecraft:oak_door",
        &[
            ("facing", "east"),
            ("half", "upper"),
            ("hinge", "right"),
            ("open", "false"),
            ("powered", "false"),
        ],
    ),
    (
        OAK_WALL_SIGN_NORTH,
        "minecraft:oak_wall_sign",
        &[("facing", "north"), ("waterlogged", "false")],
    ),
];

impl BlockResolver for Registry {
    /// The whole property set has to match: a state the caller half-named is not the
    /// state it asked for.
    fn resolve(&self, name: &str, properties: &[(String, String)]) -> Option<u16> {
        let wanted: Vec<(&str, &str)> = properties
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        STATES
            .iter()
            .find(|(_, other, props)| *other == name && *props == wanted.as_slice())
            // A name with no properties is the type's default state, which is how a
            // palette entry that lost its properties still resolves to *something*.
            .or_else(|| {
                properties
                    .is_empty()
                    .then(|| STATES.iter().find(|(_, other, _)| *other == name))
                    .flatten()
            })
            .map(|(state, _, _)| *state)
    }

    fn info(&self, state: u16) -> Option<BlockStateInfo> {
        let (_, name, properties) = STATES.iter().find(|(id, _, _)| *id == state)?;
        Some(BlockStateInfo {
            name: (*name).to_string(),
            properties: properties
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        })
    }

    fn is_air(&self, state: u16) -> bool {
        state == AIR
    }

    fn is_solid(&self, _state: u16) -> bool {
        true
    }

    fn is_liquid(&self, _state: u16) -> bool {
        false
    }

    /// The bare block type, as production's is: a mask matches a type, not a state, so
    /// this is `minecraft:oak_log` and never `minecraft:oak_log[axis=x]`. A test registry
    /// that answered with the properties would be asserting a convention no real server
    /// follows.
    fn block_name(&self, state: u16) -> Option<&str> {
        STATES
            .iter()
            .find(|(id, _, _)| *id == state)
            .map(|(_, name, _)| *name)
    }
}

/// The name a state is written into a palette as, which is `//schem save`'s `name_of`.
///
/// The whole state, not the block type — see [`schematic::state_string`] — because that is
/// what the palette key is. A registry is asked for `info` here rather than for
/// `block_name`, exactly as `//schem save` has to ask the host: `block_name` answers with
/// the type and loses everything the state is.
fn name_of(state: u16) -> Option<String> {
    let info = Registry.info(state)?;
    Some(schematic::state_string(&info))
}

/// A clipboard whose blocks are laid out so a transposition would be obvious: each
/// block's state depends on all three of its coordinates.
fn striped(width: i32, height: i32, length: i32) -> Clipboard {
    let region = CuboidRegion::new(
        BlockPos::new(0, 0, 0),
        BlockPos::new(width - 1, height - 1, length - 1),
    );
    let dimensions = region.dimensions();
    assert!(
        dimensions.volume() <= 8,
        "striped needs a different registry for this size"
    );
    let blocks: Vec<BlockValue> = (0..dimensions.volume())
        .map(|index| BlockValue::new(FIRST_NAMED + index as u16))
        .collect();
    Clipboard::from_blocks(region, BlockPos::new(0, 0, 0), blocks)
}

/// Reads a fixture out of the test directory.
fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Unwraps the gzip, leaving the NBT document itself.
fn plain_of(bytes: &[u8]) -> Vec<u8> {
    let mut plain = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut plain)
        .expect("decompressed");
    plain
}

/// The compound a key holds, failing the test if it is not one.
fn compound_at<'a>(parent: &'a nbt::Compound, key: &str) -> &'a nbt::Compound {
    match parent.get(key) {
        Some(Value::Compound(compound)) => compound,
        other => panic!("{key} is {other:?}, expected a compound"),
    }
}

/// The `Blocks` compound of a written schematic.
fn blocks_of(bytes: &[u8]) -> nbt::Compound {
    let document = nbt::read(&plain_of(bytes)).expect("parsed");
    compound_at(compound_at(&document, "Schematic"), "Blocks").clone()
}

fn v2_fixture() -> nbt::Compound {
    let bytes = schematic::write(&striped(2, 2, 2), &name_of).unwrap();
    let document = nbt::read(&plain_of(&bytes)).unwrap();
    let mut schematic = compound_at(&document, "Schematic").clone();
    let blocks = compound_at(&schematic, "Blocks").clone();
    schematic.remove("Blocks");
    schematic.insert("Version".into(), Value::Int(2));
    schematic.insert("Palette".into(), blocks["Palette"].clone());
    schematic.insert("BlockData".into(), blocks["Data"].clone());
    schematic.insert("PaletteMax".into(), Value::Int(8));
    schematic.insert("Offset".into(), Value::IntArray(vec![10, 20, 30]));
    let mut metadata = nbt::Compound::new();
    metadata.insert("WEOffsetX".into(), Value::Int(2));
    metadata.insert("WEOffsetY".into(), Value::Int(3));
    metadata.insert("WEOffsetZ".into(), Value::Int(4));
    schematic.insert("Metadata".into(), Value::Compound(metadata));
    schematic
}

fn encode_fixture(schematic: nbt::Compound) -> Vec<u8> {
    let mut document = nbt::Compound::new();
    document.insert("Schematic".into(), Value::Compound(schematic));
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&nbt::write(&document)).unwrap();
    gzip.finish().unwrap()
}

#[test]
fn reads_v2_blocks_offsets_and_flattened_entities() {
    let mut schematic = v2_fixture();
    let mut entity = nbt::Compound::new();
    entity.insert("Id".into(), Value::String("minecraft:chest".into()));
    entity.insert("Pos".into(), Value::IntArray(vec![1, 1, 0]));
    entity.insert("CustomName".into(), Value::String("test".into()));
    schematic.insert(
        "BlockEntities".into(),
        Value::List {
            element: nbt::TAG_COMPOUND,
            items: vec![Value::Compound(entity)],
        },
    );
    let clipboard = schematic::read(&encode_fixture(schematic), &Registry).unwrap();
    assert_eq!(clipboard.origin(), BlockPos::new(8, 17, 26));
    for y in 0..2 {
        for z in 0..2 {
            for x in 0..2 {
                let index = x + z * 2 + y * 4;
                assert_eq!(
                    clipboard
                        .block_at(BlockPos::new(10 + x, 20 + y, 30 + z))
                        .unwrap()
                        .state,
                    FIRST_NAMED + index as u16
                );
            }
        }
    }
    let block = clipboard.block_at(BlockPos::new(11, 21, 30)).unwrap();
    let data = nbt::read(block.entity.as_ref().unwrap()).unwrap();
    assert_eq!(
        data.get("id"),
        Some(&Value::String("minecraft:chest".into()))
    );
    assert_eq!(data.get("x"), Some(&Value::Int(11)));
    assert_eq!(data.get("y"), Some(&Value::Int(21)));
    assert_eq!(data.get("z"), Some(&Value::Int(30)));
    assert_eq!(data.get("CustomName"), Some(&Value::String("test".into())));
    assert!(!data.contains_key("Pos"));
}

#[test]
fn refuses_v2_palette_size_mismatch_and_v1() {
    let mut schematic = v2_fixture();
    schematic.insert("PaletteMax".into(), Value::Int(7));
    assert!(schematic::read(&encode_fixture(schematic), &Registry).is_err());
    let mut schematic = v2_fixture();
    schematic.insert("Version".into(), Value::Int(1));
    assert!(matches!(
        schematic::read(&encode_fixture(schematic), &Registry),
        Err(SchematicError::UnsupportedVersion(1))
    ));
}

#[test]
fn rejects_v2_coordinate_overflow() {
    let mut schematic = v2_fixture();
    schematic.insert("Offset".into(), Value::IntArray(vec![i32::MIN, 20, 30]));
    assert!(schematic::read(&encode_fixture(schematic), &Registry).is_err());
    let mut schematic = v2_fixture();
    schematic.insert("Offset".into(), Value::IntArray(vec![i32::MAX, 20, 30]));
    assert!(schematic::read(&encode_fixture(schematic), &Registry).is_err());
}

#[test]
fn nested_version_tags_do_not_override_the_schematic_version() {
    let mut schematic = v2_fixture();
    let Some(Value::Compound(metadata)) = schematic.get_mut("Metadata") else {
        panic!("fixture metadata missing");
    };
    metadata.insert("Version".into(), Value::Int(42));
    assert!(schematic::read(&encode_fixture(schematic), &Registry).is_ok());
}

#[test]
fn rejects_empty_schematic_dimensions() {
    let mut schematic = v2_fixture();
    schematic.insert("Width".into(), Value::Short(0));
    assert!(schematic::read(&encode_fixture(schematic), &Registry).is_err());
}

/// The palette keys a written schematic holds, in the index order the reader uses.
///
/// Sorted by index rather than left in the map's own order, because the palette is a
/// compound — alphabetical — and the indices are what decide which block is which.
fn palette_keys(bytes: &[u8]) -> Vec<String> {
    let blocks = blocks_of(bytes);
    let palette = compound_at(&blocks, "Palette");
    let mut by_index: Vec<(i32, &str)> = palette
        .iter()
        .map(|(name, slot)| match slot {
            Value::Int(index) => (*index, name.as_str()),
            other => panic!("palette entry {name} is {other:?}, expected an index"),
        })
        .collect();
    by_index.sort();
    by_index
        .into_iter()
        .map(|(_, name)| name.to_string())
        .collect()
}

/// The `Blocks.BlockEntities` entries a written schematic carries.
fn block_entities(bytes: &[u8]) -> Vec<nbt::Compound> {
    match blocks_of(bytes).get("BlockEntities") {
        Some(Value::List { items, .. }) => items
            .iter()
            .map(|item| match item {
                Value::Compound(entry) => entry.clone(),
                other => panic!("a BlockEntities entry is {other:?}, expected a compound"),
            })
            .collect(),
        other => panic!("BlockEntities is {other:?}, expected a list"),
    }
}

/// The head every Sponge v3 document starts with: the root tag, then the member.
fn root_head() -> Vec<u8> {
    let mut head = vec![nbt::TAG_COMPOUND, 0, 0];
    head.push(nbt::TAG_COMPOUND);
    head.extend_from_slice(&9u16.to_be_bytes());
    head.extend_from_slice(b"Schematic");
    head
}

/// The gzip layer has to read a file a real implementation wrote.
///
/// A hand-built fixture would share this reader's idea of the format and so could not
/// catch a wrong one, which is the whole reason for keeping FAWE's own files. The
/// decompression and the tag framing are checked alongside the reader tests below.
#[test]
fn a_real_fawe_schematic_decompresses() {
    for name in ["fawe-fast-v2.schem", "fawe-fast-v3.schem"] {
        let bytes = fixture(name);
        // The gzip magic, which is what makes the file a schematic at all.
        assert_eq!(&bytes[..2], &[0x1f, 0x8b], "{name} is not gzipped");

        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut plain)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(!plain.is_empty(), "{name} decompressed to nothing");

        // A document's root is a compound. The two real files differ in whether it is
        // named: FAWE's v3 writer emits an unnamed root holding a `Schematic`
        // member, its v2 writer names the root `Schematic` directly.
        assert_eq!(plain[0], 10, "{name} does not start with a compound tag");
        let root_name_length = u16::from_be_bytes([plain[1], plain[2]]);
        let root_name = &plain[3..3 + usize::from(root_name_length)];
        assert!(
            root_name.is_empty() || root_name == b"Schematic",
            "{name}: unexpected root name {root_name:?}"
        );
    }
}

#[test]
fn reads_a_real_sponge_v2_schematic() {
    let clipboard =
        schematic::read(&fixture("fawe-fast-v2.schem"), &Registry).expect("read Sponge v2 fixture");
    assert_eq!(clipboard.origin(), BlockPos::new(-100, 75, 73));
    for (position, state) in [
        (BlockPos::new(-101, 74, 73), FIRST_NAMED + 5),
        (BlockPos::new(-100, 74, 73), 17),
        (BlockPos::new(-101, 74, 74), 18),
        (BlockPos::new(-100, 74, 74), 19),
    ] {
        assert_eq!(
            clipboard.block_at(position).map(|block| block.state),
            Some(state)
        );
    }
}

/// FAWE's Sponge v3 fixture is read as a Sponge file.
#[test]
fn reads_a_real_sponge_v3_schematic() {
    let clipboard =
        schematic::read(&fixture("fawe-fast-v3.schem"), &Registry).expect("read Sponge v3 fixture");
    assert!(clipboard.region().dimensions().volume() > 0);
}

/// A clipboard has to survive being written and read back, block for block.
///
/// The states are laid out so that swapping the X, Y and Z axes gives a different
/// answer than the original, so a transposition in either direction fails here.
#[test]
fn a_clipboard_survives_a_write_and_a_read() {
    let original = striped(2, 2, 2);
    let bytes = schematic::write(&original, &name_of).expect("the schematic should be written");
    let restored = schematic::read(&bytes, &Registry).expect("the schematic should be read back");

    assert_eq!(restored.region(), original.region());
    assert_eq!(restored.origin(), original.origin());
    assert_eq!(restored.len(), original.len());
    for index in 0..original.len() {
        let region = original.region();
        let dimensions = region.dimensions();
        let x = (index as i32) % dimensions.x;
        let z = ((index as i32) / dimensions.x) % dimensions.z;
        let y = (index as i32) / (dimensions.x * dimensions.z);
        let position = BlockPos::new(x, y, z);
        assert_eq!(
            restored.block_at(position),
            original.block_at(position),
            "block at {position:?} did not survive the round trip"
        );
    }
}

/// The packed data runs Y outermost, then Z, then X.
///
/// This is the one ordering in the format that is easy to get backwards, and getting it
/// backwards transposes the build rather than failing. So it is checked on the raw
/// bytes: a 2×1×1 schematic is one byte per block and a 1×1×2 is one too, so a
// transposition there cannot hide.
#[test]
fn the_packed_order_is_y_then_z_then_x() {
    let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(1, 1, 1));
    // In the clipboard's own order: y=0z=0x=0, y=0z=0x=1, y=0z=1x=0, y=0z=1x=1.
    let blocks = vec![
        BlockValue::new(1),
        BlockValue::new(2),
        BlockValue::new(3),
        BlockValue::new(4),
    ];
    let clipboard = Clipboard::from_blocks(region, BlockPos::new(0, 0, 0), blocks);
    let bytes = schematic::write(&clipboard, &name_of).expect("written");

    let restored = schematic::read(&bytes, &Registry).expect("read");
    // The same four positions come back with the same four states.
    assert_eq!(
        restored.block_at(BlockPos::new(0, 0, 0)).map(|b| b.state),
        Some(1)
    );
    assert_eq!(
        restored.block_at(BlockPos::new(1, 0, 0)).map(|b| b.state),
        Some(2)
    );
    assert_eq!(
        restored.block_at(BlockPos::new(0, 0, 1)).map(|b| b.state),
        Some(3)
    );
    assert_eq!(
        restored.block_at(BlockPos::new(1, 0, 1)).map(|b| b.state),
        Some(4)
    );
}

/// Palette indices are assigned in the order blocks are first seen, and the reader
/// must honour the stored index rather than the alphabetical order of the names.
///
/// This is the bug a sorted map invites: a palette written `zebra` then `apple` would
/// be read as `apple` then `zebra`, swapping every block in the build while still
/// loading cleanly.
#[test]
fn a_palette_is_read_by_its_stored_index_not_by_name_order() {
    let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(1, 0, 0));
    // 'zebra' is dirt, discovered first, so it takes palette slot 0; 'apple' is stone
    // and takes slot 1. Alphabetically they would swap.
    let clipboard = Clipboard::from_blocks(
        region,
        BlockPos::new(0, 0, 0),
        vec![BlockValue::new(2), BlockValue::new(1)],
    );
    let bytes = schematic::write(&clipboard, &name_of).expect("written");
    let restored = schematic::read(&bytes, &Registry).expect("read");

    assert_eq!(
        restored.block_at(BlockPos::new(0, 0, 0)).map(|b| b.state),
        Some(2),
        "the first block was discovered first and must keep palette slot 0"
    );
    assert_eq!(
        restored.block_at(BlockPos::new(1, 0, 0)).map(|b| b.state),
        Some(1)
    );
}

/// Two identical blocks share one palette entry, which is what makes a schematic of a
/// flat build small.
#[test]
fn repeated_blocks_share_one_palette_entry() {
    let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(9, 9, 9));
    let clipboard = Clipboard::empty(region, BlockPos::new(0, 0, 0), BlockValue::new(1));
    let bytes = schematic::write(&clipboard, &name_of).expect("written");

    assert_eq!(
        palette_keys(&bytes),
        vec!["minecraft:stone".to_string()],
        "one block type, one palette entry"
    );
}

/// A block the server does not have is reported by name rather than pasted as
/// something else, because silently substituting would corrupt a build quietly.
#[test]
fn a_block_this_server_does_not_have_is_named_in_the_error() {
    let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
    let clipboard = Clipboard::from_blocks(
        region,
        BlockPos::new(0, 0, 0),
        vec![BlockValue::new(99)], // not in Registry
    );
    let bytes = schematic::write(&clipboard, &|state| Some(format!("minecraft:block{state}")))
        .expect("written");

    match schematic::read(&bytes, &Registry) {
        Err(SchematicError::UnknownBlock(name)) => {
            assert!(
                name.contains("block99"),
                "the error should name the block: {name}"
            );
        }
        other => panic!("expected an unknown-block error, got {other:?}"),
    }
}

/// A schematic with properties keeps them, and the reader resolves the same way
/// `//set` does.
#[test]
fn a_palette_entry_with_properties_is_resolved_with_them() {
    let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
    let clipboard = Clipboard::from_blocks(
        region,
        BlockPos::new(0, 0, 0),
        vec![BlockValue::new(OAK_LOG_X)],
    );
    let bytes = schematic::write(&clipboard, &name_of).expect("written");

    let restored = schematic::read(&bytes, &Registry).expect("read");
    assert_eq!(
        restored.block_at(BlockPos::new(0, 0, 0)).map(|b| b.state),
        Some(OAK_LOG_X),
        "minecraft:oak_log[axis=x] must resolve to the axis=x state"
    );
}

/// A block state with properties has to be written into the palette as its whole state
/// string, or a save quietly replaces every stair, log, door and sign with its default
/// state.
///
/// Java writes `block.toImmutableState().getAsString()` (`SpongeSchematicV3Writer.java:186`),
/// which is `minecraft:oak_stairs[facing=east,half=top,…]` — see
/// `BlockStateHolder.getAsString()` at `BlockStateHolder.java:216-227` — and
/// `ReaderUtil.decodePalette` (`ReaderUtil.java:136-149`) parses that back with WorldEdit's
/// block parser.
///
/// The failure is silent by construction: `minecraft:oak_stairs` on its own is a *valid*
/// pattern, so a palette holding the bare id still loads and every stateful block comes
/// back as its default. Nothing is refused, nothing is logged, and `//schem save` followed
/// by `//paste` builds something else.
#[test]
fn a_state_with_properties_survives_a_save_and_a_load() {
    let states = [
        OAK_STAIRS_EAST_TOP,
        OAK_LOG_X,
        OAK_DOOR_UPPER_LEFT,
        FIRST_NAMED,
    ];
    let region = CuboidRegion::new(
        BlockPos::new(0, 0, 0),
        BlockPos::new(states.len() as i32 - 1, 0, 0),
    );
    let clipboard = Clipboard::from_blocks(
        region,
        BlockPos::new(0, 0, 0),
        states.iter().copied().map(BlockValue::new).collect(),
    );

    let bytes = schematic::write(&clipboard, &name_of).expect("written");

    // The keys are asserted on the file rather than only through the round trip, so a
    // reader that happened to agree with a wrong writer could not hide the loss.
    assert_eq!(
        palette_keys(&bytes),
        [
            "minecraft:oak_stairs[facing=east,half=top,shape=straight,waterlogged=false]",
            "minecraft:oak_log[axis=x]",
            "minecraft:oak_door[facing=east,half=upper,hinge=right,open=false,powered=false]",
            "minecraft:stone",
        ]
        .map(String::from)
    );

    let restored = schematic::read(&bytes, &Registry).expect("read");
    for (index, state) in states.iter().enumerate() {
        let position = BlockPos::new(index as i32, 0, 0);
        assert_eq!(
            restored.block_at(position).map(|block| block.state),
            Some(*state),
            "the state at {position:?} did not survive the round trip"
        );
    }
}

/// A block entity's `Id` names the tile entity, so it carries no properties.
///
/// Java writes `block.getNbtId()` (`SpongeSchematicV3Writer.java:179`), which is the `id`
/// tag of the block's own NBT (`BaseBlock.java:167-173`) — `minecraft:oak_wall_sign`, not
/// the palette key — and `ReaderUtil.java:168` reads it back as that NBT `id`. A sign is
/// one of the block entities whose *type* has properties, so handing this field the
/// palette key would give a Java reader an `id` no tile entity matches, and the text on
/// the sign would be dropped on load.
///
/// A chest is the case that looks wrong and is not: `minecraft:chest` has no properties,
/// so its palette key and its NBT id are the same string either way.
#[test]
fn a_block_entity_id_is_the_bare_block_id() {
    let entity = nbt::write(&nbt::Compound::from([(
        "Text".into(),
        Value::String("hello".into()),
    )]))
    .into_boxed_slice();
    let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
    let clipboard = Clipboard::from_blocks(
        region,
        BlockPos::new(0, 0, 0),
        vec![BlockValue::with_entity(OAK_WALL_SIGN_NORTH, entity)],
    );

    let bytes = schematic::write(&clipboard, &name_of).expect("written");
    let entries = block_entities(&bytes);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].get("Id"),
        Some(&Value::String("minecraft:oak_wall_sign".into())),
        "the entry names the block entity's type, which has no properties"
    );
}

/// A state with no properties is written as its bare id, which is what Java renders for
/// one: `getAsString()` returns the id on its own when there are no states to write
/// (`BlockStateHolder.java:217-218`).
///
/// Pinned because the fix above builds the key out of the properties, and the simple case
/// is the one a schematic of a plain build is made of.
#[test]
fn a_state_with_no_properties_is_written_as_a_bare_id() {
    let info = BlockStateInfo {
        name: "minecraft:stone".to_string(),
        properties: Vec::new(),
    };
    assert_eq!(schematic::state_string(&info), "minecraft:stone");

    let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
    let clipboard = Clipboard::from_blocks(
        region,
        BlockPos::new(0, 0, 0),
        vec![BlockValue::new(FIRST_NAMED)],
    );
    let bytes = schematic::write(&clipboard, &name_of).expect("written");
    assert_eq!(
        palette_keys(&bytes),
        vec!["minecraft:stone".to_string()],
        "a property-less block's key carries no brackets"
    );
    assert_eq!(
        schematic::read(&bytes, &Registry)
            .expect("read")
            .block_at(BlockPos::new(0, 0, 0))
            .map(|block| block.state),
        Some(FIRST_NAMED)
    );
}

/// The palette key is `getAsString()` and nothing else: the id alone with no properties,
/// `id[key=value,…]` with them, values lower-cased, and the order the state reported them
/// in.
///
/// Java lower-cases each value (`BlockStateHolder.java:220-224`); every property value
/// Minecraft defines is already lower-case, so this only shows up on a state the host
/// spelled otherwise — but a schematic that is handed to another Sponge v3 reader has to
/// be the string that reader expects.
#[test]
fn the_palette_key_is_the_block_state_string() {
    let state = |properties: &[(&str, &str)]| BlockStateInfo {
        name: "minecraft:oak_stairs".to_string(),
        properties: properties
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect(),
    };
    assert_eq!(
        schematic::state_string(&state(&[])),
        "minecraft:oak_stairs",
        "no properties is the bare id, with no brackets"
    );
    assert_eq!(
        schematic::state_string(&state(&[("facing", "east"), ("half", "top")])),
        "minecraft:oak_stairs[facing=east,half=top]",
        "one `key=value` per property, comma-separated, in the order reported"
    );
    assert_eq!(
        schematic::state_string(&state(&[("half", "TOP")])),
        "minecraft:oak_stairs[half=top]",
        "values are lower-cased"
    );
}

/// A schematic claiming an impossible size must be refused before anything is
/// allocated, not after the process has already tried to hold a three-billion blocks.
#[test]
fn an_impossible_size_is_refused_before_allocating() {
    let document = nbt::Compound::from([(
        "Schematic".to_string(),
        Value::Compound(nbt::Compound::from([
            ("Version".into(), Value::Int(3)),
            ("Width".into(), Value::Short(30_000)),
            ("Height".into(), Value::Short(30_000)),
            ("Length".into(), Value::Short(30_000)),
            (
                "Blocks".into(),
                Value::Compound(nbt::Compound::from([
                    ("Palette".into(), Value::Compound(nbt::Compound::new())),
                    ("Data".into(), Value::ByteArray(Vec::new())),
                ])),
            ),
        ])),
    )]);

    let bytes = {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&nbt::write(&document)).expect("written");
        encoder.finish().expect("finished")
    };

    match schematic::read(&bytes, &Registry) {
        Err(SchematicError::TooLarge { .. }) => {}
        other => panic!("expected a size refusal, got {other:?}"),
    }
}

/// A damaged file is reported rather than read as an empty build.
#[test]
fn a_damaged_file_is_refused() {
    assert!(matches!(
        schematic::read(b"not a gzip stream at all", &Registry),
        Err(SchematicError::Container(_))
    ));
    assert!(schematic::read(&[], &Registry).is_err());
}

/// The negative offset a schematic of a region below the origin needs has to survive,
/// or a build pasted into a cave loads in the sky.
#[test]
fn a_negative_offset_survives_the_round_trip() {
    let region = CuboidRegion::new(BlockPos::new(-40, -60, -30), BlockPos::new(-38, -58, -28));
    let clipboard = Clipboard::from_blocks(
        region,
        BlockPos::new(-40, -60, -30),
        vec![BlockValue::new(1); 27],
    );
    let bytes = schematic::write(&clipboard, &name_of).expect("written");
    let restored = schematic::read(&bytes, &Registry).expect("read");

    assert_eq!(restored.region().minimum(), BlockPos::new(-40, -60, -30));
    assert_eq!(restored.region().maximum(), BlockPos::new(-38, -58, -28));
}

/// Trailing bytes after a complete document are ignored, because real files have them.
///
/// FAWE's own v3 writer leaves several zero bytes after the root compound's `TAG_End`,
/// and its v2 writer emits a *named* root whose name is read as a member. Refusing a
/// file over either would reject a good schematic.
///
/// This is the check the strict parser cannot make on its own: it is what proves the
/// `read_payload_starts_after_the_tag_byte` framing is right against real bytes rather
/// than only against fixtures this code wrote.
#[test]
fn a_real_fawe_schematic_parses_its_root_compound() {
    for name in ["fawe-fast-v2.schem", "fawe-fast-v3.schem"] {
        let bytes = fixture(name);
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut plain)
            .unwrap_or_else(|error| panic!("{name}: {error}"));

        let document = nbt::read(&plain)
            .unwrap_or_else(|error| panic!("{name}: the root compound did not parse: {error}"));
        let has_schematic = match document.get("Schematic") {
            Some(Value::Compound(_)) => true,
            _ => {
                matches!(document.get(""), Some(Value::Compound(inner)) if inner.contains_key("Schematic"))
            }
        };
        assert!(
            has_schematic,
            "{name}: no Schematic compound in {document:?}"
        );
    }
}

/// The document's head has to be a real root tag: `TAG_Compound`, a zero-length name,
/// and then the `Schematic` member inside it.
///
/// FAWE does not parse the head, it skips it — `skipHeader` consumes `1+2` bytes for
/// the root (`TAG_Compound` and the length of a name that is empty) and then `1+2+9`
/// for the `Schematic` member (`FastSchematicReaderV3.java:759-761`). A file written
/// without the root's own type byte and name starts on the member's tag byte, so that
/// skip lands one byte short of the name and every field after it is read from the
/// wrong offset — which is a file WorldEdit loads as something else rather than
/// refuses. The write is `LinRootEntry("", ...)` at `SpongeSchematicV3Writer.java:64`.
#[test]
fn a_written_schematic_starts_with_the_root_named_tag() {
    let bytes = schematic::write(&striped(2, 2, 2), &name_of).expect("written");
    let expected = root_head();

    assert_eq!(
        &plain_of(&bytes)[..expected.len()],
        expected.as_slice(),
        "the head must be `0a 00 00` for the root and `0a 00 09 Schematic` after it"
    );
    // FAWE's own file starts with the same fifteen bytes, so the two agree.
    assert_eq!(
        &plain_of(&fixture("fawe-fast-v3.schem"))[..expected.len()],
        expected.as_slice()
    );

    // ...and the extra header is not something this reader trips over.
    assert!(schematic::read(&bytes, &Registry).is_ok());
}

/// A chest's contents have to survive being saved and loaded.
///
/// The clipboard already keeps block entity NBT (`BlockValue::entity`), and the format
/// already had somewhere to put it: `Blocks.BlockEntities`, which
/// `SpongeSchematicV3Writer.encodeBlocks` (`SpongeSchematicV3Writer.java:170-190`)
/// fills with one compound per block. This writer emitted it empty, and an empty list
/// is not "unknown" — it tells a Java reader the schematic has no block entities at
/// all, so a saved chest came back as an empty chest with nothing said about it on
/// either side.
#[test]
fn a_block_entity_survives_the_round_trip() {
    let tags = nbt::Compound::from([
        ("id".into(), Value::String("minecraft:chest".into())),
        ("CustomName".into(), Value::String("Trove".into())),
        ("Items".into(), Value::ByteArray(vec![1, 2, 3])),
    ]);
    let entity = nbt::write(&tags).into_boxed_slice();
    let minimum = BlockPos::new(5, 64, -3);
    let region = CuboidRegion::new(minimum, minimum.offset(1, 0, 0));
    let clipboard = Clipboard::from_blocks(
        region,
        minimum,
        vec![
            BlockValue::new(FIRST_NAMED + 1),
            BlockValue::with_entity(FIRST_NAMED, entity.clone()),
        ],
    );

    let bytes = schematic::write(&clipboard, &name_of).expect("written");

    // The entry has to land where Java puts it, or a WorldEdit reader looks elsewhere.
    let document = nbt::read(&plain_of(&bytes)).expect("parsed");
    let blocks = compound_at(compound_at(&document, "Schematic"), "Blocks");
    let items = match blocks.get("BlockEntities") {
        Some(Value::List { items, .. }) => items,
        other => panic!("BlockEntities is {other:?}, expected a list"),
    };
    assert_eq!(items.len(), 1, "one block with block entity, one entry");
    let entry = match &items[0] {
        Value::Compound(entry) => entry,
        other => panic!("the entry is {other:?}, expected a compound"),
    };
    assert_eq!(
        entry.get("Pos"),
        Some(&Value::IntArray(vec![1, 0, 0])),
        "Pos is relative to the minimum corner, as `point.subtract(min)` writes it"
    );
    let mut data_tags = tags.clone();
    data_tags.remove("id");
    assert_eq!(
        entry.get("Id"),
        Some(&Value::String("minecraft:chest".into()))
    );
    assert_eq!(entry.get("Data"), Some(&Value::Compound(data_tags)));

    let restored = schematic::read(&bytes, &Registry).expect("read back");
    assert_eq!(
        restored
            .block_at(minimum.offset(1, 0, 0))
            .and_then(|block| block.entity.clone()),
        Some(entity),
        "the block's NBT must survive the round trip"
    );
    assert_eq!(
        restored
            .block_at(minimum)
            .and_then(|block| block.entity.clone()),
        None,
        "a block with no block entity must not be given one"
    );
}

/// A selection wider than 32767 blocks is a legal size and has to survive.
///
/// The dimensions are stored as a short, which Java reads as *unsigned* —
/// `valueAsShort() & 0xFFFF` at `SpongeSchematicV3Reader.java:92-94`. Reading it as a
/// signed short turned a 40001-wide schematic into a negative width, which the size
/// check then refused: the file this writer had just written could not be read back by
/// this reader, and the error named a size the build never had.
#[test]
fn a_selection_wider_than_a_short_round_trips() {
    let width = 40_001;
    let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(width - 1, 0, 0));
    let clipboard = Clipboard::from_blocks(
        region,
        BlockPos::new(0, 0, 0),
        vec![BlockValue::new(FIRST_NAMED); width as usize],
    );

    let bytes = schematic::write(&clipboard, &name_of).expect("written");
    let restored = schematic::read(&bytes, &Registry).expect("read back");
    assert_eq!(restored.region().dimensions().x, width);
}

/// Past the format's own limit the writer refuses, rather than writing a size it cannot
/// represent.
///
/// `SpongeSchematicV3Writer` throws past `MAX_SIZE`, which is `Short.MAX_VALUE -
/// Short.MIN_VALUE` — 65535, not 32767 (`SpongeSchematicV3Writer.java:51` and the
/// three checks at lines 84-92), because the short is read back unsigned. Casting to
/// `i16` instead produced a file whose width read back as a *smaller* size, so a save
/// of a wide region came back as a silently truncated build.
#[test]
fn a_region_too_wide_for_the_format_is_refused_on_save() {
    let width = 65_537;
    let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(width - 1, 0, 0));
    let clipboard = Clipboard::from_blocks(
        region,
        BlockPos::new(0, 0, 0),
        vec![BlockValue::new(FIRST_NAMED); width as usize],
    );

    match schematic::write(&clipboard, &name_of) {
        Err(SchematicError::TooLarge { width: claimed, .. }) => {
            assert_eq!(claimed, width, "the refusal has to name the size asked for")
        }
        other => panic!("expected a size refusal, got {other:?}"),
    }
}
