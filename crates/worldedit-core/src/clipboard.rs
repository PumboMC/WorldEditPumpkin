//! The clipboard: what `//copy` and `//cut` captured, and what `//paste` replays.

use crate::{
    block::{BlockResolver, BlockValue},
    extent::Extent,
    math::BlockPos,
    region::{CuboidRegion, Region},
    transform::Transform,
};

/// Blocks captured from the world, plus the origin they were captured against.
///
/// Storage is a flat array in the same YZX order FAWE's clipboards and the
/// schematic format use: `index = x + y * width * length + z * width`. Keeping
/// that order means a schematic written from this needs no reordering, and it is
/// the order `CuboidRegion.iterator_old` already walks in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clipboard {
    region: CuboidRegion,
    origin: BlockPos,
    blocks: Vec<BlockValue>,
}

impl Clipboard {
    /// Captures `region` from `source`, anchored so that `origin` maps to the
    /// clipboard's `(0, 0, 0)`.
    ///
    /// Block entity NBT is kept, because a `//copy` of a chest has to paste a
    /// chest with the same contents. `//set` and `//replace` leave NBT alone,
    /// which is why they do not go through here.
    #[must_use]
    pub fn capture(source: &dyn Extent, region: CuboidRegion, origin: BlockPos) -> Self {
        let dimensions = region.dimensions();
        let minimum = region.minimum();
        let mut blocks = Vec::with_capacity(dimensions.volume());
        for y in 0..dimensions.y {
            for z in 0..dimensions.z {
                for x in 0..dimensions.x {
                    blocks.push(source.get_block(minimum.offset(x, y, z)));
                }
            }
        }
        Self {
            region,
            origin,
            blocks,
        }
    }

    /// A clipboard of the given size holding `air` throughout.
    ///
    /// `//copy` needs this when a mask matched nothing: the selection still has a
    /// size, and `//size -c` has to be able to report it.
    #[must_use]
    pub fn empty(region: CuboidRegion, origin: BlockPos, air: BlockValue) -> Self {
        let dimensions = region.dimensions();
        Self {
            region,
            origin,
            blocks: vec![air; dimensions.volume()],
        }
    }

    /// A clipboard holding `blocks`, which must be in this clipboard's own order:
    /// Y outermost, then Z, then X, relative to the region's minimum corner.
    ///
    /// For a schematic reader, which has already produced exactly that order. Public
    /// because the order is the format's, not an implementation detail: a caller that
    /// filled it in the usual X-fastest order would transpose the build without any
    /// error, so the requirement is stated rather than implied.
    #[must_use]
    pub fn from_blocks(region: CuboidRegion, origin: BlockPos, blocks: Vec<BlockValue>) -> Self {
        Self {
            region,
            origin,
            blocks,
        }
    }

    /// The region the blocks were copied from, in world coordinates.
    #[must_use]
    pub fn region(&self) -> CuboidRegion {
        self.region
    }

    /// Where the clipboard's `(0, 0, 0)` was in the world.
    #[must_use]
    pub fn origin(&self) -> BlockPos {
        self.origin
    }

    /// Moves the paste anchor, which is what `//paste -o` and `//copy -c` use.
    pub fn set_origin(&mut self, origin: BlockPos) {
        self.origin = origin;
    }

    /// How many blocks the clipboard holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether the clipboard holds no blocks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// The block at a world position inside the captured region.
    #[must_use]
    pub fn block_at(&self, position: BlockPos) -> Option<&BlockValue> {
        if !self.region.contains(position) {
            return None;
        }
        let minimum = self.region.minimum();
        self.blocks
            .get(self.index(position.offset(-minimum.x, -minimum.y, -minimum.z)))
    }

    /// The clipboard position `destination` takes its block from under
    /// `transform`, or `None` when that lands outside the captured region.
    ///
    /// This is the read half of the paste mapping, and it is why a rotated paste
    /// of a non-square clipboard leaves some blocks out rather than reading past
    /// the end of the block array.
    #[must_use]
    pub fn source_of(&self, destination: BlockPos, transform: &Transform) -> Option<BlockPos> {
        let source = transform.paste_position(destination, self.origin);
        self.region.contains(source).then_some(source)
    }

