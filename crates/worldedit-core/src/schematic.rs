//! Reading Sponge schematic v2 and v3, and writing v3.
//!
//! ## What this reads
//!
//! The Sponge Schematic Specification version 3, which is the format WorldEdit itself
//! writes through `//schem save` and which every other schematic tool understands. A
//! file is gzip-wrapped NBT with a `Schematic` compound holding the dimensions, the
//! blocks and the biomes.
//!
//! The block data is a palette: a `Palette` compound mapping generated names to block
//! ids, and a `Data` byte array holding one variable-length integer per position. The
//! integers are packed seven bits at a time, seven bits per byte with the top bit set on
//! every byte but the last, which is what `SpongeSchematicV3Writer.encodeBlocks` emits.
//!
//! A palette's key is a whole **block state**, not a block type: `BlockStateHolder.getAsString()`
//! (`BlockStateHolder.java:216-227`) spells `minecraft:oak_stairs[facing=east,half=top,shape=straight,waterlogged=false]`
//! and that is what goes in, read back by `ReaderUtil.decodePalette`
//! (`ReaderUtil.java:136-149`). The properties are what make it a round trip rather than a
//! lossy one — a bare type is a valid pattern that resolves to that type's *default*
//! state — so [`state_string`] is the spelling to write and [`write`] takes the naming
//! it needs from its caller.
//!
//! The order of the packed data is `y`, then `z`, then `x` — slowest axis first — and
//! not the `x + y*w + z*w*h` the clipboard's own storage uses. Getting it wrong does
//! not fail, it transposes the build, so [`decode_blocks`] has a test that would catch
//! it.
//!
//! A block that carries NBT of its own — a chest, a sign, a banner — is listed once more
//! in `Blocks.BlockEntities`, at a position relative to the selection's minimum corner.
//! An empty list there is a positive statement that there are none, so a schematic which
//! has them and writes an empty one loses them in both directions.
//!
//! ## Version 2
//!
//! Sponge version 2 keeps the same palette and the same seven-bit packing, but stores
//! them on the `Schematic` compound itself rather than under `Blocks`: `Palette`,
//! `BlockData` and `BlockEntities` are top-level members. A block entity's own NBT is
//! flattened into its `BlockEntities` entry next to `Id` and `Pos` instead of being
//! nested under `Data`. Its `Pos` is relative to the selection's minimum corner;
//! the reader restores absolute coordinates in the block entity data.
//!
//! The origin is a different pair of tags as well. Version 2 writes
//! `Metadata.WEOffsetX/Y/Z` and takes the clipboard origin as the minimum corner minus
//! that offset (`SpongeSchematicV1Reader.readVersion1`), where version 3 writes
//! `Metadata.WorldEdit.Origin` and the minimum corner is `Offset` plus it.
//!
//! ## What this does not read
//!
//! FAWE-specific compressed formats are not supported. A version number alone does not
//! distinguish every encoding, so unsupported files cannot always be identified reliably.
//!
//! Version 1 is not supported.

use std::collections::BTreeMap;
use std::io::{Read, Write};

use flate2::{Compression, read::GzDecoder, write::GzEncoder};

use crate::{
    block::{BlockResolver, BlockStateInfo, BlockValue},
    clipboard::Clipboard,
    math::BlockPos,
    nbt::{self, Compound, Value},
    region::CuboidRegion,
};

/// The version this reads and writes.
pub const VERSION: i32 = 3;

/// The minimum version this reads.
pub const MIN_VERSION: i32 = 2;

/// The data version written into new schematics.
///
/// Minecraft 1.21 is data version 3953. A reader is meant to use this to decide whether
/// a block id needs fixing up for the live server's version; this plugin has no such
/// fixer, so it is written as a constant and only a *reader* would act on it.
const DATA_VERSION: i32 = 3953;

