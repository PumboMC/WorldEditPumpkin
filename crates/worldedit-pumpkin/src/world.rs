//! The Pumpkin side of the engine: a world the engine can read and write.

use std::cell::RefCell;

use pumpkin_plugin_api::{
    World as PumpkinWorld,
    world::{
        BlockFlags, BlockPos as WitBlockPos, block_state_to_info, get_all_blocks,
        get_block_state_by_id, resolve_block_state,
    },
};
// `Registry` needs no world handle, so a name can be resolved without one.
use worldedit_core::{
    block::{BlockResolver, BlockStateInfo, BlockValue},
    extent::Extent,
    math::BlockPos,
    schematic,
    shape::Bounds,
};

/// A block in a Pumpkin world, as the engine's [`Extent`] sees it.
pub struct PumpkinExtent {
    world: PumpkinWorld,
    min_y: i32,
    max_y: i32,
    air: u16,
    /// Block name lookups, kept alongside the world so a command has one thing to
    /// carry rather than two that have to be kept in step.
    registry: Registry,
    actor: Option<String>,
    level: String,
    dimension: String,
    denied: usize,
}

/// Block name lookup against the server's block registry.
///
/// Separate from [`PumpkinExtent`] because resolving a name does not touch the world
/// at all — the host's functions are module-level — and because an edit needs the
/// world mutably and the resolver immutably at the same time, which one value holding
/// both cannot lend out.
#[derive(Clone, Default)]
pub struct Registry {
    /// Block names resolved while parsing, memoised for the duration of one command.
    ///
    /// A pattern is resolved once while parsing, so this only ever serves the
    /// handful of blocks one command names. `RefCell` rather than a lock because a
    /// Wasm guest is single-threaded and the engine never re-enters it.
    resolved: RefCell<Vec<(String, Option<u16>)>>,
}

/// Every block name the server has, enumerated once for the whole process.
///
/// A cache on [Registry] would be useless: a tab-completion builds a fresh one per
/// keystroke and throws it away, so the thousand-block walk would happen on every key
/// press. The block list cannot change while the server is running, so it belongs here
/// rather than on a value that dies with the request.
static ALL_BLOCK_NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Block names by state id, looked up at most once per distinct state.
///
/// A mask tests one block per position in the selection, so this is the hottest lookup in
/// the plugin: `//gmask stone` over a million blocks asks for the same few names a million
/// times. The host builds a fresh `String` for every answer, so each one has to be kept
/// rather than handed straight back — but kept *once*, not once per call, or the heap grows
/// by a million strings for the life of the process.
///
/// The signature hands back a borrowed `&str`, which cannot come out of a lock guard, so
/// what is stored is `&'static str`. One leaked allocation per block state — a couple of
/// thousand for the whole server, bounded by the registry rather than by the size of the
/// edit — is what buys a `&str` the caller can hold.
static BLOCK_NAMES: std::sync::OnceLock<std::sync::Mutex<Vec<Option<&'static str>>>> =
    std::sync::OnceLock::new();

/// The block name for `state`, from the host the first time and from memory after that.
fn block_name(state: u16) -> Option<&'static str> {
    let cache = BLOCK_NAMES.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut names = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let index = state as usize;
    if names.len() <= index {
        names.resize(index + 1, None);
    }
    if names[index].is_none() {
        names[index] = block_state_to_info(state)
            .map(|info| &*Box::leak(info.name.into_boxed_str()) as &'static str);
    }
    names[index]
}

/// Block-state strings by state id, cached the way [`block_name`] is.
///
/// A separate cache because the two spellings are asked for by different work and neither
/// pass wants the other's: a mask tests block types, a save names whole states. Both go
/// through the host's one `block_state_to_info`, which builds both halves and a `Vec` of
/// properties to do it.
static BLOCK_STATE_STRINGS: std::sync::OnceLock<std::sync::Mutex<Vec<Option<&'static str>>>> =
    std::sync::OnceLock::new();

