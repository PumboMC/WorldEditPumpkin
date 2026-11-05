//! Per-player state: selection, clipboard, mask and undo history.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use worldedit_core::{
    clipboard::ClipboardHolder, history::History, mask::Mask, math::BlockPos, region::CuboidRegion,
};

/// How many edits a player can undo by default.
///
/// This is `LocalSession.MAX_HISTORY_SIZE`, which counts change sets rather than
/// blocks: a player who fills a world and then fills it again loses the first fill
/// rather than losing undo entirely.
pub const DEFAULT_HISTORY_SIZE: usize = 15;

/// What a player has selected, and what they have in hand.
pub struct Session {
    id: u64,
    /// The first selection point, from `//pos1`.
    pos1: Option<BlockPos>,
    /// The second selection point, from `//pos2`.
    pos2: Option<BlockPos>,
    /// The clipboard, from `//copy` or `//cut`.
    clipboard: Option<ClipboardHolder>,
    /// The mask applied to the destination of every edit, from `//gmask`.
    mask: Option<Mask>,
    /// The selection world name exchanged with WorldGuard.
    edit_world: Option<String>,
    /// The player's block limit, from `//limit`.
    ///
    /// `None` means "whatever `config.yml` says", which is how raising the limit in
    /// the config reaches players who never ran `//limit`. A `Some(0)` is a player
    /// who explicitly asked for no limit and has to stay distinguishable from
    /// `None`: collapsing the two would let `//limit 0` silently re-impose the
    /// configured cap on the next command.
    limit: Option<i64>,
    /// The player's undo and redo stacks.
    history: History,
    /// The item `//wand` gave them, by registry key.
    ///
    /// Recorded rather than hard-coded to an item so `//wand <item>` works: the wand
    /// handler compares the item in the player's hand against this, and a player
    /// holding two axes should only get a selection point from the one `//wand` gave
    /// them.
    wand: Option<String>,
    /// The brush equipped by `//sphere` and friends.
    ///
    /// Equipping does nothing on its own; the brush runs when the player right-clicks a
    /// block. This is FAWE's model — see `brush.rs`.
    brush: Option<crate::brush::Brush>,
    /// The most recent block click this player made, and when.
    ///
    /// Only there to recognise an echo. The vanilla client sends an arm-swing packet
    /// alongside the use-item-on packet for one right click, and the host turns each
    /// packet into its own `PlayerInteractEvent`, so a single right click arrives as a
    /// `RightClickBlock` followed by a `LeftClickBlock`. Treated as two clicks it sets
    /// both selection points and says two things about one gesture.
    last_click: Option<Click>,
}

/// One block click, as the host reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Click {
    /// The block that was clicked.
    pub position: BlockPos,
    /// The tick it was reported on.
    pub tick: u64,
    /// Whether it came from a right click.
    ///
    /// Kept as the flag rather than as the `InteractAction` because the plugin API's
    /// enum is a host type, and a session that stored it would be untestable here.
    pub right: bool,
}

/// How close together two clicks have to be to be the same gesture.
///
/// Long enough to cover the gap between two packets in one click, short enough that
/// clicking the same block again on purpose still counts. The client sends them back to
/// back within one tick; two ticks of slack covers a tick boundary landing between them.
const ECHO_TICKS: u64 = 2;

/// How long a break gesture keeps re-firing before it counts as a new one.
///
/// Generous on purpose: the cost of suppressing it is nothing (pos1 is already that
/// block), while the cost of allowing it is a message every break-time for as long as the
/// player holds the button. Ten seconds covers the slowest block a wand could be pointed
/// at.
const BREAK_WINDOW_TICKS: u64 = 200;

impl Default for Session {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY_SIZE)
    }
}