/// Why a schematic could not be read.
#[derive(Debug, Clone, PartialEq)]
pub enum SchematicError {
    /// The gzip or NBT envelope was damaged.
    Container(String),
    /// The file is a schematic version this does not read.
    UnsupportedVersion(i32),
    /// A required tag was missing or the wrong type.
    Missing(&'static str),
    /// The block or biome palette could not be used.
    Palette(String),
    /// A block in the palette is not one the server has.
    UnknownBlock(String),
    /// The block data is invalid or uses an unsupported encoding.
    LikelyFaweFastFormat,
    /// The schematic is bigger than this will allocate.
    TooLarge {
        /// The size it claimed.
        width: i32,
        /// The size it claimed.
        height: i32,
        /// The size it claimed.
        length: i32,
    },
}

impl std::fmt::Display for SchematicError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Container(why) => write!(formatter, "the schematic is damaged: {why}"),
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "this is a version {version} schematic; only versions {MIN_VERSION} and {VERSION} are supported"
            ),
            Self::Missing(what) => write!(formatter, "the schematic has no {what}"),
            Self::Palette(why) => write!(formatter, "the schematic's palette is unusable: {why}"),
            Self::UnknownBlock(name) => write!(
                formatter,
                "the schematic uses '{name}', which this server does not have"
            ),
            Self::LikelyFaweFastFormat => write!(
                formatter,
                "the schematic block data is invalid or uses an unsupported encoding; \
                 re-save it as Sponge v2 or v3 and try again"
            ),
            Self::TooLarge {
                width,
                height,
                length,
            } => write!(
                formatter,
                "the schematic claims to be {width}x{height}x{length}, which is too large to load"
            ),
        }
    }
}

impl std::error::Error for SchematicError {}

/// Turns an NBT failure into a schematic failure.
fn container(why: impl std::fmt::Display) -> SchematicError {
    SchematicError::Container(why.to_string())
}

/// Unwraps the gzip and reads the NBT document inside.
fn read_envelope(bytes: &[u8]) -> Result<Compound, SchematicError> {
    let mut plain = Vec::new();
    GzDecoder::new(bytes)
        .read_to_end(&mut plain)
        .map_err(|error| container(format!("gzip: {error}")))?;
    nbt::read(&plain).map_err(container)
}

/// Wraps an NBT document in gzip.
fn write_envelope(document: &Compound) -> Result<Vec<u8>, SchematicError> {
    let plain = nbt::write(document);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&plain)
        .map_err(|error| container(format!("gzip: {error}")))?;
    encoder
        .finish()
        .map_err(|error| container(format!("gzip: {error}")))
}

/// The `Schematic` compound a version 3 file is made of.
///
/// Both root shapes in the wild arrive here as a `Schematic` member. `nbt::read`
/// normalises them: FAWE's v3 writer emits an *unnamed* root compound holding a
/// `Schematic` member (`LinRootEntry("", ...)`, `SpongeSchematicV3Writer.java:64`),
/// whose members are the document, and its v2 writer names the root `Schematic`
/// directly, with no member at all, which Java also seeks to by name
/// (`BuiltInClipboardFormat.detectOldSpongeSchematic:492`). Only a `Schematic` member
/// is looked for: taking whichever compound came first would make a file whose
/// `Schematic` tag is missing parse as some other tag's contents, which is a worse
/// failure than saying the tag is missing.
fn schematic_tag(document: &Compound) -> Result<&Compound, SchematicError> {
    match document.get("Schematic") {
        Some(Value::Compound(schematic)) => Ok(schematic),
        _ => Err(SchematicError::Missing("a Schematic compound")),
    }
}

/// Reads a schematic into a clipboard.
///
/// `resolver` turns the palette's block ids into the server's state ids, so a schematic
/// naming a block this server does not have is reported by name rather than pasted as
/// something else.
pub fn read(bytes: &[u8], resolver: &dyn BlockResolver) -> Result<Clipboard, SchematicError> {
    let document = read_envelope(bytes)?;
    let schematic = schematic_tag(&document)?;
    let version = version_of(schematic)?;

    let dimensions = Dimensions::read(schematic)?;
    dimensions.check_size()?;

    // The minimum corner is where the build starts, the origin is what `//paste` anchors
    // it by. The two versions spell both differently, so they are read together.
    let (minimum, origin) = geometry(schematic, version)?;
    let maximum = BlockPos::new(
        minimum
            .x
            .checked_add(dimensions.width - 1)
            .ok_or_else(|| container("schematic coordinates overflow"))?,
        minimum
            .y
            .checked_add(dimensions.height - 1)
            .ok_or_else(|| container("schematic coordinates overflow"))?,
        minimum
            .z
            .checked_add(dimensions.length - 1)
            .ok_or_else(|| container("schematic coordinates overflow"))?,
    );
    let region = CuboidRegion::new(minimum, maximum);

    let blocks = decode_blocks(
        &block_section(schematic, version)?,
        dimensions,
        minimum,
        resolver,
    )?;

    // Already in the clipboard's own YZX order: position was derived from the same
    // axis order the data was walked in, so no transposition happens here.
    Ok(Clipboard::from_blocks(region, origin, blocks))
}

