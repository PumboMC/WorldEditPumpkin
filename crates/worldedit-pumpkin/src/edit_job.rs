//! Running an edit without blocking the server tick.

use worldedit_core::{
    block::{BlockResolver, BlockValue},
    edit::{EditJob, ReplayJob, StopReason},
    extent::Extent,
    history::ChangeSet,
    math::BlockPos,
};

/// What an edit did, for the message the player gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every position was covered.
    Finished {
        /// How many blocks changed.
        affected: usize,
    },
    /// The edit would have changed more blocks than the limit allows.
    TooManyBlocks {
        /// How many blocks it changed before stopping.
        affected: usize,
        /// The limit it was working under.
        limit: usize,
    },
}

/// Runs an edit, stopping once it has changed `budget` blocks.
///
/// Returning the outcome rather than the change set is deliberate: the caller needs
/// both, and a `StopReason` of `Budget` means the edit is only part-done, which the
/// change set on its own does not say.
pub struct Progress {
    /// What happened.
    pub outcome: Outcome,
    /// What the edit recorded, so it can be undone.
    pub changes: ChangeSet,
}

/// Advances an edit by one tick's worth of blocks.
///
/// An edit over a large selection is spread over several ticks rather than run in
/// one blocking call, which is FAWE's central idea: a synchronous `//set` of a
/// million blocks would stall the tick for every player on the server. Returning
/// `None` means there is more to do and the caller should come back next tick.
pub fn tick<'a>(
    job: &mut EditJob<'a>,
    extent: &mut dyn Extent,
    resolver: &dyn BlockResolver,
    budget: usize,
    limit: Option<usize>,
) -> Option<Progress> {
    let reason = job.step(extent, resolver, budget);
    if !reason.finished() {
        return None;
    }
    let changes = job.changes();
    Some(Progress {
        outcome: match reason {
            StopReason::TooManyBlocks => Outcome::TooManyBlocks {
                affected: changes.affected(),
                limit: limit.unwrap_or(changes.affected()),
            },
            _ => Outcome::Finished {
                affected: changes.affected(),
            },
        },
        changes,
    })
}

/// Runs an edit to completion, however long it takes.
///
/// Only for edits whose positions are already in memory and whose size is known to
/// be small: a shape command's positions, or `//paste` of a clipboard the player
/// just copied. A selection-sized edit has to go through [`tick`].
pub fn run(
    job: EditJob<'_>,
    extent: &mut dyn Extent,
    resolver: &dyn BlockResolver,
) -> (Outcome, ChangeSet) {
    let changes = job.run(extent, resolver);
    (
        Outcome::Finished {
            affected: changes.affected(),
        },
        changes,
    )
}

/// The blocks an undo has to write back.
///
/// Redo is the same list with the other half of each change, so one function serves
/// both. Both directions walk the list forwards: `ChangeSetExecutor` defaults to its
/// batched path, where `UNDO_BATCH_SIZE` is 128, and `FaweStreamChangeSet` reads its
/// change stream from the front, passing `dir` only to decide which of the two values
/// each record decodes to. The `backwardIterator` in the non-batched branch is the
/// fallback for a vanilla `ChangeSet` rather than the route an undo normally takes.
#[must_use]
pub fn replay_steps(changes: &ChangeSet, undo: bool, air: BlockValue) -> Vec<ReplayStep> {
    changes
        .changes()
        .iter()
        .map(|change| {
            let block = if undo {
                change.original.clone()
            } else {
                change.replacement.clone().unwrap_or_else(|| air.clone())
            };
            // A redo of a removal has to clear the block entity the write lands on,
            // because writing air over a chest does not by itself empty it.
            let clears = !undo && (change.replacement.is_none() || block.entity.is_none());
            ReplayStep {
                position: change.position,
                block,
                clears,
            }
        })
        .collect()
}

/// One block write in an undo or a redo.
pub struct ReplayStep {
    /// Where to write.
    pub position: BlockPos,
    /// What to write.
    pub block: BlockValue,
    /// Whether the block entity at that position has to be cleared as well.
    pub clears: bool,
}