impl Session {
    /// A session whose history holds at most `history_size` edits.
    #[must_use]
    pub fn new(history_size: usize) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            pos1: None,
            pos2: None,
            clipboard: None,
            mask: None,
            edit_world: None,
            limit: None,
            history: History::new(history_size),
            wand: None,
            brush: None,
            last_click: None,
        }
    }

    /// Identifies this particular login session, not a later session with the same name.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Records a click the session has just acted on.
    pub fn record_click(&mut self, position: BlockPos, tick: u64, right: bool) {
        self.last_click = Some(Click {
            position,
            tick,
            right,
        });
    }

    /// Whether this is a second report of a click already acted on, rather than a new one.
    ///
    /// One gesture reaches this plugin as more than one event, and how many depends on the
    /// host rather than on the player: a right click sends a use-item packet *and* a swing
    /// packet, a break arrives as a start-destroy and then a stop-destroy, and the two are
    /// only tied together by landing on the same block within a tick or two. The test that
    /// separates them is therefore position-and-time, not which button was involved —
    /// comparing the button would mean guessing the host's packet order, and guessing
    /// wrong drops a real selection instead of a duplicate message.
    ///
    /// The first event to arrive is the one that acts. Everything else for that block in
    /// that window is dropped, silently, which is what makes one click produce one
    /// message no matter how many events the host made of it.
    #[must_use]
    pub fn is_repeat_click(&self, position: BlockPos, tick: u64) -> bool {
        self.last_click.is_some_and(|previous| {
            previous.position == position && tick.saturating_sub(previous.tick) <= ECHO_TICKS
        })
    }

    /// Whether this is another pass of a break gesture the plugin has already acted on.
    ///
    /// Separate from [`Session::is_repeat_click`] because the two repeat for different
    /// reasons and want very different windows. A click arrives as a burst of packets a
    /// tick or two apart. A *break* arrives once per break-time, for as long as the button
    /// is held, because cancelling the break does not cancel the mining. Two ticks would
    /// re-fire on every cycle; the window here is long enough to swallow a whole cycle.
    ///
    /// Suppressing it costs nothing: pos1 is being set to the block it is already set to.
    #[must_use]
    pub fn is_repeat_break(&self, position: BlockPos, tick: u64) -> bool {
        self.last_click.is_some_and(|previous| {
            previous.position == position
                && !previous.right
                && tick.saturating_sub(previous.tick) <= BREAK_WINDOW_TICKS
        })
    }

    /// Sets the first selection point.
    pub fn set_pos1(&mut self, position: BlockPos) {
        self.pos1 = Some(position);
    }

    /// Sets the second selection point.
    pub fn set_pos2(&mut self, position: BlockPos) {
        self.pos2 = Some(position);
    }

    /// The first selection point, if set.
    #[must_use]
    pub fn pos1(&self) -> Option<BlockPos> {
        self.pos1
    }

    /// The second selection point, if set.
    #[must_use]
    pub fn pos2(&self) -> Option<BlockPos> {
        self.pos2
    }

    /// The selection, or `None` while a point is missing.
    ///
    /// This is what every edit command asks for. Two points make a cuboid, which is
    /// WorldEdit's default selector.
    #[must_use]
    pub fn selection(&self) -> Option<CuboidRegion> {
        Some(CuboidRegion::new(self.pos1?, self.pos2?))
    }

    /// Forgets both selection points, as `//sel none` does.
    pub fn clear_selection(&mut self) {
        self.pos1 = None;
        self.pos2 = None;
    }

    /// The clipboard and its paste transform.
    pub fn set_clipboard(&mut self, clipboard: ClipboardHolder) {
        self.clipboard = Some(clipboard);
    }

    /// The clipboard and its paste transform, if one has been copied.
    #[must_use]
    pub fn clipboard(&self) -> Option<&ClipboardHolder> {
        self.clipboard.as_ref()
    }

    /// The clipboard, mutably, for `//rotate` and `//flip`.
    pub fn clipboard_mut(&mut self) -> Option<&mut ClipboardHolder> {
        self.clipboard.as_mut()
    }

    /// Forgets the clipboard, as `//clearclipboard` does.
    pub fn clear_clipboard(&mut self) {
        self.clipboard = None;
    }

    /// Sets or clears the global mask.
    pub fn set_mask(&mut self, mask: Option<Mask>) {
        self.mask = mask;
    }

    /// The global mask, if one is set.
    #[must_use]
    pub fn mask(&self) -> Option<&Mask> {
        self.mask.as_ref()
    }

    /// The player's history.
    #[must_use]
    pub fn history(&self) -> &History {
        &self.history
    }

    /// The player's history, mutably.
    pub fn history_mut(&mut self) -> &mut History {
        &mut self.history
    }

    /// Records which world an edit was made in, for `//undo` and `//redo`.
    pub fn set_edit_world(&mut self, world: String) {
        self.edit_world = Some(world);
    }

    /// The world the player's last edit was made in.
    #[must_use]
    pub fn edit_world(&self) -> Option<&str> {
        self.edit_world.as_deref()
    }

    /// Sets the player's block limit, where `Some(0)` means no limit.
    pub fn set_limit(&mut self, limit: Option<i64>) {
        self.limit = limit;
    }

    /// The player's own block limit, or `None` to follow the configuration.
    #[must_use]
    pub fn limit(&self) -> Option<i64> {
        self.limit
    }

    /// Records which item acts as this player's selection wand.
    ///
    /// Recorded per player rather than hard-coded, so `//wand <item>` works: the
    /// interact handler compares the held item against this, and a player carrying two
    /// of the same axe should only get a selection point from the one `//wand` gave
    /// them.
    pub fn set_wand(&mut self, item: Option<String>) {
        self.wand = item;
    }

    /// The item that acts as this player's selection wand.
    #[must_use]
    pub fn wand(&self) -> Option<&str> {
        self.wand.as_deref()
    }

    /// Equips a brush, replacing any already equipped.
    ///
    /// Per player rather than per world, which is what makes two players able to hold
    /// different brushes at once. FAWE keys tools to the item type as well, so two
    /// players holding different items can hold different brushes; here every player
    /// has exactly one slot, which is the simpler half of that and the half a single
    /// player can actually observe.
    pub fn set_brush(&mut self, brush: crate::brush::Brush) {
        self.brush = Some(brush);
    }

    /// The brush this player has equipped, if any.
    #[must_use]
    pub fn brush(&self) -> Option<&crate::brush::Brush> {
        self.brush.as_ref()
    }

    /// Unequips whatever brush is held.
    ///
    /// FAWE's `/brush unbind`. Separate from `set_brush` because clearing a slot has to
    /// work without inventing a brush to put in it.
    pub fn clear_brush(&mut self) {
        self.brush = None;
    }
}