/// The schematic's declared version, which decides where every other tag lives.
fn version_of(schematic: &Compound) -> Result<i32, SchematicError> {
    let version = match schematic.get("Version") {
        Some(Value::Int(version)) => *version,
        Some(_) => return Err(SchematicError::Missing("a numeric Version tag")),
        None => return Err(SchematicError::Missing("a Version tag")),
    };
    if !(MIN_VERSION..=VERSION).contains(&version) {
        return Err(SchematicError::UnsupportedVersion(version));
    }
    Ok(version)
}

/// A schematic's three dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    /// Along X.
    pub width: i32,
    /// Along Y.
    pub height: i32,
    /// Along Z.
    pub length: i32,
}

impl Dimensions {
    /// Reads the three short tags, which the format stores as signed 16-bit.
    fn read(schematic: &Compound) -> Result<Self, SchematicError> {
        Ok(Self {
            width: read_dimension(schematic, "Width")?,
            height: read_dimension(schematic, "Height")?,
            length: read_dimension(schematic, "Length")?,
        })
    }

    /// How many blocks the schematic claims to hold.
    #[must_use]
    pub fn volume(self) -> usize {
        (self.width.max(0) as usize) * (self.height.max(0) as usize) * (self.length.max(0) as usize)
    }

    /// Refuses a schematic whose declared size cannot be allocated, or cannot be named.
    ///
    /// The check is on the declared size, before anything is read, because a hostile or
    /// corrupt header claiming a three-billion-block cube would otherwise try to
    /// allocate it.
    fn check_size(self) -> Result<(), SchematicError> {
        const LIMIT: usize = 1 << 30; // a gigabyte of blocks
        // Each axis is a short, so 65535 is all one can hold — the same `MAX_SIZE` of
        // `Short.MAX_VALUE - Short.MIN_VALUE` that `SpongeSchematicV3Writer.java:51`
        // refuses to go past, at lines 84-92. Casting a larger axis to `i16` instead
        // wrote a *smaller* size than the region really is, which came back as a
        // truncated build rather than as a save that failed.
        const MAX_AXIS: i32 = u16::MAX as i32;
        let too_wide = self.width > MAX_AXIS || self.height > MAX_AXIS || self.length > MAX_AXIS;
        if too_wide || self.width <= 0 || self.height <= 0 || self.length <= 0 {
            return Err(SchematicError::TooLarge {
                width: self.width,
                height: self.height,
                length: self.length,
            });
        }
        let volume = self.width as u64 * self.height as u64 * self.length as u64;
        if volume > LIMIT as u64 {
            return Err(SchematicError::TooLarge {
                width: self.width,
                height: self.height,
                length: self.length,
            });
        }
        Ok(())
    }

    /// The position of the `index`th block in the packed data.
    ///
    /// Y outermost, then Z, then X — the order `encodePalettedData` writes them in. Not
    /// the clipboard's own `x + y*w + z*w*h` order, and not the usual X-fastest order
    /// either.
    pub fn position(self, index: usize, minimum: BlockPos) -> BlockPos {
        let width = self.width as usize;
        let length = self.length as usize;
        let x = index % width;
        let z = (index / width) % length;
        let y = index / (width * length);
        BlockPos::new(
            minimum.x + x as i32,
            minimum.y + y as i32,
            minimum.z + z as i32,
        )
    }
}

/// Reads one of the three dimension tags.
fn read_dimension(schematic: &Compound, name: &'static str) -> Result<i32, SchematicError> {
    // The format stores a short, but a file written by a tool that used an int has to
    // be readable, so both widths are accepted rather than only the spec's.
    //
    // The short is read as *unsigned*, which is what Java does —
    // `valueAsShort() & 0xFFFF` at `SpongeSchematicV3Reader.java:92-94`. A signed read
    // turns any dimension above 32767 into a negative number, so a schematic of a wide
    // selection was refused as `TooLarge` with a size the build never had.
    match schematic.get(name) {
        Some(Value::Short(value)) => Ok((*value as u32 & 0xFFFF) as i32),
        Some(Value::Int(value)) => Ok(*value),
        _ => Err(SchematicError::Missing(name)),
    }
}

