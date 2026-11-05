//! Blocks, and the platform hook that turns names into the server's own ids.

/// A block as the engine passes it around.
///
/// The state is the server's numeric block-state id rather than a name, because
/// an edit touches the same handful of states millions of times and comparing
/// `u16`s is what makes that affordable. This mirrors FAWE, which resolves
/// patterns to palette indices before the queue ever runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockValue {
    /// The server's block-state id.
    pub state: u16,
    /// The block entity's raw NBT, for signs, banners, chests and the rest.
    ///
    /// Stored as a Java NBT document with an empty root name.
    pub entity: Option<Box<[u8]>>,
}

impl BlockValue {
    /// A plain block with no block entity.
    #[must_use]
    pub const fn new(state: u16) -> Self {
        Self {
            state,
            entity: None,
        }
    }

    /// A block carrying block entity NBT.
    #[must_use]
    pub fn with_entity(state: u16, entity: Box<[u8]>) -> Self {
        Self {
            state,
            entity: Some(entity),
        }
    }
}

/// A block-state id resolved back to a name and its properties.
///
/// Needed where a name has to reach the player, such as `//count` reporting what
/// it found or `//gmask` echoing the mask back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockStateInfo {
    /// The namespaced block id, for example `minecraft:stone`.
    pub name: String,
    /// The block state's property key-value pairs, for example `facing=north`.
    pub properties: Vec<(String, String)>,
}

/// What the engine needs from the server's block registry.
///
/// Patterns and masks name blocks as text and the engine immediately turns that
/// into ids, so an unknown block is reported while parsing rather than silently
/// skipped halfway through an edit.
pub trait BlockResolver {
    /// The block-state id for `name` with `properties`, if the server has one.
    ///
    /// A block with properties the server does not have must return `None` rather
    /// than falling back to the property-less state, because `//set
    /// oak_log[facing=sideways]` has to fail instead of placing a default log.
    fn resolve(&self, name: &str, properties: &[(String, String)]) -> Option<u16>;

    /// The name and properties behind a block-state id.
    fn info(&self, state: u16) -> Option<BlockStateInfo>;

    /// Whether the block-state id is air.
    fn is_air(&self, state: u16) -> bool;

    /// Whether the block-state id is solid, which is what `#solid` matches.
    fn is_solid(&self, state: u16) -> bool;

    /// Whether the block-state id is a liquid, which is what `#liquid` matches.
    fn is_liquid(&self, state: u16) -> bool;

    /// The namespaced block id a block-state id belongs to, for mask matching
    /// against block names rather than whole states.
    fn block_name(&self, state: u16) -> Option<&str>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-property block is compared as an ordered list, so a round trip has
    /// to preserve the order the resolver reported.
    #[test]
    fn a_block_state_info_keeps_its_property_order() {
        let info = BlockStateInfo {
            name: "minecraft:oak_log".to_string(),
            properties: vec![
                ("axis".to_string(), "x".to_string()),
                ("facing".to_string(), "north".to_string()),
            ],
        };
        assert_eq!(info.properties[0].0, "axis");
        assert_eq!(info.properties[1].1, "north");
    }

    #[test]
    fn a_block_value_without_a_block_entity_compares_on_state_alone() {
        assert_eq!(BlockValue::new(4), BlockValue::new(4));
        assert_ne!(
            BlockValue::new(4),
            BlockValue::with_entity(4, vec![1, 2, 3].into_boxed_slice())
        );
    }
}
