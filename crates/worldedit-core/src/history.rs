//! Recorded edits, and the per-player undo and redo stacks.

use std::collections::VecDeque;

use crate::{block::BlockValue, math::BlockPos};

/// One block an edit replaced, and what it was before.
///
/// The replacement is optional because removing a block is not a distinct block:
/// WorldEdit spells it as a null replacement, and keeping that spelling means the
/// undo path does not need a second case for "the edit deleted this".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Where the change happened.
    pub position: BlockPos,
    /// What was there before, block entity NBT included.
    pub original: BlockValue,
    /// What the edit put there, or `None` if it cleared the block.
    pub replacement: Option<BlockValue>,
}

/// Everything one command changed, in the order it changed it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    changes: Vec<Change>,
    world: Option<(String, String)>,
}

impl ChangeSet {
    /// An empty set, which is what an edit that matched nothing produces.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the exact level and dimension in which these changes were made.
    pub fn set_world(&mut self, level: String, dimension: String) {
        self.world = Some((level, dimension));
    }

    /// The exact target of this change set, if recorded by the host.
    pub fn world(&self) -> Option<(&str, &str)> {
        self.world
            .as_ref()
            .map(|(level, dimension)| (level.as_str(), dimension.as_str()))
    }

    /// Append an existing change, preserving removal and entity data.
    pub fn push_change(&mut self, change: Change) {
        self.changes.push(change);
    }

    /// Records a replacement, and returns whether it was worth recording.
    ///
    /// A change is dropped when the block is already what the edit wanted. The
    /// block has to be compared including its block entity, or undoing a command
    /// that re-set a sign with the text it already had would restore stale text.
    pub fn push(
        &mut self,
        position: BlockPos,
        original: BlockValue,
        replacement: BlockValue,
    ) -> bool {
        if original == replacement {
            return false;
        }
        self.changes.push(Change {
            position,
            original,
            replacement: Some(replacement),
        });
        true
    }

    /// Records a removal, and returns whether it was worth recording.
    pub fn push_removal(
        &mut self,
        position: BlockPos,
        original: BlockValue,
        air: BlockValue,
    ) -> bool {
        if original.state == air.state && original.entity.is_none() {
            return false;
        }
        self.changes.push(Change {
            position,
            original,
            replacement: None,
        });
        true
    }

    /// The recorded changes, oldest first.
    #[must_use]
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// How many blocks the edit changed.
    #[must_use]
    pub fn affected(&self) -> usize {
        self.changes.len()
    }

    /// Whether the edit changed nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// What an undo or redo should write, read off one recorded change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    /// Where to write.
    pub position: BlockPos,
    /// What to write.
    pub block: BlockValue,
    /// Whether the change was a removal, so the block entity is cleared too.
    ///
    /// A plain write of `air` also has to clear the block entity, because
    /// replacing a chest with air and then restoring it must not leave the chest's
    /// inventory in the world.
    pub clears_block_entity: bool,
}

/// One recorded change, resolved into what undoing or redoing it means.
#[must_use]
pub fn replay(change: &Change, undo: bool, air: BlockValue) -> Replay {
    let block = if undo {
        change.original.clone()
    } else {
        change.replacement.clone().unwrap_or_else(|| air.clone())
    };
    // A redo of a removal has to clear whatever block entity the write lands on,
    // because writing air over a chest does not by itself empty it. An undo puts
    // the block entity back from `original`, so it never needs clearing.
    let clears_block_entity = !undo && (change.replacement.is_none() || block.entity.is_none());
    Replay {
        position: change.position,
        block,
        clears_block_entity,
    }
}

/// A player's undo and redo stacks.
///
/// The oldest undoable edit is dropped once the stack is full, which is how
/// `history.size` behaves in WorldEdit: it counts edits, not blocks, so a player
/// who fills a world and then fills it again loses the first fill rather than
/// losing undo entirely.
#[derive(Debug, Clone)]
pub struct History {
    undo: VecDeque<ChangeSet>,
    redo: VecDeque<ChangeSet>,
    max_sets: usize,
}