/// The schematic's minimum corner and the clipboard origin `//paste` anchors by.
fn geometry(schematic: &Compound, version: i32) -> Result<(BlockPos, BlockPos), SchematicError> {
    let offset = int_array_at(schematic, "Offset").unwrap_or(BlockPos::new(0, 0, 0));
    if version == 2 {
        // `SpongeSchematicV1Reader.readVersion1`: the minimum corner is `Offset`, and the
        // origin is that corner moved back by `Metadata.WEOffset…`. Reading the version 3
        // tags here would anchor every version 2 paste at the wrong corner.
        let relative = world_edit_offset(schematic)?;
        return Ok((
            offset,
            BlockPos::new(
                offset
                    .x
                    .checked_sub(relative.x)
                    .ok_or_else(|| container("schematic coordinates overflow"))?,
                offset
                    .y
                    .checked_sub(relative.y)
                    .ok_or_else(|| container("schematic coordinates overflow"))?,
                offset
                    .z
                    .checked_sub(relative.z)
                    .ok_or_else(|| container("schematic coordinates overflow"))?,
            ),
        ));
    }
    // The origin is WorldEdit's own addition under Metadata, and its absence is normal:
    // a file from another tool has no origin, which means the clipboard is anchored at
    // its own minimum corner.
    let origin = compound_at(schematic, "Metadata")
        .and_then(|metadata| compound_at(metadata, "WorldEdit"))
        .and_then(|world_edit| int_array_at(world_edit, "Origin"))
        .unwrap_or(BlockPos::new(0, 0, 0));
    // Version 3 stores `Offset` relative to the origin, so the corner is the two added.
    Ok((
        BlockPos::new(
            offset
                .x
                .checked_add(origin.x)
                .ok_or_else(|| container("schematic coordinates overflow"))?,
            offset
                .y
                .checked_add(origin.y)
                .ok_or_else(|| container("schematic coordinates overflow"))?,
            offset
                .z
                .checked_add(origin.z)
                .ok_or_else(|| container("schematic coordinates overflow"))?,
        ),
        origin,
    ))
}

/// The `Metadata.WEOffsetX/Y/Z` triple a version 2 file moves its origin by.
///
/// Java reads `X` first and then demands `Y` and `Z`, so a file with only one of them is
/// refused rather than silently anchored at a corner it did not declare.
fn world_edit_offset(schematic: &Compound) -> Result<BlockPos, SchematicError> {
    let metadata = compound_at(schematic, "Metadata");
    let Some(x) = metadata.and_then(|metadata| int_at(metadata, "WEOffsetX")) else {
        return Ok(BlockPos::new(0, 0, 0));
    };
    let y = metadata.and_then(|metadata| int_at(metadata, "WEOffsetY"));
    let z = metadata.and_then(|metadata| int_at(metadata, "WEOffsetZ"));
    match (y, z) {
        (Some(y), Some(z)) => Ok(BlockPos::new(x, y, z)),
        (None, _) => Err(SchematicError::Missing("a Metadata.WEOffsetY tag")),
        (_, None) => Err(SchematicError::Missing("a Metadata.WEOffsetZ tag")),
    }
}

/// The int a key holds, or `None` if it is absent or another type.
fn int_at(parent: &Compound, key: &str) -> Option<i32> {
    match parent.get(key) {
        Some(Value::Int(value)) => Some(*value),
        _ => None,
    }
}

/// The compound a key holds, or `None` if it is absent or not a compound.
fn compound_at<'a>(parent: &'a Compound, key: &str) -> Option<&'a Compound> {
    match parent.get(key) {
        Some(Value::Compound(compound)) => Some(compound),
        _ => None,
    }
}

/// The three-element int array a key holds, or `None` if it is absent, of another
/// type, or too short to be a position.
fn int_array_at(parent: &Compound, key: &str) -> Option<BlockPos> {
    match parent.get(key) {
        Some(Value::IntArray(values)) if values.len() >= 3 => {
            Some(BlockPos::new(values[0], values[1], values[2]))
        }
        _ => None,
    }
}

struct BlockSection<'a> {
    compound: &'a Compound,
    version: i32,
}

fn block_section(schematic: &Compound, version: i32) -> Result<BlockSection<'_>, SchematicError> {
    let compound = if version == 2 {
        schematic
    } else {
        compound_at(schematic, "Blocks").ok_or(SchematicError::Missing("a Blocks compound"))?
    };
    Ok(BlockSection { compound, version })
}