impl Registry {
    /// A registry with nothing resolved yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every block type the server has, by namespaced name.
    ///
    /// Sorted so a suggestion filter can be a simple prefix scan, and de-duplicated
    /// because the host's list is one entry per block type with no promise that they are
    /// distinct as written.
    #[must_use]
    pub fn all_block_names(&self) -> Vec<String> {
        ALL_BLOCK_NAMES
            .get_or_init(|| {
                let mut names: Vec<String> = get_all_blocks()
                    .iter()
                    .map(|block| block.name.clone())
                    .collect();
                names.sort();
                names.dedup();
                names
            })
            .clone()
    }

    /// A block state spelled the way a schematic's palette spells it.
    ///
    /// The id *with* its properties — `minecraft:oak_stairs[facing=east,half=top,…]` —
    /// which is `BlockStateHolder.getAsString()` (`BlockStateHolder.java:216-227`), what
    /// `SpongeSchematicV3Writer.java:186` writes into the palette and what
    /// `ReaderUtil.java:136-149` reads it back with. [`schematic::state_string`] does the
    /// spelling; this asks the host for the state and keeps the answer, for the same
    /// reason [`block_name`] does — saving a build asks for the same few states once per
    /// block, and the host builds a fresh answer every time.
    ///
    /// Deliberately *not* what [`BlockResolver::block_name`] returns. That one answers
    /// with the block type, which is what a mask matches and what `//info` prints; a
    /// schematic needs the whole state, or every stair, log, door and sign in a saved
    /// build comes back as its default.
    #[must_use]
    pub fn state_string(&self, state: u16) -> Option<&'static str> {
        let cache = BLOCK_STATE_STRINGS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
        let mut strings = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = state as usize;
        if strings.len() <= index {
            strings.resize(index + 1, None);
        }
        if strings[index].is_none() {
            strings[index] = self.info(state).map(|info| {
                &*Box::leak(schematic::state_string(&info).into_boxed_str()) as &'static str
            });
        }
        strings[index]
    }
}

impl BlockResolver for Registry {
    /// Resolves a block name plus properties to the server's state id.
    ///
    /// An unknown block, or a state the server does not have, returns `None` so
    /// the caller reports it. Silently falling back to the property-less state
    /// would let `//set oak_log[facing=sideways]` place a default log instead of
    /// failing.
    fn resolve(&self, name: &str, properties: &[(String, String)]) -> Option<u16> {
        let rendered = render_properties(properties);
        let key = format!("{name}[{}]", render_key(properties));
        if let Some((_, state)) = self
            .resolved
            .borrow()
            .iter()
            .find(|(cached, _)| *cached == key)
        {
            return *state;
        }
        let state = resolve_block_state(name, &rendered);
        self.resolved.borrow_mut().push((key, state));
        state
    }

    fn info(&self, state: u16) -> Option<BlockStateInfo> {
        block_state_to_info(state).map(|info| BlockStateInfo {
            name: info.name,
            properties: info.properties,
        })
    }

    fn is_air(&self, state: u16) -> bool {
        block_info(state).is_air
    }

    fn is_solid(&self, state: u16) -> bool {
        block_info(state).is_solid
    }

    fn is_liquid(&self, state: u16) -> bool {
        block_info(state).is_liquid
    }

    fn block_name(&self, state: u16) -> Option<&str> {
        block_name(state)
    }
}

impl PumpkinExtent {
    /// Wraps a world, taking its height from configuration.
    ///
    /// The plugin WIT exposes `get-min-y` but not `get-max-y`, so the ceiling has to
    /// come from configuration rather than from the host. See the README's "Known
    /// gaps" section.
    #[must_use]
    pub fn new(world: PumpkinWorld, world_height: i32) -> Self {
        let min_y = world.get_min_y();
        let level = world.get_name();
        let dimension = world.get_dimension();
        Self {
            world,
            min_y,
            max_y: min_y + world_height - 1,
            air: 0,
            registry: Registry::new(),
            actor: None,
            level,
            dimension,
            denied: 0,
        }
    }

    pub fn with_actor(mut self, player: &str) -> Self {
        self.actor = Some(player.to_string());
        self
    }

    pub fn denied_blocks(&self) -> usize {
        self.denied
    }

