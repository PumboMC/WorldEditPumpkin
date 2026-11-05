//! Moving and stacking a selection's own contents.
//!
//! `//move` and `//stack` are *region* operations in FAWE — `RegionCommands` passes
//! `@Selection Region` to both — even though the names suggest a clipboard. This is easy
//! to get wrong and was got wrong here first time round: an early version moved and
//! stacked the clipboard, which is a different command (`//stack` of a clipboard is
//! `//paste` repeated, and FAWE has no `//move`-the-clipboard command at all).
//!
//! ## Why the copy is buffered
//!
//! Both operations copy a region onto a *shifted copy of itself*, so source and
//! destination can overlap. FAWE's `ForwardExtentCopy` enables its queue when the
//! displacement is smaller than the region (`disAbs.x() < size.x() && ...`) for exactly
//! this reason. Reading every source block before writing any one of them gives the same
//! result without the machinery, and is what [`move_pairs`] and [`stack_pairs`] encode by
//! returning pairs rather than writing.

use crate::math::BlockPos;
use crate::region::CuboidRegion;

/// One block travelling from one place to another.
///
/// `(to, from)`: the value at `from` is written to `to`. A pair rather than two lists
/// because the two have to stay together — a caller that zips two independently-shaped
/// lists would silently read a shifted block out of the wrong place when the region's
/// own ordering differs from the iteration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    /// Where the block is written.
    pub to: BlockPos,
    /// Where the block is read from.
    pub from: BlockPos,
}

/// The positions of `region`, in the engine's own cuboid order.
///
/// The order is y, then z, then x, which is the order the clipboard and the paste iterate
/// in. Keeping it means a caller reading these in order touches the world in the same
/// sequence a paste would, so a shift or a mask sees the same sequence either way.
#[must_use]
pub fn positions(region: CuboidRegion) -> Vec<BlockPos> {
    let minimum = region.minimum();
    let maximum = region.maximum();
    let mut out = Vec::new();
    for y in minimum.y..=maximum.y {
        for z in minimum.z..=maximum.z {
            for x in minimum.x..=maximum.x {
                out.push(BlockPos::new(x, y, z));
            }
        }
    }
    out
}

/// Moving `region` by `displacement`: where each block goes.
///
/// This is `EditSession.moveRegion`. Each block in the region travels the whole
/// displacement, so `//move 5 up` lifts every block of the region by 5 and leaves air
/// behind at the original site. The caller writes the replacement afterwards, because
/// what goes in the hole is a pattern and patterns are the caller's business.
///
/// A displacement of zero is returned as an empty list rather than as every block
/// "moving" onto itself, which would be a no-op that still reported having done work.
#[must_use]
pub fn move_pairs(region: CuboidRegion, displacement: BlockPos) -> Vec<Pair> {
    if displacement == BlockPos::new(0, 0, 0) {
        return Vec::new();
    }
    positions(region)
        .into_iter()
        .map(|from| Pair {
            to: from.offset(displacement.x, displacement.y, displacement.z),
            from,
        })
        .collect()
}

/// Stacking `region` `count` times along `offset`: where each block goes.
///
/// This is `EditSession.stackRegion`, which sets `ForwardExtentCopy.setRepetitions(count)`
/// and a translate transform, so each repetition reads the *original* region and writes
/// `offset` further along. Every pair therefore carries the same `from`, and the count is
/// the number of times the same source is re-read.
///
/// The copies are required not to overlap: FAWE throws
/// `worldedit.stack.intersecting-region` when `offset.abs() < region.size`, because a copy
/// overlapping the one before it would need the copy ordered carefully to avoid reading
/// its own writes, and a stack that depends on iteration order is not a stack. This
/// returns an empty list instead, which the caller reports as the refusal.
#[must_use]
pub fn stack_pairs(region: CuboidRegion, offset: BlockPos, count: i32) -> Vec<Pair> {
    if count < 1 {
        return Vec::new();
    }
    let minimum = region.minimum();
    let maximum = region.maximum();
    let size = BlockPos::new(
        maximum.x - minimum.x + 1,
        maximum.y - minimum.y + 1,
        maximum.z - minimum.z + 1,
    );
    let displaced = BlockPos::new(offset.x.abs(), offset.y.abs(), offset.z.abs());
    if displaced.x < size.x && displaced.y < size.y && displaced.z < size.z {
        return Vec::new();
    }
    let source = positions(region);
    let mut out = Vec::with_capacity(source.len() * count as usize);
    // The copies sit at `offset*1 ..= offset*count`, not `offset*0 .. offset*(count-1)`.
    // FAWE seeds `currentTransform` with the translate BEFORE `ForwardExtentCopy.resume`'s
    // repetition loop (`currentTransform = transform;` at :310-312), wraps the destination in
    // a `PositionTransformExtent` carrying it (:361-363), and bumps it only AFTER each
    // repetition (:431-458) — so repetition index `i` writes at `offset*(i+1)`. Corroborated
    // by RegionCommands.java:693-694, where the `-s` shift is `shiftSize.multiply(count)`:
    // the last copy is a whole region away, not sitting on the source.
    //
    // With `0..count` the first copy rewrote the source region and the last copy was a full
    // region short of where FAWE puts it.
    for step in 1..=count {
        let shift = BlockPos::new(offset.x * step, offset.y * step, offset.z * step);
        for from in &source {
            out.push(Pair {
                to: from.offset(shift.x, shift.y, shift.z),
                from: *from,
            });
        }
    }
    out
}