/// Turns a block section into one block per position.
///
/// `palette` maps an index to a state id and is filled as the data is walked, because
/// the first appearance of an id is what defines it.
fn decode_blocks(
    section: &BlockSection<'_>,
    dimensions: Dimensions,
    minimum: BlockPos,
    resolver: &dyn BlockResolver,
) -> Result<Vec<BlockValue>, SchematicError> {
    let blocks = section.compound;
    let palette = match blocks.get("Palette") {
        Some(Value::Compound(palette)) => palette,
        _ => return Err(SchematicError::Missing("a Blocks.Palette compound")),
    };
    if section.version == 2
        && let Some(Value::Int(max)) = blocks.get("PaletteMax")
        && (*max < 0 || *max as usize != palette.len())
    {
        return Err(SchematicError::Palette(
            "palette size does not match PaletteMax".into(),
        ));
    }
    let data_key = if section.version == 2 {
        "BlockData"
    } else {
        "Data"
    };
    let data = match blocks.get(data_key) {
        Some(Value::ByteArray(data)) => data.as_slice(),
        _ => return Err(SchematicError::Missing("a Blocks.Data byte array")),
    };

    // The palette is a compound mapping a name to its index, so the *index* is the
    // order the blocks refer to — not the order the names happen to sort into. Reading
    // them in name order would silently swap blocks in any schematic whose palette was
    // not written alphabetically, and it would still load, which is the worst way for
    // it to fail.
    let mut entries: Vec<(usize, &str)> = Vec::with_capacity(palette.len());
    for (name, slot) in palette {
        match slot {
            Value::Int(index) if *index >= 0 => entries.push((*index as usize, name.as_str())),
            _ => {
                return Err(SchematicError::Palette(format!(
                    "palette entry '{name}' has no numeric index"
                )));
            }
        }
    }
    entries.sort_unstable_by_key(|(index, _)| *index);
    for (slot, (index, _)) in entries.iter().enumerate() {
        if *index != slot {
            return Err(SchematicError::Palette(format!(
                "the palette's indices are not 0..n: {slot} is missing"
            )));
        }
    }

    // Validate the packed data before resolving palette names.
    let mut indices: Vec<usize> = Vec::with_capacity(dimensions.volume());
    let mut cursor = 0usize;
    while indices.len() < dimensions.volume() {
        let (index, used) =
            read_varint(data, cursor).ok_or(SchematicError::LikelyFaweFastFormat)?;
        cursor += used;
        if index >= entries.len() {
            return Err(SchematicError::LikelyFaweFastFormat);
        }
        indices.push(index);
    }
    if cursor != data.len() {
        return Err(SchematicError::LikelyFaweFastFormat);
    }

    let entities = decode_block_entities(section, dimensions, minimum)?;

    let mut states: Vec<u16> = Vec::with_capacity(entries.len());
    for (_, name) in &entries {
        states.push(resolve(resolver, name)?);
    }

    Ok(indices
        .into_iter()
        .enumerate()
        .map(|(index, palette)| match entities.get(&index) {
            Some(entity) => BlockValue::with_entity(states[palette], entity.clone()),
            None => BlockValue::new(states[palette]),
        })
        .collect())
}

