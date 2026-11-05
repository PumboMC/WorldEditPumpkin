//! The edit job: applying a pattern over positions, recording what it changed.
//!
//! FAWE's whole point is that an edit is queued and spread over time rather than
//! blocking the server tick, so [`EditJob`] is resumable. A command hands it a
//! position source and a block budget; each tick it spends up to the budget and
//! returns whether it is finished. Undo and redo run through the same job, which
//! is what keeps their block-change limits identical.

use crate::{
    block::{BlockResolver, BlockValue},
    extent::Extent,
    history::{Change, ChangeSet},
    mask::{Mask, MaskContext},
    math::BlockPos,
    pattern::Pattern,
};

/// Why a job stopped before the world ran out of positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The job covered every position.
    Finished,
    /// The caller's block budget ran out. Call again to continue.
    Budget,
    /// The edit would have changed more blocks than the caller allows.
    TooManyBlocks,
}

/// A resumable history replay that retains refused changes for retry.
pub struct HistoryReplay {
    pending: std::vec::IntoIter<Change>,
    undo: bool,
    refused: ChangeSet,
    applied: ChangeSet,
}

impl HistoryReplay {
    /// Prepare one history entry for replay.
    pub fn new(changes: ChangeSet, undo: bool) -> Self {
        let mut refused = ChangeSet::new();
        let mut applied = ChangeSet::new();
        if let Some((level, dimension)) = changes.world() {
            refused.set_world(level.to_owned(), dimension.to_owned());
            applied.set_world(level.to_owned(), dimension.to_owned());
        }
        Self {
            pending: changes.changes().to_vec().into_iter(),
            undo,
            refused,
            applied,
        }
    }

    /// Process at most `budget` changes, including refusals and no-ops.
    pub fn step(&mut self, extent: &mut dyn Extent, budget: usize) -> StopReason {
        let mut spent = 0;
        for change in self.pending.by_ref().take(budget) {
            spent += 1;
            let target = if self.undo {
                change.original.clone()
            } else {
                change
                    .replacement
                    .clone()
                    .unwrap_or_else(|| BlockValue::new(extent.air_state()))
            };
            let before = extent.get_block(change.position);
            if before == target {
                continue;
            }
            if !extent.try_set_block(change.position, &target) {
                self.refused.push_change(change);
            } else if self.undo {
                self.applied.push(change.position, target, before);
            } else {
                self.applied.push(change.position, before, target);
            }
        }
        if spent == budget {
            StopReason::Budget
        } else {
            StopReason::Finished
        }
    }

    /// Return changes that could not be applied and the inverse of successful writes.
    pub fn into_results(self) -> (ChangeSet, ChangeSet) {
        (self.refused, self.applied)
    }
}

/// An edit in progress.
pub struct EditJob<'a> {
    positions: Box<dyn Iterator<Item = BlockPos> + Send + 'a>,
    pattern: Pattern,
    /// When set, only positions this accepts are edited. This is the difference
    /// between `//set` and `//replace`.
    mask: Option<Mask>,
    changes: ChangeSet,
    air: BlockValue,
    /// The most blocks this job may change before giving up.
    limit: Option<usize>,
}

impl<'a> EditJob<'a> {
    /// A job that writes `pattern` at every position `positions` yields.
    ///
    /// The iterator is `Send` because a budgeted edit outlives the call that started
    /// it: the caller keeps the job in a queue shared across threads and steps it a
    /// tick at a time.
    pub fn new(
        positions: Box<dyn Iterator<Item = BlockPos> + Send + 'a>,
        pattern: Pattern,
    ) -> Self {
        Self {
            positions,
            pattern,
            mask: None,
            changes: ChangeSet::new(),
            air: BlockValue::default(),
            limit: None,
        }
    }

    /// Restricts the edit to positions `mask` accepts, which makes this a
    /// `//replace`.
    #[must_use]
    pub fn with_mask(mut self, mask: Mask) -> Self {
        self.mask = Some(mask);
        self
    }