/// Whether stacking a region by `offset` would make the copies overlap.
///
/// Kept separate from [`stack_pairs`] because the caller has to say *why* it did nothing,
/// and "the copies would overlap" is a different answer from "you asked for zero copies"
/// even though both arrive as an empty list.
#[must_use]
pub fn would_stack_overlap(region: CuboidRegion, offset: BlockPos) -> bool {
    let minimum = region.minimum();
    let maximum = region.maximum();
    let size = BlockPos::new(
        maximum.x - minimum.x + 1,
        maximum.y - minimum.y + 1,
        maximum.z - minimum.z + 1,
    );
    let displaced = BlockPos::new(offset.x.abs(), offset.y.abs(), offset.z.abs());
    displaced.x < size.x && displaced.y < size.y && displaced.z < size.z
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube(x: i32, y: i32, z: i32, size: i32) -> CuboidRegion {
        CuboidRegion::new(
            BlockPos::new(x, y, z),
            BlockPos::new(x + size - 1, y + size - 1, z + size - 1),
        )
    }

    /// `//move 2 up` lifts every block by 2, which is the whole command.
    #[test]
    fn moving_lifts_every_block_by_the_displacement() {
        let region = cube(0, 0, 0, 2);
        let pairs = move_pairs(region, BlockPos::new(0, 2, 0));
        assert_eq!(pairs.len(), 8, "every block of a 2x2x2 region travels");
        assert!(
            pairs
                .iter()
                .all(|pair| pair.to.y == pair.from.y + 2 && pair.from == pair.to.offset(0, -2, 0))
        );
    }

    /// The source is the whole region, even when the destination overlaps it.
    ///
    /// A caller filling the hole left behind has to fill the *entire* source region, not
    /// just the blocks that moved out of it. Deriving the hole from the pair list would
    /// leave a block behind wherever source and destination overlapped — which is the
    /// common case, because `//move 1 up` on a tall selection overlaps almost everywhere.
    #[test]
    fn moving_reports_every_source_block_even_when_it_overlaps() {
        let region = cube(0, 64, 0, 3);
        let pairs = move_pairs(region, BlockPos::new(1, 0, 0));
        assert_eq!(pairs.len(), 27, "a 3x3x3 region moves all 27 of its blocks");
        // Every `from` is distinct, so no block is read twice and none is skipped.
        let mut sources: Vec<BlockPos> = pairs.iter().map(|pair| pair.from).collect();
        sources.sort_unstable();
        sources.dedup();
        assert_eq!(
            sources.len(),
            27,
            "each block of the region is a source exactly once"
        );
        // 18 of them land back inside the region's own x range, which is the overlap.
        assert_eq!(
            pairs.iter().filter(|pair| pair.to.x < 3).count(),
            18,
            "a 1-block shift in x re-enters the region for two of the three columns"
        );
    }

    /// A displacement of zero is nothing, not every block onto itself.
    #[test]
    fn moving_nowhere_moves_nothing() {
        let region = cube(0, 0, 0, 3);
        assert!(move_pairs(region, BlockPos::new(0, 0, 0)).is_empty());
    }

    /// `//move` has to lift the block that was on top to the top, not to the hole's
    /// floor.
    ///
    /// This is the test that catches reading the destination instead of the source: a
    /// caller that reads `pair.to` while iterating a region that overlaps its own
    /// destination would read blocks the earlier steps already wrote, and the result
    /// would be the block *below* the one being copied.
    #[test]
    fn a_moved_block_carries_its_own_value_and_not_one_already_written() {
        let region = cube(0, 64, 0, 2);
        let pairs = move_pairs(region, BlockPos::new(0, 1, 0));
        // Reading in the engine's own order, the first pair's source is the floor and
        // its destination is one above it. If the caller read the destination it would
        // take the block already there.
        assert_eq!(pairs[0].from, BlockPos::new(0, 64, 0));
        assert_eq!(pairs[0].to, BlockPos::new(0, 65, 0));
        assert_eq!(pairs[2].from, BlockPos::new(0, 64, 1));
        assert_eq!(pairs[2].to, BlockPos::new(0, 65, 1));
    }

    /// The `//move` hole is the whole source region, which is what the caller fills.
    ///
    /// A caller that walked the pairs to find what to fill would miss every block whose
    /// destination still landed inside the region, which is most of them for a small
    /// displacement — and would leave the selection half-emptied.
    #[test]
    fn the_hole_left_behind_is_the_whole_region() {
        let region = cube(0, 64, 0, 3);
        let hole = positions(region);
        let moved = move_pairs(region, BlockPos::new(0, 1, 0));
        // Every source position appears in the hole, and the hole is not derived from
        // the pairs.
        for pair in &moved {
            assert!(
                hole.contains(&pair.from),
                "{} is in the moved set but not in the hole",
                pair.from.y
            );
        }
        assert_eq!(hole.len(), 27, "a 3x3x3 selection leaves 27 blocks of hole");
    }

    /// The blocks are read in the engine's own order, so a caller reading them in order
    /// touches the world the way a paste would.
    #[test]
    fn the_pairs_are_in_the_engines_own_cuboid_order() {
        let region = cube(0, 0, 0, 2);
        let pairs = move_pairs(region, BlockPos::new(0, 1, 0));
        let order: Vec<[i32; 3]> = pairs
            .iter()
            .map(|pair| [pair.from.y, pair.from.z, pair.from.x])
            .collect();
        // y slowest, then z, then x: the same order the clipboard iterates in.
        assert_eq!(
            order,
            vec![
                [0, 0, 0],
                [0, 0, 1],
                [0, 1, 0],
                [0, 1, 1],
                [1, 0, 0],
                [1, 0, 1],
                [1, 1, 0],
                [1, 1, 1]
            ]
        );
    }

    /// Stacking by more than the region's size makes non-overlapping copies.
    #[test]
    fn stacking_by_the_whole_region_makes_separate_copies() {
        let region = cube(0, 0, 0, 2);
        let pairs = stack_pairs(region, BlockPos::new(2, 0, 0), 3);
        assert_eq!(pairs.len(), 24, "three copies of eight blocks");
        // Every third pair reads the same source.
        assert_eq!(pairs[0].from, pairs[8].from, "copy 2 re-reads the original");
        assert_eq!(
            pairs[8].from, pairs[16].from,
            "copy 3 re-reads the original"
        );
        assert_ne!(pairs[0].to, pairs[8].to, "but lands somewhere else");
    }

    /// Stacking by less than the region's size would overlap, and FAWE refuses.
    ///
    /// This is the case that makes an ordered copy necessary: the second copy would read
    /// blocks the first copy just wrote and stack them again, growing every time.
    #[test]
    fn stacking_a_sub_region_offset_is_refused() {
        let region = cube(0, 0, 0, 4);
        assert!(would_stack_overlap(region, BlockPos::new(2, 0, 0)));
        assert!(stack_pairs(region, BlockPos::new(2, 0, 0), 3).is_empty());
    }

    /// Stacking by exactly the region's size is allowed and does not overlap.
    #[test]
    fn stacking_by_exactly_the_region_size_is_allowed() {
        let region = cube(0, 0, 0, 4);
        assert!(!would_stack_overlap(region, BlockPos::new(4, 0, 0)));
        // 4x4x4 is 64 blocks and two copies is 128 pairs.
        assert_eq!(stack_pairs(region, BlockPos::new(4, 0, 0), 2).len(), 128);
    }

    /// A count of zero or less is nothing, not an error.
    #[test]
    fn stacking_zero_copy_stacks_nothing() {
        let region = cube(0, 0, 0, 4);
        assert!(stack_pairs(region, BlockPos::new(4, 0, 0), 0).is_empty());
        assert!(stack_pairs(region, BlockPos::new(4, 0, 0), -3).is_empty());
    }

    /// The copies are laid out by whole multiples of the offset from the original, so they
    /// do not drift apart as the count rises, and the FIRST copy is already a whole
    /// region away rather than sitting on top of the source.
    #[test]
    fn the_copies_do_not_accumulate_drift() {
        let region = cube(0, 0, 0, 2);
        let pairs = stack_pairs(region, BlockPos::new(0, 2, 0), 4);
        // FAWE places repetition `i` at `offset*(i+1)`, so copies live at 2, 4, 6 and 8.
        for step in 1..=4usize {
            let expected_y = step as i32 * 2;
            let copy = &pairs[(step - 1) * 8..step * 8];
            assert!(
                copy.iter()
                    .all(|pair| pair.to.y - pair.from.y == expected_y),
                "copy {step} must sit exactly {expected_y} above the original"
            );
        }

        // The original bug: with `0..count` the first copy rewrote the source, so no
        // pair ever landed on its own block.
        assert!(
            pairs.iter().all(|pair| pair.to.y != pair.from.y),
            "no copy may sit on the source it came from"
        );
    }
}