    fn write_block(&mut self, position: BlockPos, block: &BlockValue) -> bool {
        let entity = match block
            .entity
            .as_deref()
            .map(crate::block_entity::to_host)
            .transpose()
        {
            Ok(entity) => entity,
            Err(error) => {
                tracing::error!(?position, error, "could not restore block entity");
                return false;
            }
        };
        if self.world.get_block_entity_nbt(wit(position)).is_some() {
            self.world.set_block_state(
                wit(position),
                self.air,
                update_flags() | BlockFlags::SKIP_BLOCK_ADDED_CALLBACK,
            );
        }
        let flags = if entity.is_some() {
            update_flags() | BlockFlags::SKIP_BLOCK_ADDED_CALLBACK
        } else {
            update_flags()
        };
        self.world
            .set_block_state(wit(position), block.state, flags);
        if let Some(entity) = entity
            && let Err(error) = self.world.set_block_entity_nbt(wit(position), &entity)
        {
            tracing::error!(?position, error, "could not restore block entity");
        }
        true
    }

    /// The world's inclusive Y range, which is what a generated shape is clipped to.
    #[must_use]
    pub fn bounds(&self) -> Bounds {
        Bounds {
            min_y: self.min_y,
            max_y: self.max_y,
        }
    }

    /// The name of the world, for a message that has to name it.
    #[must_use]
    pub fn name(&self) -> String {
        self.world.get_name()
    }

    /// The exact level and dimension this extent writes.
    pub fn identity(&self) -> (&str, &str) {
        (&self.level, &self.dimension)
    }

    /// The block registry, for resolving a pattern or a mask.
    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }
}

impl Extent for PumpkinExtent {
    fn get_block(&self, position: BlockPos) -> BlockValue {
        let state = self.world.get_block_state_id(wit(position));
        match self.world.get_block_entity_nbt(wit(position)) {
            Some(bytes) => match crate::block_entity::from_host(&bytes) {
                Ok(entity) => BlockValue::with_entity(state, entity),
                Err(error) => {
                    tracing::error!(?position, error, "could not capture block entity");
                    BlockValue::new(state)
                }
            },
            None => BlockValue::new(state),
        }
    }

    fn set_block(&mut self, position: BlockPos, block: &BlockValue) {
        self.try_set_block(position, block);
    }

    fn try_set_block(&mut self, position: BlockPos, block: &BlockValue) -> bool {
        let old = self.world.get_block_state_id(wit(position));
        let actions =
            u8::from(!block_info(old).is_air) | (u8::from(!block_info(block.state).is_air) << 1);
        if !crate::protection::allows(
            self.actor.as_deref(),
            &self.level,
            &self.dimension,
            position,
            if actions == 0 { 3 } else { actions },
        ) {
            self.denied += 1;
            return false;
        }
        self.write_block(position, block)
    }

    fn min_y(&self) -> i32 {
        self.min_y
    }

    fn max_y(&self) -> i32 {
        self.max_y
    }

    fn air_state(&self) -> u16 {
        self.air
    }
}

/// The update flags an edit writes with.
///
/// `notify-listeners` is what pushes the change to players, and it is the whole
/// point of an edit. `notify-neighbors` is deliberately **not** set: a bulk edit
/// should not run a neighbour update per block, which is exactly what makes a
/// large selection stall the tick. FAWE makes neighbour updates a separate,
/// explicitly triggered pass for the same reason.
#[must_use]
pub fn update_flags() -> BlockFlags {
    BlockFlags::NOTIFY_LISTENERS
        | BlockFlags::SKIP_DROPS
        | BlockFlags::SKIP_BLOCK_ENTITY_REPLACED_CALLBACK
}

/// Renders properties as the WIT's `list<tuple<string, string>>`.
///
/// The engine's ordering is preserved rather than sorted, because two patterns that
/// differ only in the order of their properties are still the same block and
/// sorting would make the cache key collide.
#[must_use]
pub fn render_properties(properties: &[(String, String)]) -> Vec<(String, String)> {
    properties.to_vec()
}