    /// Refuses to change more than `limit` blocks.
    ///
    /// The limit counts blocks actually changed, not positions visited, so
    /// `//replace stone,dirt 100` on a selection of a million with 50 dirt blocks
    /// succeeds. That is the `worldedit.edit.block-limit` behaviour.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Runs the job to completion.
    ///
    /// One `step` with an unlimited budget is enough: the position source is
    /// finite, so the step ends on [`StopReason::Finished`] rather than
    /// [`StopReason::Budget`]. Looping here would spin forever, because calling
    /// `step` again over an exhausted iterator also reports `Finished`.
    ///
    /// Only for positions that are already in memory, such as a shape command's
    /// positions. A selection-sized edit has to be driven by [`EditJob::step`]
    /// instead.
    pub fn run(mut self, extent: &mut dyn Extent, resolver: &dyn BlockResolver) -> ChangeSet {
        self.step(extent, resolver, usize::MAX);
        self.changes
    }

    /// Changes up to `budget` blocks, then reports why it stopped.
    pub fn step(
        &mut self,
        extent: &mut dyn Extent,
        resolver: &dyn BlockResolver,
        budget: usize,
    ) -> StopReason {
        self.air = BlockValue::new(extent.air_state());
        let mut spent = 0usize;

        for position in self.positions.by_ref().take(budget) {
            spent += 1;
            if let Some(mask) = &self.mask {
                // The context borrows the extent immutably and the write below
                // needs it mutably, so it is scoped to its own statement rather
                // than holding a context across the loop. That is no longer a
                // question of what the mask remembers: `Radius` and `Axis` latch
                // their origin in the mask, as `RadiusMask` and `XAxisMask` do in
                // Java, so the one job's mask keeps one origin from its first
                // position to its last, across every step of every tick.
                let accepted = {
                    let mut context = MaskContext::new(extent, resolver);
                    mask.test(&mut context, position)
                };
                if !accepted {
                    continue;
                }
            }

            // The limit is checked BEFORE the write, not after. Java's `LimitExtent.setBlock`
            // calls `limit.THROW_MAX_CHANGES()` first and `FaweLimit` throws on
            // `decrementAndGet() < 0`, so with a limit of N the writes 1..N succeed and
            // write N+1 is refused before it happens. Testing afterwards wrote one block
            // more than the player asked for, so `//limit 4` changed 5.
            let replacement = self.pattern.next(position);
            let original = extent.get_block(position);
            if original == replacement {
                continue;
            }
            if let Some(limit) = self.limit
                && self.changes.affected() >= limit
            {
                return StopReason::TooManyBlocks;
            }
            if !extent.try_set_block(position, &replacement) {
                continue;
            }
            self.changes.push(position, original, replacement.clone());
        }
        if spent == budget {
            StopReason::Budget
        } else {
            StopReason::Finished
        }
    }

    /// How many blocks the job has changed so far.
    #[must_use]
    pub fn affected(&self) -> usize {
        self.changes.affected()
    }

    /// A copy of what the job has recorded so far.
    ///
    /// Copying rather than consuming, because a budgeted edit is stepped over
    /// several ticks and the caller only wants the change set once the job has
    /// actually finished.
    #[must_use]
    pub fn changes(&self) -> ChangeSet {
        self.changes.clone()
    }

    /// Takes the recorded changes, leaving the job empty.
    #[must_use]
    pub fn into_changes(self) -> ChangeSet {
        self.changes
    }
}

impl StopReason {
    /// Whether the job has nothing left to do.
    #[must_use]
    pub fn finished(&self) -> bool {
        !matches!(self, Self::Budget)
    }
}

/// A paste in progress: writing captured blocks rather than a pattern.
///
/// Separate from [`EditJob`] because the block written at a position is not a
/// function of the position alone — it comes from wherever the transform puts the
/// clipboard's copy. FAWE models this the same way, as `ForwardExtentCopy` beside
/// the pattern-based operations, and keeping the two apart means the block limit
/// and the history recording are implemented once each rather than shared through
/// a flag.
pub struct PasteJob<'a> {
    steps: Box<dyn Iterator<Item = (BlockPos, BlockValue)> + Send + 'a>,
    changes: ChangeSet,
    limit: Option<usize>,
}

impl<'a> PasteJob<'a> {
    /// A paste that writes each `(destination, block)` pair in turn.
    ///
    /// `Send` for the same reason as [`EditJob::new`]: a paste can outlive the tick
    /// that started it.
    #[must_use]
    pub fn new(steps: Box<dyn Iterator<Item = (BlockPos, BlockValue)> + Send + 'a>) -> Self {
        Self {
            steps,
            changes: ChangeSet::new(),
            limit: None,
        }
    }