/// The block entity NBT that `Blocks.BlockEntities` holds, keyed by flat clipboard index.
///
/// `Pos` is relative to the clipboard's minimum corner — Java adds the minimum back on
/// the way in (`ReaderUtil.initializeClipboardFromBlocks:161`), because a schematic is
/// stored detached from the world it was copied from. The flat index it maps to is the
/// clipboard's own YZX order, the order the packed data above was walked in, so an
/// entity lands on the same block its palette entry did.
///
/// An entry with no `Data` has nothing to restore and is skipped: the block still comes
/// from the palette, only its NBT is absent, which is also what a freshly placed chest
/// looks like.
///
/// Rebuilds the block entity's `id` from Sponge's separate `Id` field.
fn decode_block_entities(
    section: &BlockSection<'_>,
    dimensions: Dimensions,
    minimum: BlockPos,
) -> Result<BTreeMap<usize, Box<[u8]>>, SchematicError> {
    let blocks = section.compound;
    let tag = blocks.get("BlockEntities").or_else(|| {
        (section.version == 2)
            .then(|| blocks.get("TileEntities"))
            .flatten()
    });
    let items = match tag {
        Some(Value::List { items, .. }) => items,
        // A file with no tag at all is a file with no block entities, which is what
        // Java's `findListTag` also treats it as.
        _ => return Ok(BTreeMap::new()),
    };

    let mut entities = BTreeMap::new();
    for item in items {
        let Value::Compound(entry) = item else {
            return Err(container("a BlockEntities entry is not a compound"));
        };
        let mut data = if section.version == 2 {
            let mut data = entry.clone();
            data.remove("Id");
            data.remove("Pos");
            data
        } else {
            let Some(Value::Compound(data)) = entry.get("Data") else {
                continue;
            };
            data.clone()
        };
        if let Some(Value::String(id)) = entry.get("Id") {
            data.insert("id".into(), Value::String(id.clone()));
        }
        let relative = match entry.get("Pos") {
            Some(Value::IntArray(pos)) if pos.len() >= 3 => BlockPos::new(pos[0], pos[1], pos[2]),
            _ => {
                return Err(container(
                    "a BlockEntities entry has no three-element Pos int array",
                ));
            }
        };
        if relative.x < 0
            || relative.y < 0
            || relative.z < 0
            || relative.x >= dimensions.width
            || relative.y >= dimensions.height
            || relative.z >= dimensions.length
        {
            return Err(container(format!(
                "a BlockEntities entry sits at {relative:?}, outside the schematic"
            )));
        }
        if section.version == 2 {
            for (key, base, offset) in [
                ("x", minimum.x, relative.x),
                ("y", minimum.y, relative.y),
                ("z", minimum.z, relative.z),
            ] {
                data.insert(
                    key.into(),
                    Value::Int(
                        base.checked_add(offset)
                            .ok_or_else(|| container("block entity coordinates overflow"))?,
                    ),
                );
            }
        }
        entities.insert(
            flat_index(relative, dimensions),
            nbt::write(&data).into_boxed_slice(),
        );
    }
    Ok(entities)
}

/// The clipboard's own flat index of a block at a position relative to its minimum.
///
/// The clipboard stores Y outermost, then Z, then X (`Clipboard::capture`), the same
/// order the packed data is written in, so this is what pairs a palette index with its
/// block entity.
fn flat_index(position: BlockPos, dimensions: Dimensions) -> usize {
    let width = dimensions.width as usize;
    let length = dimensions.length as usize;
    position.x as usize + position.y as usize * (width * length) + position.z as usize * width
}

/// Resolves a palette entry, reporting the name when the server has no such block.
fn resolve(resolver: &dyn BlockResolver, name: &str) -> Result<u16, SchematicError> {
    let (block, properties) = crate::pattern::split_block_id(name)
        .map_err(|why| SchematicError::Palette(format!("'{name}': {why}")))?;
    resolver
        .resolve(block, &properties)
        .ok_or_else(|| SchematicError::UnknownBlock(name.to_string()))
}

/// The block id a palette key names, with the properties taken off it.
///
/// `minecraft:oak_log[axis=x]` is `minecraft:oak_log`. The two are the same thing read by
/// the same parser the reader above uses, so this is [`pattern::split_block_id`] rather
/// than a second rule for stripping a bracket.
fn block_id(key: &str) -> Result<&str, SchematicError> {
    crate::pattern::split_block_id(key)
        .map(|(name, _)| name)
        .map_err(|why| SchematicError::Palette(format!("'{key}': {why}")))
}

/// A block state spelled the way a schematic's palette spells it.
///
/// This is `BlockStateHolder.getAsString()` (`BlockStateHolder.java:216-227`), which is
/// what `SpongeSchematicV3Writer.java:186` puts in the palette and what
/// `ReaderUtil.decodePalette` (`ReaderUtil.java:136-149`) parses back: the bare block id
/// when the state has no properties, and `id[key=value,…]` when it has them.
///
/// The properties are the whole state, not a decoration. `minecraft:oak_stairs` on its own
/// is a valid pattern that resolves to the type's *default* state, so a palette key
/// written without them loads without complaint and pastes a facing-north bottom-half
/// stair where a facing-east top-half one was saved.
///
/// Values are lower-cased, as Java lower-cases them
/// (`BlockStateHolder.java:220-224`); every property value Minecraft defines already is,
/// so this only shows up on a state spelled otherwise.
#[must_use]
pub fn state_string(info: &BlockStateInfo) -> String {
    if info.properties.is_empty() {
        return info.name.clone();
    }
    let properties = info
        .properties
        .iter()
        .map(|(key, value)| format!("{key}={}", value.to_lowercase()))
        .collect::<Vec<_>>()
        .join(",");
    format!("{}[{properties}]", info.name)
}