/// Every player's session, keyed by name.
///
/// Separate from [`Session`] so a command can find one player's session without
/// holding the whole map borrowed, which the per-tick edit job needs.
#[derive(Default)]
pub struct Sessions {
    players: HashMap<String, Session>,
}

impl Sessions {
    /// Every session, empty until one is created.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The session for `player`, created on first use.
    pub fn entry(&mut self, player: &str) -> &mut Session {
        self.players.entry(player.to_string()).or_default()
    }

    /// The session for `player`, if it exists.
    #[must_use]
    pub fn get(&self, player: &str) -> Option<&Session> {
        self.players.get(player)
    }

    /// The session for `player`, if it exists, mutably.
    pub fn get_mut(&mut self, player: &str) -> Option<&mut Session> {
        self.players.get_mut(player)
    }

    /// Runs `body` against the player's session and returns what it produced.
    ///
    /// So that a command can read a session's state back out while it holds the map
    /// borrowed, without having to name the lifetime the borrow ends at.
    pub fn with<T>(&mut self, player: &str, body: impl FnOnce(&mut Session) -> T) -> T {
        body(self.entry(player))
    }

    /// Forgets a player's session, as leaving the server does.
    pub fn remove(&mut self, player: &str) -> Option<Session> {
        self.players.remove(player)
    }

    /// How many players have a session.
    #[must_use]
    pub fn len(&self) -> usize {
        self.players.len()
    }

    /// Whether nobody has a session.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.players.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use worldedit_core::transform::Transform;

    #[test]
    fn reconnect_cannot_inherit_the_previous_session_identity() {
        let mut sessions = Sessions::new();
        let old = sessions.entry("Alex").id();
        sessions.remove("Alex");
        let new = sessions.entry("Alex").id();
        assert_ne!(old, new);
    }