/// A cache key for a set of properties.
///
/// The engine keeps properties in the order they were typed, but the server does
/// not care about the order, so `axis=x,waterlogged=false` and
/// `waterlogged=false,axis=x` are one block. Without folding them into one key the
/// cache would hold two entries for the same state and resolve it twice.
#[must_use]
pub fn render_key(properties: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = properties.iter().collect();
    sorted.sort();
    sorted
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The server's view of a block state, or a stand-in for an id it does not know.
///
/// Returning all-false rather than panicking matters because a mask asks about the
/// state of every block in a selection, and an id the host does not recognise must
/// not take the server down.
fn block_info(state: u16) -> pumpkin_plugin_api::world::BlockState {
    get_block_state_by_id(state).unwrap_or(pumpkin_plugin_api::world::BlockState {
        id: state,
        block_id: 0,
        block_name: String::new(),
        luminance: 0,
        opacity: 0,
        hardness: 0.0,
        is_air: false,
        is_liquid: false,
        is_solid: false,
        is_full_cube: false,
        piston_behavior: pumpkin_plugin_api::world::PistonBehavior::Normal,
        has_random_ticks: false,
        burnable: false,
        tool_required: false,
        sided_transparency: false,
        replaceable: false,
        is_solid_block: false,
        block_entity_type: 0,
        instrument: pumpkin_plugin_api::world::NoteblockInstrument::Harp,
        collision_shapes: Vec::new(),
        outline_shapes: Vec::new(),
        down_side_solid: false,
        up_side_solid: false,
        north_side_solid: false,
        south_side_solid: false,
        west_side_solid: false,
        east_side_solid: false,
        down_center_solid: false,
        up_center_solid: false,
        map_color: 0,
        properties: Vec::new(),
    })
}

/// Converts an engine position into the WIT's.
#[must_use]
pub fn wit(position: BlockPos) -> WitBlockPos {
    WitBlockPos {
        x: position.x,
        y: position.y,
        z: position.z,
    }
}

/// Converts a WIT block position into an engine position.
#[must_use]
pub fn from_wit(position: WitBlockPos) -> BlockPos {
    BlockPos::new(position.x, position.y, position.z)
}

/// The world and the registry can be borrowed at the same time, one mutably.
///
/// This is the property `jobs::Pending::advance` depends on: an edit writes through the
/// world while naming blocks through the resolver, so a single value holding both could
/// not lend out the pair.
///
/// It is outside the test module and never called, because the assertion is that it
/// *compiles* rather than what it returns. Every method on either trait reaches a Wasm
/// host function and traps on the host target, so there is nothing to call at run time.
/// Folding `Registry` back into `PumpkinExtent` stops this compiling, which is the
/// regression it guards.
#[allow(dead_code, reason = "the point is that it compiles")]
fn borrows_the_world_and_the_registry_at_once(extent: &mut PumpkinExtent, registry: &Registry) {
    fn takes_both(extent: &mut dyn Extent, resolver: &dyn BlockResolver) -> (i32, u16) {
        (extent.min_y(), resolver.resolve("stone", &[]).unwrap_or(0))
    }
    let _ = takes_both(extent, registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property list has to survive the round trip into the WIT unchanged,
    /// because the resolver keys its cache on the rendered form.
    #[test]
    fn properties_keep_the_order_they_were_written_in() {
        let properties = vec![
            ("facing".to_string(), "north".to_string()),
            ("half".to_string(), "bottom".to_string()),
        ];
        assert_eq!(render_properties(&properties), properties);
    }

    #[test]
    fn a_position_survives_the_conversion_both_ways() {
        let position = BlockPos::new(-3, 64, 4096);
        assert_eq!(from_wit(wit(position)), position);
    }

    /// The cache key folds the property order away, because two patterns that differ
    /// only in the order they wrote their properties are the same block. Without this
    /// the registry would hold two entries for one state and resolve it twice.
    #[test]
    fn the_cache_key_ignores_the_order_the_properties_were_written_in() {
        let one = vec![
            ("axis".to_string(), "y".to_string()),
            ("waterlogged".to_string(), "false".to_string()),
        ];
        let other = vec![
            ("waterlogged".to_string(), "false".to_string()),
            ("axis".to_string(), "y".to_string()),
        ];
        assert_eq!(render_key(&one), render_key(&other));
        // But a different value is a different block.
        let different = vec![
            ("axis".to_string(), "x".to_string()),
            ("waterlogged".to_string(), "false".to_string()),
        ];
        assert_ne!(render_key(&one), render_key(&different));
    }
}