    /// The `(destination, block)` pairs a paste writes.
    ///
    /// Each captured block goes to its transformed position, then `shift` moves the
    /// whole thing. The order is the shift after the rotation, which is what makes
    /// `//rotate 90` followed by a paste land the copy beside the player rather
    /// than rotated around wherever it was originally copied.
    #[must_use]
    pub fn paste_steps(
        &self,
        transform: &Transform,
        shift: BlockPos,
    ) -> Vec<(BlockPos, BlockValue)> {
        crate::region::RegionWalker::new(self.region.boxed())
            .map(|source| (transform.paste_position(source, self.origin), source))
            .filter_map(|(rotated, source)| {
                self.block_at(source)
                    .map(|block| (rotated.offset(shift.x, shift.y, shift.z), block.clone()))
            })
            .collect()
    }

    /// The same paste, with the blocks turned as well as their positions.
    ///
    /// `BlockTransformExtent` is what makes this necessary: a stair copied facing
    /// north has to come out of `//rotate 90` facing east, or the copy is sideways
    /// and the wrong way round. Turning a state needs a resolver to read its
    /// properties and resolve the turned ones, which [`Self::paste_steps`] has no
    /// way to reach - so it copies the block as it was.
    #[must_use]
    pub fn paste_steps_with(
        &self,
        blocks: &dyn BlockResolver,
        transform: &Transform,
        shift: BlockPos,
    ) -> Vec<(BlockPos, BlockValue)> {
        crate::region::RegionWalker::new(self.region.boxed())
            .map(|source| (transform.paste_position(source, self.origin), source))
            .filter_map(|(rotated, source)| {
                self.block_at(source).map(|block| {
                    let turned = transform.transform_block(block, blocks);
                    (rotated.offset(shift.x, shift.y, shift.z), turned)
                })
            })
            .collect()
    }

    fn index(&self, offset: BlockPos) -> usize {
        let dimensions = self.region.dimensions();
        offset.x as usize
            + offset.y as usize * (dimensions.x * dimensions.z) as usize
            + offset.z as usize * dimensions.x as usize
    }
}

/// A clipboard together with the transform `//paste` should apply.
///
/// `//rotate` and `//flip` set the transform here rather than rearranging the
/// captured blocks, so rotating back and pasting again gives the original copy.
///
/// Not `Eq`: a transform carries rotation angles, which compare by value here but
/// have no meaningful total order for hashing.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipboardHolder {
    clipboard: Clipboard,
    transform: Transform,
}

impl ClipboardHolder {
    /// A holder over `clipboard` with no transform.
    #[must_use]
    pub fn new(clipboard: Clipboard) -> Self {
        Self {
            clipboard,
            transform: Transform::Identity,
        }
    }

    /// The captured blocks.
    #[must_use]
    pub fn clipboard(&self) -> &Clipboard {
        &self.clipboard
    }

    /// The captured blocks, mutably, for `//copy -c` re-anchoring.
    pub fn clipboard_mut(&mut self) -> &mut Clipboard {
        &mut self.clipboard
    }

    /// The transform applied while pasting.
    #[must_use]
    pub fn transform(&self) -> &Transform {
        &self.transform
    }

    /// Replaces the transform.
    pub fn set_transform(&mut self, transform: Transform) {
        self.transform = transform;
    }