    /// A paste built from a clipboard, its transform and where to put it.
    ///
    /// `shift` moves the clipboard's origin to the new anchor, so pasting at the
    /// position the copy was taken from puts it back where it was.
    #[must_use]
    pub fn from_clipboard(
        clipboard: &crate::clipboard::Clipboard,
        transform: &crate::transform::Transform,
        shift: BlockPos,
    ) -> Self {
        let steps = clipboard.paste_steps(transform, shift).into_iter();
        Self::new(Box::new(steps))
    }

    /// Refuses to change more than `limit` blocks.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Writes up to `budget` blocks, then reports why it stopped.
    pub fn step(&mut self, extent: &mut dyn Extent, budget: usize) -> StopReason {
        let mut spent = 0usize;
        for (position, block) in self.steps.by_ref().take(budget) {
            spent += 1;
            // Same rule as `EditJob::step`: the limit is tested before the write, so a
            // paste stops at exactly the limit rather than one block past it.
            let original = extent.get_block(position);
            if original == block {
                continue;
            }
            if let Some(limit) = self.limit
                && self.changes.affected() >= limit
            {
                return StopReason::TooManyBlocks;
            }
            if !extent.try_set_block(position, &block) {
                continue;
            }
            self.changes.push(position, original, block.clone());
        }
        if spent == budget {
            StopReason::Budget
        } else {
            StopReason::Finished
        }
    }

    /// How many blocks the paste has changed so far.
    #[must_use]
    pub fn affected(&self) -> usize {
        self.changes.affected()
    }

    /// Runs the paste to completion.
    ///
    /// A paste's blocks are all in memory already — they came out of the clipboard
    /// rather than off a walk — so there is nothing to resume and this is always a
    /// single pass.
    pub fn run(mut self, extent: &mut dyn Extent) -> ChangeSet {
        self.step(extent, usize::MAX);
        self.changes
    }

    /// A copy of what the paste has recorded so far.
    #[must_use]
    pub fn changes(&self) -> ChangeSet {
        self.changes.clone()
    }

    /// Takes the recorded changes, leaving the job empty.
    #[must_use]
    pub fn into_changes(self) -> ChangeSet {
        self.changes
    }
}

/// Builds the `(destination, block)` pairs for a paste, without editing anything.
///
/// A thin wrapper over [`Clipboard::paste_steps`](crate::clipboard::Clipboard::paste_steps),
/// which is where the mapping lives and where its tests are. It stays here because
/// the edit module is where a caller reaches for paste behaviour.
#[must_use]
pub fn paste_steps(
    clipboard: &crate::clipboard::Clipboard,
    transform: &crate::transform::Transform,
    shift: BlockPos,
) -> Vec<(BlockPos, BlockValue)> {
    clipboard.paste_steps(transform, shift)
}

/// Replays a recorded change set, for `//undo` and `//redo`.
pub struct ReplayJob<'a> {
    steps: Box<dyn Iterator<Item = (BlockPos, BlockValue, bool)> + 'a>,
    changes: ChangeSet,
    written: usize,
}

impl<'a> ReplayJob<'a> {
    /// A job that writes the given `(position, block, clears_block_entity)` steps.
    #[must_use]
    pub fn new(steps: Box<dyn Iterator<Item = (BlockPos, BlockValue, bool)> + 'a>) -> Self {
        Self {
            steps,
            changes: ChangeSet::new(),
            written: 0,
        }
    }

    /// Runs the replay to completion.
    pub fn run(mut self, extent: &mut dyn Extent) -> ChangeSet {
        self.step(extent, usize::MAX);
        self.changes
    }

    /// Writes up to `budget` blocks, then reports why it stopped.
    pub fn step(&mut self, extent: &mut dyn Extent, budget: usize) -> StopReason {
        self.written = 0;
        for (position, block, clears_block_entity) in self.steps.by_ref() {
            let original = extent.get_block(position);
            // A replay clears the block entity when the change it is undoing was a
            // removal, because writing air over a chest does not by itself remove
            // the chest's contents.
            let replacement = if clears_block_entity && block.entity.is_none() {
                BlockValue::new(block.state)
            } else {
                block.clone()
            };
            if !extent.try_set_block(position, &replacement) {
                self.written += 1;
                if self.written >= budget {
                    return StopReason::Budget;
                }
                continue;
            }
            self.changes.push(position, original, replacement.clone());

            self.written += 1;
            if self.written >= budget {
                return StopReason::Budget;
            }
        }
        StopReason::Finished
    }