    fn holder() -> ClipboardHolder {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(1, 0, 0));
        ClipboardHolder::new(worldedit_core::clipboard::Clipboard::empty(
            region,
            BlockPos::new(0, 0, 0),
            worldedit_core::block::BlockValue::default(),
        ))
    }

    /// A selection needs both points; one point alone is not a region.
    /// The wand names the point it set, and adds the volume once both exist.
    ///
    /// FAWE's `explainPrimarySelection` and `explainSecondarySelection` are separate
    /// sentences from `selection_summary`, and the difference is the point: a player who
    /// has just clicked wants to know which corner moved, not how big the region is.
    #[test]
    fn the_wand_names_the_point_it_just_set() {
        let mut session = Session::new(5);

        // One point: no volume, because saying "(0 blocks)" would be a lie.
        session.set_pos1(BlockPos::new(12, 64, -34));
        let said = crate::commands::wand_summary(&session, BlockPos::new(12, 64, -34), true);
        assert_eq!(said, "First position set to (12, 64, -34).");

        // Both points: the volume is added, as FAWE does. Two and a half cubes across,
        // so 3 x 7 x 3.
        session.set_pos2(BlockPos::new(14, 70, -32));
        let said = crate::commands::wand_summary(&session, BlockPos::new(12, 64, -34), true);
        assert_eq!(said, "First position set to (12, 64, -34) (63).");

        // And the second point says "Second".
        let said = crate::commands::wand_summary(&session, BlockPos::new(14, 70, -32), false);
        assert_eq!(said, "Second position set to (14, 70, -32) (63).");
    }

    #[test]
    fn a_selection_needs_both_points() {
        let mut session = Session::default();
        assert!(session.selection().is_none());

        session.set_pos1(BlockPos::new(0, 0, 0));
        assert!(session.selection().is_none());

        session.set_pos2(BlockPos::new(5, 5, 5));
        let selection = session.selection().expect("both points are set");
        assert_eq!(selection.minimum(), BlockPos::new(0, 0, 0));
        assert_eq!(selection.maximum(), BlockPos::new(5, 5, 5));
    }

    /// The points can be set in either order, because a cuboid is defined by two
    /// corners rather than a start and an end.
    #[test]
    fn the_points_may_be_set_in_either_order() {
        let mut session = Session::default();
        session.set_pos1(BlockPos::new(9, 9, 9));
        session.set_pos2(BlockPos::new(1, 1, 1));
        let selection = session.selection().expect("both points are set");
        assert_eq!(selection.minimum(), BlockPos::new(1, 1, 1));
        assert_eq!(selection.maximum(), BlockPos::new(9, 9, 9));
    }

    #[test]
    fn clearing_the_selection_forgets_both_points() {
        let mut session = Session::default();
        session.set_pos1(BlockPos::new(0, 0, 0));
        session.set_pos2(BlockPos::new(1, 1, 1));
        session.clear_selection();
        assert!(session.pos1().is_none());
        assert!(session.selection().is_none());
    }

    /// `//rotate` and `//flip` change the transform on the holder, so rotating away
    /// and back must give the original holder again.
    #[test]
    fn a_rotate_and_a_flip_can_be_undone_by_stacking_the_inverse() {
        let mut session = Session::default();
        session.set_clipboard(holder());

        {
            let clipboard = session.clipboard_mut().expect("a clipboard was set");
            clipboard.push_transform(Transform::RotateY(-90.0));
            clipboard.push_transform(Transform::RotateY(90.0));
        }
        let now = session
            .clipboard()
            .expect("a clipboard was set")
            .transform()
            .clone();

        // A fresh clipboard has the identity transform, so that is what this has to
        // match.
        let original = Transform::Identity;
        for position in [
            BlockPos::new(3, 0, 5),
            BlockPos::new(-2, 1, 0),
            BlockPos::new(0, 0, 0),
        ] {
            assert_eq!(
                original.paste_position(position, BlockPos::new(0, 0, 0)),
                now.paste_position(position, BlockPos::new(0, 0, 0)),
                "stacking a quarter turn and its inverse must be the identity"
            );
        }
    }

    #[test]
    fn a_sessions_entry_is_created_once_and_kept() {
        let mut sessions = Sessions::new();
        assert!(sessions.is_empty());

        sessions.entry("alice").set_pos1(BlockPos::new(1, 2, 3));
        sessions.entry("alice").set_pos2(BlockPos::new(4, 5, 6));
        assert_eq!(sessions.len(), 1);
        assert!(sessions.get("alice").is_some_and(|s| s.pos1().is_some()));

        // A second entry for the same player keeps the first one's state.
        sessions.entry("alice").set_pos1(BlockPos::new(7, 7, 7));
        assert_eq!(
            sessions.get("alice").expect("alice has a session").pos1(),
            Some(BlockPos::new(7, 7, 7))
        );
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn removing_a_session_takes_its_history_with_it() {
        let mut sessions = Sessions::new();
        sessions.entry("alice");
        assert!(sessions.remove("alice").is_some());
        assert!(sessions.get("alice").is_none());
        assert!(sessions.is_empty());
    }

    /// A right click is reported twice, and the second report must be ignored.
    ///
    /// This is the whole reason `Click` exists. Without it, one right click moved two
    /// selection points and printed two lines, and the player had no way to tell the
    /// plugin apart from a wand that had genuinely registered two clicks.
    #[test]
    fn the_swing_half_of_a_right_click_is_an_echo() {
        let mut session = Session::default();
        let block = BlockPos::new(10, 64, -3);
        session.record_click(block, 500, true);
        assert!(
            session.is_repeat_click(block, 500),
            "the swing packet that follows a right click must be dropped"
        );
    }

    /// A genuine click with nothing ahead of it has to get through.
    ///
    /// The failure this guards against is the opposite one: treating every click as a
    /// repeat would make the wand do nothing at all.
    #[test]
    fn a_left_click_with_no_right_click_before_it_is_real() {
        let session = Session::default();
        let block = BlockPos::new(10, 64, -3);
        assert!(
            !session.is_repeat_click(block, 500),
            "a click nobody preceded is the real thing"
        );
    }

    /// A second gesture on the same block, a tick later, is the player clicking again.
    ///
    /// This is the boundary the window sets, and it is why the window is two ticks rather
    /// than a whole second: a player who means to move a selection point back and forth
    /// over the same block would otherwise be ignored.
    #[test]
    fn a_second_left_click_is_not_an_echo_of_the_first() {
        let mut session = Session::default();
        let block = BlockPos::new(10, 64, -3);
        session.record_click(block, 500, false);
        assert!(session.is_repeat_click(block, 501));
    }

    /// Clicking the same block again later is two gestures the player meant.
    ///
    /// The window is what separates "the second packet of this click" from "the player
    /// clicked again", and it is the one number here that could quietly swallow input
    /// if it were wrong.
    #[test]
    fn a_click_on_the_same_block_after_the_window_is_real() {
        let mut session = Session::default();
        let block = BlockPos::new(10, 64, -3);
        session.record_click(block, 500, true);
        assert!(session.is_repeat_click(block, 500 + ECHO_TICKS));
        assert!(
            !session.is_repeat_click(block, 500 + ECHO_TICKS + 1),
            "past the window this is a second click, not another report of the first"
        );
    }

    /// Moving to another block between the two reports means these are not one gesture.
    #[test]
    fn a_click_on_a_different_block_is_real() {
        let mut session = Session::default();
        session.record_click(BlockPos::new(10, 64, -3), 500, true);
        assert!(!session.is_repeat_click(BlockPos::new(10, 64, -2), 500));
    }

    /// The side is not part of the test, because the host decides the order and a rule
    /// that guessed it would drop a real selection instead of a duplicate message.
    ///
    /// Dropping the *second* report is what costs nothing: whichever event got there
    /// first has already set the point and already said so, so the player is left with one
    /// selection and one line however many events the host split the gesture into.
    #[test]
    fn a_second_report_of_the_same_click_is_dropped_however_it_arrives() {
        for first_right in [false, true] {
            for second_right in [false, true] {
                let mut session = Session::default();
                let block = BlockPos::new(10, 64, -3);
                session.record_click(block, 500, first_right);
                assert!(
                    session.is_repeat_click(block, 500),
                    "right={first_right} then right={second_right} is one click, not two"
                );
            }
        }
    }
}