impl History {
    /// A history holding at most `max_sets` undoable edits.
    #[must_use]
    pub fn new(max_sets: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            max_sets,
        }
    }

    /// Records a finished edit and discards any redo that a new edit invalidates.
    pub fn remember(&mut self, change_set: ChangeSet) {
        if change_set.is_empty() {
            // An edit that changed nothing must not push a redo away: nothing was
            // undone, so the redo stack is still valid.
            return;
        }
        self.redo.clear();
        self.undo.push_back(change_set);
        while self.undo.len() > self.max_sets {
            self.undo.pop_front();
        }
    }

    /// Takes the most recent edit off the undo stack and puts it on the redo
    /// stack, returning the changes to write back.
    pub fn take_undo(&mut self) -> Option<ChangeSet> {
        let change_set = self.undo.pop_back()?;
        self.redo.push_back(change_set.clone());
        Some(change_set)
    }

    /// Takes the most recent undone edit off the redo stack and puts it back on
    /// the undo stack, returning the changes to write again.
    pub fn take_redo(&mut self) -> Option<ChangeSet> {
        let change_set = self.redo.pop_back()?;
        self.undo.push_back(change_set.clone());
        Some(change_set)
    }

    /// Remove an entry for replay without committing it to the opposite stack.
    pub fn begin_replay(&mut self, undo: bool) -> Option<ChangeSet> {
        if undo {
            self.undo.pop_back()
        } else {
            self.redo.pop_back()
        }
    }

    /// Restore refused changes and move successfully replayed changes to the opposite stack.
    pub fn finish_replay(&mut self, undo: bool, refused: ChangeSet, applied: ChangeSet) {
        let (source, destination) = if undo {
            (&mut self.undo, &mut self.redo)
        } else {
            (&mut self.redo, &mut self.undo)
        };
        if !refused.is_empty() {
            source.push_back(refused);
        }
        if !applied.is_empty() {
            destination.push_back(applied);
            while destination.len() > self.max_sets {
                destination.pop_front();
            }
        }
    }

    /// The most recent edit without consuming it, which is what a peek needs.
    #[must_use]
    pub fn peek_undo(&self) -> Option<&ChangeSet> {
        self.undo.back()
    }

    /// How many edits can still be undone.
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    /// How many undone edits can be redone.
    #[must_use]
    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    /// Throws away every edit, as `//clearhistory` does.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn air() -> BlockValue {
        BlockValue::new(0)
    }

    fn set(change_set: &mut ChangeSet, pos: BlockPos, original: u16, replacement: u16) -> bool {
        change_set.push(pos, BlockValue::new(original), BlockValue::new(replacement))
    }

    #[test]
    fn partial_replay_keeps_destination_history_within_the_configured_limit() {
        let mut history = History::new(1);
        let mut first = ChangeSet::new();
        set(&mut first, BlockPos::new(0, 0, 0), 1, 5);
        let mut second = ChangeSet::new();
        set(&mut second, BlockPos::new(1, 0, 0), 2, 5);
        history.finish_replay(true, second.clone(), first);
        assert_eq!((history.undo_depth(), history.redo_depth()), (1, 1));
        let retried = history.begin_replay(true).unwrap();
        history.finish_replay(true, ChangeSet::new(), retried);
        assert_eq!((history.undo_depth(), history.redo_depth()), (0, 1));
        assert_eq!(history.begin_replay(false), Some(second));
    }

    /// Undo has to restore every block of a multi-block edit, in the order the edit
    /// made them, which is the order `replay_steps` walks.
    #[test]
    fn undoing_an_edit_restores_the_originals() {
        let mut change_set = ChangeSet::new();
        set(&mut change_set, BlockPos::new(0, 0, 0), 1, 5);
        set(&mut change_set, BlockPos::new(1, 0, 0), 2, 5);
        set(&mut change_set, BlockPos::new(2, 0, 0), 3, 5);

        let mut history = History::new(15);
        history.remember(change_set.clone());

        let undo = history.take_undo().expect("an edit was recorded");
        assert_eq!(undo.affected(), 3);
        let restored: Vec<u16> = undo
            .changes()
            .iter()
            .map(|change| change.original.state)
            .collect();
        assert_eq!(restored, vec![1, 2, 3]);
    }

    #[test]
    fn redoing_replays_the_replacements() {
        let mut change_set = ChangeSet::new();
        set(&mut change_set, BlockPos::new(0, 0, 0), 1, 5);

        let mut history = History::new(15);
        history.remember(change_set);
        history.take_undo();

        let redo = history.take_redo().expect("an edit was undone");
        assert_eq!(
            redo.changes()[0].replacement.as_ref().map(|b| b.state),
            Some(5)
        );
    }

    /// A new edit after an undo makes the undone edit unreachable, which is what
    /// clears the redo stack in WorldEdit.
    #[test]
    fn a_new_edit_discards_the_redo_stack() {
        let mut history = History::new(15);
        let mut first = ChangeSet::new();
        set(&mut first, BlockPos::new(0, 0, 0), 1, 5);
        history.remember(first);
        history.take_undo();
        assert_eq!(history.redo_depth(), 1);

        let mut second = ChangeSet::new();
        set(&mut second, BlockPos::new(0, 0, 0), 5, 9);
        history.remember(second);
        assert_eq!(history.redo_depth(), 0);
    }

    /// An edit that changed nothing must not eat a redo, or `//set stone` on an
    /// already-stone selection would silently cancel a pending `//redo`.
    #[test]
    fn an_empty_edit_leaves_the_redo_stack_alone() {
        let mut history = History::new(15);
        let mut first = ChangeSet::new();
        set(&mut first, BlockPos::new(0, 0, 0), 1, 5);
        history.remember(first);
        history.take_undo();

        history.remember(ChangeSet::new());
        assert_eq!(history.redo_depth(), 1);
    }

    /// Setting a block to itself is not a change, and recording it would make
    /// `//undo` restore a block that never moved.
    #[test]
    fn a_no_op_replacement_is_not_recorded() {
        let mut change_set = ChangeSet::new();
        assert!(!set(&mut change_set, BlockPos::new(0, 0, 0), 5, 5));
        assert!(change_set.is_empty());
    }

    /// Clearing an already-air block is not a change either.
    #[test]
    fn clearing_air_is_not_recorded() {
        let mut change_set = ChangeSet::new();
        assert!(!change_set.push_removal(BlockPos::new(0, 0, 0), air(), air()));
        assert!(change_set.is_empty());
    }

    /// A block entity is part of the block for comparison purposes, so re-setting
    /// a sign with different text is a change and with the same text is not.
    #[test]
    fn a_block_entity_difference_is_a_change() {
        let pos = BlockPos::new(0, 0, 0);
        let sign = |bytes: &[u8]| BlockValue::with_entity(7, bytes.to_vec().into_boxed_slice());

        let mut same = ChangeSet::new();
        assert!(!same.push(pos, sign(&[1, 2]), sign(&[1, 2])));

        let mut different = ChangeSet::new();
        assert!(different.push(pos, sign(&[1, 2]), sign(&[3, 4])));
        assert_eq!(different.affected(), 1);
    }

    #[test]
    fn a_full_stack_drops_the_oldest_edit() {
        let mut history = History::new(2);
        for index in 0..3u16 {
            let mut change_set = ChangeSet::new();
            set(
                &mut change_set,
                BlockPos::new(i32::from(index), 0, 0),
                index,
                index + 10,
            );
            history.remember(change_set);
        }
        assert_eq!(history.undo_depth(), 2);
        // The oldest edit is gone, so three undos cannot be taken.
        assert!(history.take_undo().is_some());
        assert!(history.take_undo().is_some());
        assert!(history.take_undo().is_none());
    }

    /// Undoing a removal writes the original block back, and must clear the block
    /// entity it is overwriting first or a chest keeps its contents.
    #[test]
    fn undoing_a_removal_clears_the_block_entity() {
        let chest = BlockValue::with_entity(9, vec![1, 2, 3].into_boxed_slice());
        let mut change_set = ChangeSet::new();
        assert!(change_set.push_removal(BlockPos::new(0, 0, 0), chest.clone(), air()));

        let undo = replay(&change_set.changes()[0], true, air());
        assert_eq!(undo.block, chest);
        assert!(!undo.clears_block_entity);

        let redo = replay(&change_set.changes()[0], false, air());
        assert_eq!(redo.block.state, air().state);
        assert!(redo.clears_block_entity);
    }
}