    /// Takes the changes the replay itself recorded, which is the set that undoes
    /// it again.
    #[must_use]
    pub fn into_changes(self) -> ChangeSet {
        self.changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        block::BlockStateInfo,
        region::{CuboidRegion, Region},
        transform::Transform,
    };

    const AIR: u16 = 9;

    #[test]
    fn history_replay_preserves_world_and_retries_only_refused_writes() {
        struct GuardedWorld {
            blocks: [u16; 2],
            deny_second: bool,
        }
        impl Extent for GuardedWorld {
            fn get_block(&self, pos: BlockPos) -> BlockValue {
                BlockValue::new(self.blocks[pos.x as usize])
            }
            fn set_block(&mut self, pos: BlockPos, block: &BlockValue) {
                self.blocks[pos.x as usize] = block.state;
            }
            fn try_set_block(&mut self, pos: BlockPos, block: &BlockValue) -> bool {
                if self.deny_second && pos.x == 1 {
                    return false;
                }
                self.set_block(pos, block);
                true
            }
            fn min_y(&self) -> i32 {
                0
            }
            fn max_y(&self) -> i32 {
                319
            }
            fn air_state(&self) -> u16 {
                AIR
            }
        }
        let mut history = crate::history::History::new(10);
        let mut first = ChangeSet::new();
        first.set_world("level".into(), "overworld".into());
        first.push(
            BlockPos::new(0, 0, 0),
            BlockValue::new(AIR),
            BlockValue::new(1),
        );
        history.remember(first);
        let mut second = ChangeSet::new();
        second.set_world("level".into(), "nether".into());
        for x in 0..2 {
            second.push(
                BlockPos::new(x, 0, 0),
                BlockValue::new(AIR),
                BlockValue::new(2),
            );
        }
        history.remember(second);
        let entry = history.begin_replay(true).unwrap();
        assert_eq!(entry.world(), Some(("level", "nether")));
        let mut world = GuardedWorld {
            blocks: [2, 2],
            deny_second: true,
        };
        let mut replay = HistoryReplay::new(entry, true);
        assert_eq!(replay.step(&mut world, 1), StopReason::Budget);
        assert_eq!(world.blocks, [AIR, 2]);
        assert_eq!(replay.step(&mut world, 1), StopReason::Budget);
        assert_eq!(replay.step(&mut world, 1), StopReason::Finished);
        let (refused, applied) = replay.into_results();
        assert_eq!((refused.affected(), applied.affected()), (1, 1));
        assert_eq!(refused.world(), Some(("level", "nether")));
        history.finish_replay(true, refused, applied);
        assert_eq!((history.undo_depth(), history.redo_depth()), (2, 1));

        world.deny_second = false;
        let mut retry = HistoryReplay::new(history.begin_replay(true).unwrap(), true);
        assert_eq!(retry.step(&mut world, 10), StopReason::Finished);
        let (refused, applied) = retry.into_results();
        assert!(refused.is_empty());
        history.finish_replay(true, refused, applied);
        assert_eq!(world.blocks, [AIR, AIR]);
        let overworld = history.begin_replay(true).unwrap();
        assert_eq!(overworld.world(), Some(("level", "overworld")));
        let mut redo = HistoryReplay::new(history.begin_replay(false).unwrap(), false);
        assert_eq!(redo.step(&mut world, 10), StopReason::Finished);
        assert_eq!(world.blocks, [AIR, 2]);
    }