/// Reads one variable-length integer, seven bits per byte, high bit set on every byte
/// but the last.
///
/// The encoding `encodePalettedData` emits: `id & 127 | 128` while more bits remain,
/// then the final byte without the flag.
fn read_varint(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = 0usize;
    let mut shift = 0u32;
    let mut cursor = from;
    loop {
        let byte = *data.get(cursor)?;
        cursor += 1;
        index |= usize::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Some((index, cursor - from));
        }
        shift += 7;
        // Java's `VarIntIterator` throws only once `bitsRead > 7 * 5`, so it accepts a
        // sixth group. Its accumulator is an `int`, though, so `(next & 0x7F) << 35`
        // wraps mod 32 and the value it returns is already garbage by then. The same
        // arithmetic on a 32-bit `usize` is worse than wrapping — it is a panic — and
        // this plugin compiles to wasm32, so five groups is the limit that is both safe
        // and equivalent to what Java actually produces. A stream that keeps setting the
        // high bit past that is corrupt, not merely large.
        if shift >= usize::BITS {
            return None;
        }
    }
}

/// Writes a variable-length integer, the inverse of [`read_varint`].
fn write_varint(mut index: usize, out: &mut Vec<u8>) {
    while index >= 128 {
        out.push((index & 127) as u8 | 128);
        index >>= 7;
    }
    out.push(index as u8);
}

/// Writes a clipboard out as a version 3 schematic.
///
/// Block ids are named through `name_of`, which is what puts the palette's strings
/// together, and it has to name a *state*: the key is
/// [`state_string`]'s form — `minecraft:oak_stairs[facing=east,half=top,…]` — which is
/// what `SpongeSchematicV3Writer.java:186` writes and what this module's reader parses.
/// A schematic is only as good as that naming: an id the server cannot name comes back as
/// a load error on the other side, and one named *without* its properties comes back as a
/// different block.
///
/// Block entity NBT the clipboard carries goes into `Blocks.BlockEntities` as the format
/// expects, one compound per block. An empty list is not the same as no list: it tells a
/// WorldEdit reader the schematic has no block entities at all, so a saved chest came
/// back as an empty chest with nothing said about it on either side.
pub fn write(
    clipboard: &Clipboard,
    name_of: &dyn Fn(u16) -> Option<String>,
) -> Result<Vec<u8>, SchematicError> {
    let region = clipboard.region();
    let minimum = region.minimum();
    let dimensions = Dimensions {
        width: region.maximum().x - region.minimum().x + 1,
        height: region.maximum().y - region.minimum().y + 1,
        length: region.maximum().z - region.minimum().z + 1,
    };
    dimensions.check_size()?;

    let mut palette: Compound = Compound::new();
    let mut data = Vec::with_capacity(dimensions.volume());
    let mut block_entities: Vec<Value> = Vec::new();

    for index in 0..dimensions.volume() {
        let position = dimensions.position(index, minimum);
        let block = clipboard.block_at(position);
        let name = block
            .and_then(|block| name_of(block.state))
            .unwrap_or_else(|| "minecraft:air".to_string());
        if let Some(entity) = block.and_then(|block| block.entity.as_deref()) {
            // The entry's `Id` is the block *entity's* id, which is a different string
            // from the palette key even though both come from the same block: Java writes
            // `block.getNbtId()` (`SpongeSchematicV3Writer.java:179`), the `id` tag of the
            // block's own NBT (`BaseBlock.java:167-173`), and reads it back as that NBT
            // `id` (`ReaderUtil.java:168`). A chest is the case where the two coincide,
            // because `minecraft:chest` has no properties; a sign's do not, and an `id`
            // carrying them would match no tile entity.
            block_entities.push(block_entity(block_id(&name)?, position, minimum, entity)?);
        }
        // The generated name is the key, so two states with the same name share an entry,
        // which is what makes a schematic of a flat build tiny.
        let slot = match palette.get(&name) {
            Some(Value::Int(slot)) => usize::try_from(*slot).unwrap_or(usize::MAX),
            _ => {
                let slot = palette.len();
                palette.insert(name.clone(), Value::Int(slot as i32));
                slot
            }
        };
        write_varint(slot, &mut data);
    }

    let blocks = Compound::from([
        ("Palette".into(), Value::Compound(palette)),
        ("Data".into(), Value::ByteArray(data)),
        (
            "BlockEntities".into(),
            Value::List {
                element: nbt::TAG_COMPOUND,
                items: block_entities,
            },
        ),
    ]);

    let metadata = Compound::from([(
        "WorldEdit".into(),
        Value::Compound(Compound::from([(
            "Origin".into(),
            Value::IntArray(vec![
                clipboard.origin().x,
                clipboard.origin().y,
                clipboard.origin().z,
            ]),
        )])),
    )]);

    let schematic = Compound::from([
        ("Version".into(), Value::Int(VERSION)),
        ("DataVersion".into(), Value::Int(DATA_VERSION)),
        ("Metadata".into(), Value::Compound(metadata)),
        // A short, as the format specifies. `check_size` has already refused an axis
        // too big for one, so this only ever reinterprets a value in range — the same
        // `(short) width` of `SpongeSchematicV3Writer.java:124-126`, which its reader
        // takes back as unsigned.
        ("Width".into(), Value::Short(dimensions.width as i16)),
        ("Height".into(), Value::Short(dimensions.height as i16)),
        ("Length".into(), Value::Short(dimensions.length as i16)),
        (
            // `Offset` is the region's minimum corner *relative to the origin*, not the
            // corner itself: the reader adds the origin back to get the minimum. Writing
            // the corner would double the origin on every load.
            "Offset".into(),
            Value::IntArray(vec![
                region.minimum().x - clipboard.origin().x,
                region.minimum().y - clipboard.origin().y,
                region.minimum().z - clipboard.origin().z,
            ]),
        ),
        ("Blocks".into(), Value::Compound(blocks)),
    ]);

    // The unnamed root holding a `Schematic` member, which is the document FAWE's v3
    // writer produces: `LinRootEntry("", ...)` at `SpongeSchematicV3Writer.java:64`.
    let document = Compound::from([("Schematic".into(), Value::Compound(schematic))]);
    write_envelope(&document)
}