    /// Stacks `transform` onto the current one, which is what `//rotate` and
    /// `//flip` do so that repeating them composes.
    ///
    /// The existing transform is applied first and the new one second, because that
    /// is the order FAWE's `combine` produces. `combine` is the matrix product
    /// `new * existing`, and a matrix product applies right to left, so
    /// `new.combine(existing)` means "existing, then new".
    ///
    /// This is not a harmless detail: a quarter turn and a mirror do not commute, so
    /// building the chain the other way round makes `//rotate 90` then `//flip` paste
    /// somewhere else entirely. See `a_second_transform_is_applied_after_the_first`.
    pub fn push_transform(&mut self, transform: Transform) {
        self.transform = self.transform.clone().then(transform);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::BlockAxis;

    /// A world of known blocks, standing in for the server.
    struct Grid {
        region: CuboidRegion,
        blocks: Vec<BlockValue>,
        air: u16,
    }

    impl Grid {
        fn from_states(region: CuboidRegion, states: Vec<u16>) -> Self {
            Self {
                region,
                blocks: states.into_iter().map(BlockValue::new).collect(),
                air: u16::MAX,
            }
        }
    }

    impl Extent for Grid {
        fn get_block(&self, position: BlockPos) -> BlockValue {
            let dimensions = self.region.dimensions();
            let minimum = self.region.minimum();
            if !self.region.contains(position) {
                return BlockValue::new(self.air);
            }
            let offset = position.offset(-minimum.x, -minimum.y, -minimum.z);
            let index = offset.x as usize
                + offset.y as usize * (dimensions.x * dimensions.z) as usize
                + offset.z as usize * dimensions.x as usize;
            self.blocks
                .get(index)
                .cloned()
                .unwrap_or(BlockValue::new(self.air))
        }

        fn set_block(&mut self, position: BlockPos, block: &BlockValue) {
            let dimensions = self.region.dimensions();
            let minimum = self.region.minimum();
            if !self.region.contains(position) {
                return;
            }
            let offset = position.offset(-minimum.x, -minimum.y, -minimum.z);
            let index = offset.x as usize
                + offset.y as usize * (dimensions.x * dimensions.z) as usize
                + offset.z as usize * dimensions.x as usize;
            if let Some(slot) = self.blocks.get_mut(index) {
                *slot = block.clone();
            }
        }

        fn min_y(&self) -> i32 {
            self.region.minimum().y
        }

        fn max_y(&self) -> i32 {
            self.region.maximum().y
        }

        fn air_state(&self) -> u16 {
            self.air
        }
    }

    fn states(count: usize) -> Vec<u16> {
        (0..count).map(|value| value as u16).collect()
    }

    /// The clipboard has to agree with the schematic format's YZX order: X is the
    /// fastest axis, then Z, then Y. Getting this backwards transposes every copy
    /// in X and Z.
    #[test]
    fn a_capture_is_stored_in_yzx_order() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 0, 1));
        let all_states = states(6);
        let source = Grid::from_states(region, all_states.clone());
        let clipboard = Clipboard::capture(&source, region, BlockPos::new(0, 0, 0));

        for (index, state) in all_states.iter().enumerate() {
            let x = (index % 3) as i32;
            let z = (index / 3) as i32;
            assert_eq!(
                clipboard.block_at(BlockPos::new(x, 0, z)).map(|b| b.state),
                Some(*state),
                "block at x={x} z={z} should be at flat index {index}"
            );
        }
    }

    /// The flat index has to agree with the walk order on every axis, or a copy of
    /// a selection that is offset from the origin comes back transposed.
    #[test]
    fn a_capture_reads_the_region_it_was_given() {
        let region = CuboidRegion::new(BlockPos::new(-3, 64, 5), BlockPos::new(0, 65, 7));
        let dimensions = region.dimensions();
        let all_states = states(dimensions.volume());
        let source = Grid::from_states(region, all_states.clone());
        let clipboard = Clipboard::capture(&source, region, BlockPos::new(-3, 64, 5));

        let minimum = region.minimum();
        let mut index = 0usize;
        for y in 0..dimensions.y {
            for z in 0..dimensions.z {
                for x in 0..dimensions.x {
                    assert_eq!(
                        clipboard.block_at(minimum.offset(x, y, z)).map(|b| b.state),
                        Some(all_states[index])
                    );
                    index += 1;
                }
            }
        }
    }

    /// `//copy` of a chest has to paste a chest with the same contents, so the
    /// capture cannot drop block entity NBT the way `//set` leaves it alone.
    #[test]
    fn a_capture_keeps_block_entity_data() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
        let nbt = vec![1u8, 2, 3].into_boxed_slice();
        let mut source = Grid::from_states(region, vec![7]);
        source.set_block(
            BlockPos::new(0, 0, 0),
            &BlockValue::with_entity(7, nbt.clone()),
        );

        let clipboard = Clipboard::capture(&source, region, BlockPos::new(0, 0, 0));
        assert_eq!(
            clipboard
                .block_at(BlockPos::new(0, 0, 0))
                .and_then(|block| block.entity.clone()),
            Some(nbt)
        );
    }

    #[test]
    fn reading_outside_the_captured_region_yields_nothing() {
        let region = CuboidRegion::new(BlockPos::new(10, 10, 10), BlockPos::new(12, 10, 12));
        let clipboard = Clipboard::empty(region, BlockPos::new(10, 10, 10), BlockValue::default());
        assert!(clipboard.block_at(BlockPos::new(11, 10, 11)).is_some());
        assert!(clipboard.block_at(BlockPos::new(9, 10, 11)).is_none());
        assert!(clipboard.block_at(BlockPos::new(11, 11, 11)).is_none());
    }

    /// A rotated paste must read from inside the clipboard, and a non-square one
    /// has positions that rotate outside it. Those have to come back as `None`
    /// rather than indexing past the end of the block array.
    #[test]
    fn a_rotated_paste_off_the_edge_of_the_clipboard_yields_nothing() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 0, 0));
        let clipboard = Clipboard::empty(region, BlockPos::new(0, 0, 0), BlockValue::default());
        let transform = Transform::RotateY(-90.0);
        assert_eq!(
            clipboard.source_of(BlockPos::new(0, 0, -1), &transform),
            Some(BlockPos::new(1, 0, 0))
        );
        assert_eq!(
            clipboard.source_of(BlockPos::new(0, 0, 1), &transform),
            None
        );
    }

    #[test]
    fn a_holder_stacks_transforms_rather_than_replacing_them() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
        let mut holder = ClipboardHolder::new(Clipboard::empty(
            region,
            BlockPos::new(0, 0, 0),
            BlockValue::default(),
        ));
        assert!(holder.transform().is_identity());

        // Mirroring in X and then rotating a quarter turn has to leave both in
        // effect; replacing the transform would silently drop the mirror.
        holder.push_transform(Transform::Flip(BlockAxis::X));
        holder.push_transform(Transform::RotateY(-90.0));

        let mirror_only = Transform::Flip(BlockAxis::X)
            .paste_position(BlockPos::new(2, 0, 1), BlockPos::new(0, 0, 0));
        let both = holder
            .transform()
            .paste_position(BlockPos::new(2, 0, 1), BlockPos::new(0, 0, 0));
        assert_ne!(both, mirror_only);
    }

    /// The order a second transform is applied in, which a rotation and a mirror do
    /// not commute and so is observable.
    ///
    /// FAWE stacks with `holder.setTransform(new.combine(existing))`, and `combine`
    /// is the matrix product `new * existing`. A matrix product applies right to left,
    /// so the **existing** transform is applied first and the new one second.
    ///
    /// So `//rotate 90` then `//flip` turns first and mirrors second: (2, 0, 1) goes
    /// to (-1, 0, 2) under the quarter turn and then to (1, 0, 2) under the mirror.
    /// The other order — mirror then turn — would give (-1, 0, -2).
    #[test]
    fn a_second_transform_is_applied_after_the_first() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
        let mut holder = ClipboardHolder::new(Clipboard::empty(
            region,
            BlockPos::new(0, 0, 0),
            BlockValue::default(),
        ));

        holder.push_transform(Transform::RotateY(-90.0));
        holder.push_transform(Transform::Flip(BlockAxis::X));

        assert_eq!(
            holder
                .transform()
                .paste_position(BlockPos::new(2, 0, 1), BlockPos::new(0, 0, 0)),
            BlockPos::new(1, 0, 2),
            "//rotate 90 then //flip must turn first and mirror second, as FAWE does"
        );
    }

    /// The same order seen from the other side: rotating then mirroring must not give
    /// the same answer as mirroring then rotating, which is the property that makes the
    /// order worth pinning down at all.
    #[test]
    fn the_two_transform_orders_really_do_differ() {
        let pivot = BlockPos::new(0, 0, 0);
        let point = BlockPos::new(2, 0, 1);
        let rotate = Transform::RotateY(-90.0);
        let mirror = Transform::Flip(BlockAxis::X);

        let rotate_then_mirror = rotate.clone().then(mirror.clone());
        let mirror_then_rotate = mirror.then(rotate);

        assert_ne!(
            rotate_then_mirror.paste_position(point, pivot),
            mirror_then_rotate.paste_position(point, pivot),
            "a quarter turn and a mirror do not commute, so order is observable"
        );
    }

    /// The staircase states the paste test needs, one for each cardinal facing. A
    /// state id is the index, so a turned paste comes back as the id of the facing
    /// it was turned to.
    const FACINGS: &[&str] = &["north", "east", "south", "west"];

    /// A registry over [`FACINGS`], naming its property the way the server does.
    struct Stairs;

    impl crate::block::BlockResolver for Stairs {
        fn resolve(&self, name: &str, properties: &[(String, String)]) -> Option<u16> {
            if name != "minecraft:oak_stairs" {
                return None;
            }
            let facing = properties.iter().find(|(key, _)| key == "facing")?;
            FACINGS
                .iter()
                .position(|value| *value == facing.1)
                .map(|index| index as u16)
        }

        fn info(&self, state: u16) -> Option<crate::block::BlockStateInfo> {
            FACINGS
                .get(state as usize)
                .map(|facing| crate::block::BlockStateInfo {
                    name: "minecraft:oak_stairs".to_string(),
                    properties: vec![("facing".to_string(), (*facing).to_string())],
                })
        }

        fn is_air(&self, _state: u16) -> bool {
            false
        }

        fn is_solid(&self, _state: u16) -> bool {
            true
        }

        fn is_liquid(&self, _state: u16) -> bool {
            false
        }

        fn block_name(&self, _state: u16) -> Option<&str> {
            Some("minecraft:oak_stairs")
        }
    }

    /// The destination of a paste is already turned, but so has to be the block:
    /// a stair copied facing north comes out of `//rotate 90` facing east, or the
    /// copy is sideways and the wrong way round.
    #[test]
    fn a_rotated_paste_turns_the_copied_blocks() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
        let source = Grid::from_states(region, vec![0]);
        let clipboard = Clipboard::capture(&source, region, BlockPos::new(0, 0, 0));

        let steps =
            clipboard.paste_steps_with(&Stairs, &Transform::RotateY(-90.0), BlockPos::new(0, 0, 0));

        assert_eq!(steps.len(), 1);
        let (_, block) = &steps[0];
        assert_eq!(
            Stairs
                .info(block.state)
                .expect("the pasted stair is a state the registry knows")
                .properties,
            vec![("facing".to_string(), "east".to_string())],
            "//rotate 90 has to turn the copied stair, not only its position"
        );
    }

    /// The same paste through the resolver-free path, which is what the callers
    /// that cannot reach the registry still use. Without one there is nothing to
    /// resolve a turned property against, so the block is copied as it was.
    #[test]
    fn a_paste_without_a_registry_copies_the_block_as_it_was() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
        let source = Grid::from_states(region, vec![0]);
        let clipboard = Clipboard::capture(&source, region, BlockPos::new(0, 0, 0));

        let steps = clipboard.paste_steps(&Transform::RotateY(-90.0), BlockPos::new(0, 0, 0));

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].1, source.get_block(BlockPos::new(0, 0, 0)));
    }
}