    #[test]
    fn skipped_edit_and_paste_positions_consume_budget() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(4, 0, 0));
        let mut world = World::air(region);
        let mut edit = EditJob::new(selection(region), pattern("air"));
        for _ in 0..5 {
            assert_eq!(edit.step(&mut world, &TestBlocks, 1), StopReason::Budget);
        }
        assert_eq!(edit.step(&mut world, &TestBlocks, 1), StopReason::Finished);

        let mut masked = EditJob::new(selection(region), pattern("stone")).with_mask(mask("dirt"));
        for _ in 0..5 {
            assert_eq!(masked.step(&mut world, &TestBlocks, 1), StopReason::Budget);
        }
        assert_eq!(
            masked.step(&mut world, &TestBlocks, 1),
            StopReason::Finished
        );

        let positions = (0..5).map(|x| (BlockPos::new(x, 0, 0), BlockValue::new(AIR)));
        let mut paste = PasteJob::new(Box::new(positions));
        for _ in 0..5 {
            assert_eq!(paste.step(&mut world, 1), StopReason::Budget);
        }
        assert_eq!(paste.step(&mut world, 1), StopReason::Finished);
    }

    #[test]
    fn refused_writes_never_enter_edit_paste_or_replay_history() {
        struct Denied;
        impl Extent for Denied {
            fn get_block(&self, _: BlockPos) -> BlockValue {
                BlockValue::new(AIR)
            }
            fn set_block(&mut self, _: BlockPos, _: &BlockValue) {
                panic!("refused write reached storage");
            }
            fn try_set_block(&mut self, _: BlockPos, _: &BlockValue) -> bool {
                false
            }
            fn min_y(&self) -> i32 {
                0
            }
            fn max_y(&self) -> i32 {
                319
            }
            fn air_state(&self) -> u16 {
                AIR
            }
        }
        let position = BlockPos::new(1, 64, 1);
        let pattern = crate::pattern::parse("stone", &TestBlocks).unwrap();
        let changes =
            EditJob::new(Box::new([position].into_iter()), pattern).run(&mut Denied, &TestBlocks);
        assert!(changes.is_empty());
        let changes =
            PasteJob::new(Box::new([(position, BlockValue::new(1))].into_iter())).run(&mut Denied);
        assert!(changes.is_empty());
        let changes = ReplayJob::new(Box::new(
            [(position, BlockValue::new(1), false)].into_iter(),
        ))
        .run(&mut Denied);
        assert!(changes.is_empty());
        let pattern = crate::pattern::parse("stone", &TestBlocks).unwrap();
        let mut job = EditJob::new(Box::new([position, position].into_iter()), pattern);
        assert_eq!(job.step(&mut Denied, &TestBlocks, 1), StopReason::Budget);
        assert_eq!(job.step(&mut Denied, &TestBlocks, 1), StopReason::Budget);
        assert_eq!(job.step(&mut Denied, &TestBlocks, 1), StopReason::Finished);
    }

    struct TestBlocks;

    impl BlockResolver for TestBlocks {
        fn resolve(&self, name: &str, _properties: &[(String, String)]) -> Option<u16> {
            Some(match name {
                "stone" => 1,
                "dirt" => 2,
                "air" => AIR,
                _ => return None,
            })
        }

        fn info(&self, _state: u16) -> Option<BlockStateInfo> {
            None
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

        fn block_name(&self, state: u16) -> Option<&str> {
            Some(match state {
                1 => "stone",
                2 => "dirt",
                AIR => "air",
                _ => "other",
            })
        }
    }

    /// A world holding `air` everywhere, so a test can watch what an edit put in.
    struct World {
        region: CuboidRegion,
        blocks: Vec<BlockValue>,
    }

    impl World {
        fn air(region: CuboidRegion) -> Self {
            Self {
                region,
                blocks: vec![BlockValue::new(AIR); region.dimensions().volume()],
            }
        }

        fn index(&self, position: BlockPos) -> usize {
            let dimensions = self.region.dimensions();
            let minimum = self.region.minimum();
            let offset = position.offset(-minimum.x, -minimum.y, -minimum.z);
            offset.x as usize
                + offset.y as usize * (dimensions.x * dimensions.z) as usize
                + offset.z as usize * dimensions.x as usize
        }
    }

    impl Extent for World {
        fn get_block(&self, position: BlockPos) -> BlockValue {
            self.blocks
                .get(self.index(position))
                .cloned()
                .unwrap_or_default()
        }

        fn set_block(&mut self, position: BlockPos, block: &BlockValue) {
            let index = self.index(position);
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
            AIR
        }
    }

    fn pattern(text: &str) -> Pattern {
        crate::pattern::parse(text, &TestBlocks).expect("the pattern should parse")
    }

    fn mask(text: &str) -> Mask {
        crate::mask::parse(text, &TestBlocks, (-64, 319)).expect("the mask should parse")
    }

    fn selection(region: CuboidRegion) -> Box<dyn Iterator<Item = BlockPos> + Send> {
        Box::new(crate::region::RegionWalker::new(region.boxed()))
    }

    #[test]
    fn a_finished_job_reports_every_block_it_changed() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 0, 0));
        let mut world = World::air(region);
        let job = EditJob::new(selection(region), pattern("stone"));
        let changes = job.run(&mut world, &TestBlocks);
        assert_eq!(changes.affected(), 3);
        assert_eq!(world.get_block(BlockPos::new(1, 0, 0)).state, 1);
    }

    /// A no-op replacement must not be recorded, or `//undo` on a selection that
    /// was already stone would put blocks back that never moved.
    #[test]
    fn writing_the_block_that_is_already_there_is_not_a_change() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
        let mut world = World::air(region);
        world.set_block(BlockPos::new(0, 0, 0), &BlockValue::new(1));

        let job = EditJob::new(selection(region), pattern("stone"));
        assert_eq!(job.run(&mut world, &TestBlocks).affected(), 0);
    }

    /// A mask turns a set into a replace, and the affected count has to reflect
    /// only the positions that passed.
    #[test]
    fn a_mask_limits_which_positions_are_written() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 0, 0));
        let mut world = World::air(region);
        world.set_block(BlockPos::new(1, 0, 0), &BlockValue::new(2));

        let job = EditJob::new(selection(region), pattern("stone")).with_mask(mask("dirt"));
        let changes = job.run(&mut world, &TestBlocks);
        assert_eq!(changes.affected(), 1);
        assert_eq!(world.get_block(BlockPos::new(0, 0, 0)).state, AIR);
        assert_eq!(world.get_block(BlockPos::new(1, 0, 0)).state, 1);
    }

    /// `#xaxis` compares every position to one latched coordinate, so a queued edit over
    /// five columns must write one of them and no more.
    ///
    /// The mask is what holds the latch. `EditJob::step` builds a `MaskContext` per
    /// position, because the write it does next needs the extent mutably, so a latch
    /// kept in the context would be empty every time — and a mask comparing each
    /// position to itself accepts all five columns.
    #[test]
    fn an_axis_mask_edits_one_column_not_the_whole_selection() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(4, 0, 0));
        let mut world = World::air(region);

        let mut job = EditJob::new(selection(region), pattern("stone")).with_mask(mask("#xaxis"));
        while !job.step(&mut world, &TestBlocks, usize::MAX).finished() {}

        assert_eq!(
            job.affected(),
            1,
            "#xaxis must latch one column, not accept every position"
        );
        for x in 0..=4 {
            let position = BlockPos::new(x, 0, 0);
            // The walk starts at the lowest X, so that is the column the mask
            // latched on its first test.
            assert_eq!(
                world.get_block(position).state == 1,
                x == 0,
                "x={x} disagrees with the one column #xaxis latches"
            );
        }
    }

    /// A radius is measured from one latched origin, so a spread-out selection is
    /// edited in the annulus around that origin and nowhere else.
    ///
    /// `{[5][10]}` is this port's spelling of a radius mask. Every position is on one
    /// line starting at X 0, which is where the walk begins and therefore where the
    /// mask latches, so the annulus is the six positions five to ten blocks along it.
    /// A latch in the context instead measures every position against itself, which is
    /// a distance of zero and so matches nothing at all.
    #[test]
    fn a_radius_mask_edits_only_the_annulus_around_its_origin() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(30, 0, 0));
        let mut world = World::air(region);

        let mut job =
            EditJob::new(selection(region), pattern("stone")).with_mask(mask("{[5][10]}"));
        while !job.step(&mut world, &TestBlocks, usize::MAX).finished() {}

        assert_eq!(job.affected(), 6, "expected the five-to-ten shell only");
        for x in 0..=30 {
            let position = BlockPos::new(x, 0, 0);
            let in_annulus = (5..=10).contains(&x);
            assert_eq!(
                world.get_block(position).state == 1,
                in_annulus,
                "x={x} is not within five to ten of the latched origin"
            );
        }
    }

    /// A budgeted job has to be resumable and must not skip or repeat a position,
    /// because that would leave a half-edited world.
    #[test]
    fn a_budgeted_job_covers_the_whole_selection_across_several_steps() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(9, 0, 0));
        let mut world = World::air(region);
        let mut job = EditJob::new(selection(region), pattern("stone"));

        let mut steps = 0;
        loop {
            let reason = job.step(&mut world, &TestBlocks, 3);
            steps += 1;
            if reason.finished() {
                break;
            }
            assert_eq!(reason, StopReason::Budget);
            assert!(steps < 20, "the job should not need this many steps");
        }
        assert_eq!(job.affected(), 10);
        for x in 0..=9 {
            assert_eq!(
                world.get_block(BlockPos::new(x, 0, 0)).state,
                1,
                "block at x={x} was skipped"
            );
        }
    }

    /// The block limit is what stops an admin's `//set` from rewriting a whole
    /// world. It must trigger rather than silently truncating.
    #[test]
    fn the_block_limit_stops_the_job() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(9, 0, 0));
        let mut world = World::air(region);
        let mut job = EditJob::new(selection(region), pattern("stone")).with_limit(3);
        assert_eq!(
            job.step(&mut world, &TestBlocks, usize::MAX),
            StopReason::TooManyBlocks
        );
    }

    /// The limit counts blocks changed, not positions visited.
    #[test]
    fn the_block_limit_counts_changes_not_positions() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(9, 0, 0));
        let mut world = World::air(region);
        for x in 0..10 {
            world.set_block(BlockPos::new(x, 0, 0), &BlockValue::new(1));
        }
        // Only two of the ten are dirt, so replacing dirt with stone changes two.
        world.set_block(BlockPos::new(0, 0, 0), &BlockValue::new(2));
        world.set_block(BlockPos::new(1, 0, 0), &BlockValue::new(2));

        let job = EditJob::new(selection(region), pattern("stone"))
            .with_mask(mask("dirt"))
            .with_limit(5);
        let changes = job.run(&mut world, &TestBlocks);
        assert_eq!(changes.affected(), 2);
    }

    /// Undoing has to put the original blocks back, which is what makes the
    /// recorded change set worth keeping.
    #[test]
    fn replaying_a_change_set_inverts_the_edit() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 0, 0));
        let mut world = World::air(region);
        world.set_block(BlockPos::new(1, 0, 0), &BlockValue::new(2));

        let changes =
            EditJob::new(selection(region), pattern("stone")).run(&mut world, &TestBlocks);
        assert_eq!(world.get_block(BlockPos::new(0, 0, 0)).state, 1);

        let undo = ReplayJob::new(Box::new(
            changes
                .changes()
                .iter()
                .map(|change| (change.position, change.original.clone(), true))
                .collect::<Vec<_>>()
                .into_iter(),
        ));
        undo.run(&mut world);

        assert_eq!(world.get_block(BlockPos::new(0, 0, 0)).state, AIR);
        assert_eq!(world.get_block(BlockPos::new(1, 0, 0)).state, 2);
    }

    /// A world holding distinct states so a transposed paste is visible.
    fn striped_world() -> World {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 0, 0));
        let mut world = World::air(region);
        // 1, 2, 1 from west to east, so a paste that transposes X and Z is obvious.
        for (x, state) in [1u16, 2, 1].into_iter().enumerate() {
            world.set_block(BlockPos::new(x as i32, 0, 0), &BlockValue::new(state));
        }
        world
    }

    /// An untransformed paste with no shift has to put every block back where it
    /// was, one for one and in the right order.
    #[test]
    fn an_untransformed_paste_reproduces_the_capture() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 0, 0));
        let world = striped_world();
        let clipboard =
            crate::clipboard::Clipboard::capture(&world, region, BlockPos::new(0, 0, 0));

        let steps = paste_steps(&clipboard, &Transform::Identity, BlockPos::new(0, 0, 0));
        assert_eq!(steps.len(), 3);
        for (destination, block) in steps {
            assert_eq!(destination, BlockPos::new(destination.x, 0, 0));
            assert_eq!(block.state, world.get_block(destination).state);
        }
    }

    /// The shift is what moves a paste to a new anchor, and it has to apply after
    /// the rotation so `//rotate 90` then pasting somewhere else lands sideways of
    /// where the player is standing rather than on top of them.
    #[test]
    fn a_shift_moves_the_whole_paste() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 0, 0));
        let world = striped_world();
        let clipboard =
            crate::clipboard::Clipboard::capture(&world, region, BlockPos::new(0, 0, 0));

        let steps = paste_steps(&clipboard, &Transform::Identity, BlockPos::new(0, 0, 10));
        let mut xs: Vec<i32> = steps.iter().map(|(destination, _)| destination.x).collect();
        xs.sort_unstable();
        assert_eq!(xs, vec![0, 1, 2]);
        for (destination, _) in &steps {
            assert_eq!(destination.z, 10, "the shift should move Z only");
        }
    }

    /// A quarter turn has to rotate the blocks, and because the capture is
    /// asymmetric (1, 2, 1) a rotation that leaves X alone would be caught here.
    #[test]
    fn a_rotated_paste_turns_the_copy_sideways() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 0, 0));
        let world = striped_world();
        let clipboard =
            crate::clipboard::Clipboard::capture(&world, region, BlockPos::new(0, 0, 0));

        let steps = paste_steps(
            &clipboard,
            &Transform::RotateY(-90.0),
            BlockPos::new(0, 0, 0),
        );
        // The strip ran along X, so after a quarter turn it runs along Z.
        for (destination, _) in &steps {
            assert_eq!(
                destination.x, 0,
                "the rotated paste should be a column in Z"
            );
        }
        let mut zs: Vec<i32> = steps.iter().map(|(destination, _)| destination.z).collect();
        zs.sort_unstable();
        // Turning X into Z about the origin sends (x, 0, 0) to (0, 0, x), so the
        // column occupies Z 0 through 2 rather than straddling the origin.
        assert_eq!(zs, vec![0, 1, 2]);
    }

    /// A paste writes the captured block at each destination and records what was
    /// there, so `//undo` after a paste restores the world.
    #[test]
    fn a_paste_is_recorded_so_it_can_be_undone() {
        let source_region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 0, 0));
        let source = striped_world();
        let clipboard =
            crate::clipboard::Clipboard::capture(&source, source_region, BlockPos::new(0, 0, 0));

        let target_region = CuboidRegion::new(BlockPos::new(0, 0, 20), BlockPos::new(2, 0, 20));
        let mut target = World::air(target_region);
        let job = PasteJob::new(Box::new(
            paste_steps(&clipboard, &Transform::Identity, BlockPos::new(0, 0, 20)).into_iter(),
        ));
        let changes = job.run(&mut target);

        assert_eq!(changes.affected(), 3);
        assert_eq!(target.get_block(BlockPos::new(1, 0, 20)).state, 2);

        let undo = ReplayJob::new(Box::new(
            changes
                .changes()
                .iter()
                .map(|change| (change.position, change.original.clone(), true))
                .collect::<Vec<_>>()
                .into_iter(),
        ));
        undo.run(&mut target);
        assert_eq!(target.get_block(BlockPos::new(1, 0, 20)).state, AIR);
    }
    /// A weighted pattern really does write a mix, and writes the same mix twice.
    ///
    /// This is the whole of `//replace stone 50%air,50%stone`: the weights are resolved
    /// per position rather than once per job, so a "scatter" spreads across the region
    /// instead of filling it with one block. Determinism matters as much as the mix —
    /// re-running the same edit has to give the same result, or a queued edit split
    /// across ticks would change under itself and the undo record would not match.
    #[test]
    fn a_weighted_pattern_writes_a_mix_and_is_repeatable() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(19, 0, 0));
        let pattern = crate::pattern::parse("50%air,50%stone", &TestBlocks).expect("parses");

        let run = || {
            let mut world = World::air(region);
            let mut job = EditJob::new(selection(region), pattern.clone());
            while !job.step(&mut world, &TestBlocks, usize::MAX).finished() {}
            world
        };

        let first = run();
        let second = run();
        let blocks: Vec<u16> = (0..20)
            .map(|x| first.get_block(BlockPos::new(x, 0, 0)).state)
            .collect();

        assert!(
            blocks.contains(&1) && blocks.contains(&AIR),
            "a 50/50 pattern must write both, got {blocks:?}"
        );
        for x in 0..20 {
            assert_eq!(
                first.get_block(BlockPos::new(x, 0, 0)),
                second.get_block(BlockPos::new(x, 0, 0)),
                "the same position must get the same block every run"
            );
        }
    }

    /// The two arms come out roughly even, not one-dominant.
    ///
    /// Twenty blocks is too few to assert on a proportion, so this is loose on purpose:
    /// it catches a roll that always picks the first arm, which a 50/50 split must not do.
    #[test]
    fn an_even_split_actually_splits() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(99, 0, 0));
        let pattern = crate::pattern::parse("50%air,50%stone", &TestBlocks).expect("parses");
        let mut world = World::air(region);
        let mut job = EditJob::new(selection(region), pattern);
        while !job.step(&mut world, &TestBlocks, usize::MAX).finished() {}
        let stone = (0..100)
            .filter(|x| world.get_block(BlockPos::new(*x, 0, 0)).state == 1)
            .count();
        assert!(
            stone > 20 && stone < 80,
            "expected a rough even split, got {stone}/100 stone"
        );
    }
}