/// The one `Blocks.BlockEntities` entry a block carrying block entity NBT contributes.
///
/// `SpongeSchematicV3Writer.encodeBlocks` (`SpongeSchematicV3Writer.java:170-190`) puts
/// three tags in each: the block's id, its position, and its NBT under `Data`.
///
/// `Id` is the block entity's own id rather than the block's, and it carries no
/// properties: it becomes the `id` key of the NBT this reader hands back
/// (`ReaderUtil.java:168`), so it has to be a name a tile entity matches. It is taken from
/// the palette key with [`block_id`] for want of a second naming callback — the two
/// strings are one apart, and the properties are the only difference.
///
/// `Pos` is relative to the clipboard's minimum corner — `point.subtract(min)` — which
/// is what a schematic is: a build stored apart from the world it was copied from. An
/// absolute position here would put a saved chest's contents back at the coordinates it
/// happened to be captured at.
///
/// `Data` is a bare compound in Java, because `block.getNbt()` hands the writer a
/// compound. The clipboard holds the same tags as raw bytes, read back here with
/// [`nbt::read`], so they are an NBT document as [`nbt::write`] produces one; embedding
/// them as a byte array instead would be something no other reader of the format could
/// make sense of.
///
/// Unreadable bytes are an error rather than a skipped entry, because skipping is what
/// made a chest come back empty: an entry this cannot read is a chest whose contents
/// would be lost with nothing to show for it.
fn block_entity(
    name: &str,
    position: BlockPos,
    minimum: BlockPos,
    entity: &[u8],
) -> Result<Value, SchematicError> {
    let mut data = nbt::read(entity).map_err(|error| {
        container(format!(
            "could not read the block entity NBT at {position:?}: {error}"
        ))
    })?;
    let id = data
        .remove("id")
        .unwrap_or_else(|| Value::String(name.to_string()));
    for coordinate in ["x", "y", "z"] {
        data.remove(coordinate);
    }
    Ok(Value::Compound(Compound::from([
        ("Id".into(), id),
        (
            "Pos".into(),
            Value::IntArray(vec![
                position.x - minimum.x,
                position.y - minimum.y,
                position.z - minimum.z,
            ]),
        ),
        ("Data".into(), Value::Compound(data)),
    ])))
}