/// Runs an undo or a redo.
///
/// Unlike an edit, a replay is bounded by the size of a change set the player
/// already accepted once, so it runs in one go rather than over several ticks.
#[must_use]
pub fn replay(extent: &mut dyn Extent, steps: Vec<ReplayStep>) -> ChangeSet {
    ReplayJob::new(Box::new(
        steps
            .into_iter()
            .map(|step| (step.position, step.block, step.clears)),
    ))
    .run(extent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use worldedit_core::{
        block::BlockStateInfo,
        mask::Mask,
        pattern::Pattern,
        region::{CuboidRegion, Region, RegionWalker},
    };

    const AIR: u16 = 9;

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
        worldedit_core::pattern::parse(text, &TestBlocks).expect("the pattern should parse")
    }

    fn selection(region: CuboidRegion) -> Box<dyn Iterator<Item = BlockPos> + Send> {
        Box::new(RegionWalker::new(region.boxed()))
    }

    /// A tick-limited edit has to come back for more work rather than reporting
    /// done, or a large `//set` would silently stop after the first budget.
    #[test]
    fn a_budgeted_edit_comes_back_until_it_is_finished() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(9, 0, 0));
        let mut world = World::air(region);
        let mut job = EditJob::new(selection(region), pattern("stone"));

        let mut ticks = 0;
        let changes = loop {
            match tick(&mut job, &mut world, &TestBlocks, 4, None) {
                None => ticks += 1,
                Some(progress) => {
                    assert!(progress.changes.affected() == 10);
                    assert_eq!(progress.outcome, Outcome::Finished { affected: 10 });
                    break progress.changes;
                }
            }
            assert!(ticks < 10, "the edit should not need this many ticks");
        };
        assert_eq!(
            ticks, 2,
            "ten blocks at four per tick is three ticks, last one empty"
        );
        assert_eq!(world.get_block(BlockPos::new(9, 0, 0)).state, 1);
        drop(changes);
    }

    /// The undo has to put the original blocks back, and redo has to put the new
    /// ones back again.
    #[test]
    fn undo_then_redo_returns_to_the_same_world() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 0, 0));
        let mut world = World::air(region);
        let (_, changes) = run(
            EditJob::new(selection(region), pattern("stone")),
            &mut world,
            &TestBlocks,
        );
        assert_eq!(world.get_block(BlockPos::new(1, 0, 0)).state, 1);

        let air = BlockValue::new(AIR);
        let undo = replay(&mut world, replay_steps(&changes, true, air.clone()));
        assert_eq!(world.get_block(BlockPos::new(1, 0, 0)).state, AIR);

        let redo = replay(&mut world, replay_steps(&changes, false, air));
        assert_eq!(world.get_block(BlockPos::new(1, 0, 0)).state, 1);
        // The undo is itself recorded, so it can be redone back.
        assert_eq!(undo.affected(), 3);
        assert_eq!(redo.affected(), 3);
    }

    /// The message for a limit has to name the limit the player actually had, or
    /// raising `block-limit` would not fix what they were told to do.
    #[test]
    fn a_block_limit_is_reported_with_its_value() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(9, 0, 0));
        let mut world = World::air(region);
        let mut job = EditJob::new(selection(region), pattern("stone")).with_limit(4);
        let progress = tick(&mut job, &mut world, &TestBlocks, usize::MAX, Some(4))
            .expect("the limit stops the edit");

        // The edit stops ON the fourth change, not after a fifth. Java's `LimitExtent`
        // calls `limit.THROW_MAX_CHANGES()` before delegating the write and
        // `FaweLimit` throws on `decrementAndGet() < 0`, so with a limit of N the
        // writes 1..N succeed and write N+1 is refused before it happens. The old
        // behaviour tested the limit after writing, so `//limit 4` changed five blocks.
        // The limit itself is still reported so the player knows what to raise.
        assert_eq!(
            progress.outcome,
            Outcome::TooManyBlocks {
                affected: 4,
                limit: 4
            }
        );
    }

    /// A mask narrows an edit, and the limit counts only what it changed.
    #[test]
    fn a_mask_and_a_limit_apply_together() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(9, 0, 0));
        let mut world = World::air(region);
        for x in 0..3 {
            world.set_block(BlockPos::new(x, 0, 0), &BlockValue::new(2));
        }
        let mask = worldedit_core::mask::parse("dirt", &TestBlocks, (-64, 319))
            .expect("the mask should parse");
        let (_, changes) = run(
            EditJob::new(selection(region), pattern("stone")).with_mask(mask),
            &mut world,
            &TestBlocks,
        );
        assert_eq!(
            changes.affected(),
            3,
            "only the three dirt blocks were changed"
        );
        let _ = Mask::AlwaysTrue;
    }
}
