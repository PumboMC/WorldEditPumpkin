//! The engine's view of a world, and the place an edit is applied.

use crate::{block::BlockValue, math::BlockPos};

/// Somewhere blocks can be read and written.
///
/// This is the seam between the engine and the server. Everything above it works
/// in block-state ids and knows nothing about Pumpkin; the Pumpkin side implements
/// this over the plugin WIT's `world` resource.
pub trait Extent {
    /// The block at `pos`, with its block entity NBT if it has one.
    fn get_block(&self, pos: BlockPos) -> BlockValue;

    /// Writes `block` at `pos`, replacing whatever was there.
    fn set_block(&mut self, pos: BlockPos, block: &BlockValue);

    /// Attempt a write subject to platform protection. Refused writes must not
    /// be counted or recorded in history by an edit job.
    fn try_set_block(&mut self, pos: BlockPos, block: &BlockValue) -> bool {
        self.set_block(pos, block);
        true
    }

    /// The lowest buildable Y in this world.
    fn min_y(&self) -> i32;

    /// The highest buildable Y in this world, inclusive.
    fn max_y(&self) -> i32;

    /// Clears the block at `pos`, which also clears any block entity.
    fn clear_block(&mut self, pos: BlockPos) {
        self.set_block(pos, &BlockValue::new(self.air_state()));
    }

    /// The block-state id for air, needed by [`Extent::clear_block`] and by the
    /// masks that treat "no block here" as a real state.
    fn air_state(&self) -> u16;
}
