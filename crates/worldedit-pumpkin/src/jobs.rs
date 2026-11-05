//! Carrying an edit across server ticks.
//!
//! FAWE's whole point is that an edit is spread over time instead of blocking the
//! server tick: a synchronous `//set` of a million blocks would stall the tick for
//! every player on the server. A Wasm guest cannot reach Pumpkin's worker threads,
//! so the same idea is expressed as a per-tick block budget driven by the plugin
//! scheduler — [`tick`] advances the first queued edit by `blocks-per-tick` and stops.
//!
//! ## Why the queue is not locked while advancing
//!
//! Writing a block is a host call, and the queue lock must not be held across one:
//! anything that needed the lock in the meantime would block the tick, which is the
//! exact failure the budget exists to avoid. So [`tick`] takes one item out, does
//! the host work, and puts it back if it is unfinished. The same rule governs
//! history: a change set joins the player's undo stack only once the edit is over,
//! so `//undo` cannot reach a block the edit never wrote.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use pumpkin_plugin_api::{Server, text::TextComponent};
use worldedit_core::{
    block::{BlockResolver, BlockValue},
    edit::{EditJob, HistoryReplay, PasteJob, StopReason},
    history::ChangeSet,
    mask::Mask,
    math::BlockPos,
    pattern::Pattern,
    region::{CuboidRegion, Region, RegionWalker},
};

use crate::{
    edit_job, state,
    world::{PumpkinExtent, Registry},
};

/// What a finished edit says to the player who asked for it.
///
/// Every FAWE edit caption is a template with one `{0}`, and that placeholder is the
/// number of blocks the edit actually changed — a number that is not known when the
/// command runs, because the edit is still queued. So the caption travels with the job
/// and is rendered when the job drains, which is also the only point at which it can be
/// an honest number. Substituting a count at queue time would mean substituting
/// `positions.len()`, which counts what the edit *tried* to write rather than what it
/// changed, and a command that prints a wrong count confidently is worse than one that
/// prints the right one late.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caption {
    /// Say nothing at all.
    ///
    /// For the callers that already reported the edit when they queued it — a brush
    /// stroke, a paste, a cut. One gesture, one message: a second line when the queue
    /// drained would be a second message for the same thing.
    Silent,
    /// The caption, whatever the count.
    Always(&'static str),
    /// The caption, but only when at least one block changed.
    ///
    /// `RegionCommands.set` guards its own print: `if (affected != 0) { actor.print(...) }`
    /// (`RegionCommands.java:124-127`), and `//air` reaches the same handler
    /// (`:140-142`), so `//set air` over already-empty air says nothing at all rather
    /// than reporting a completion of zero.
    IfChanged(&'static str),
}

impl Caption {
    /// The line this caption prints for an edit that changed `affected` blocks.
    ///
    /// `None` means it prints nothing: either [`Caption::Silent`], or an edit that
    /// changed nothing under a caption Java guards the same way.
    #[must_use]
    pub fn render(self, affected: usize) -> Option<String> {
        match self {
            Self::Silent => None,
            Self::Always(template) => Some(substitute(template, affected)),
            Self::IfChanged(template) => (affected != 0).then(|| substitute(template, affected)),
        }
    }
}

/// Fills a caption's `{0}` with the changed block count.
///
/// One placeholder, replaced once. Every caption this plugin uses carries exactly one,
/// and a template carrying none is a caption FAWE would render unchanged, so leaving it
/// alone is the same answer.
fn substitute(template: &str, affected: usize) -> String {
    template.replacen("{0}", &affected.to_string(), 1)
}

/// An edit that has been started but not finished.
struct Pending {
    /// Who to tell when it is done, and whose history it belongs to.
    player: String,
    session_id: u64,
    /// The world being written to.
    ///
    /// The extent is held rather than just the world handle so the world name is
    /// available when the edit finishes and the change set has to be filed under it.
    extent: PumpkinExtent,
    /// Block name lookup, held beside the world rather than inside it so an edit can
    /// borrow the world mutably and the resolver immutably at the same time.
    registry: Registry,
    /// The most blocks this edit may change, or `None` for no limit.
    limit: Option<usize>,
    /// What to say once the changed block count is known.
    caption: Caption,
    /// The remaining work.
    work: Work,
}

/// The two kinds of edit that can outlive the tick they started on.
enum Work {
    /// A pattern applied over positions.
    Edit(Box<EditJob<'static>>),
    /// A clipboard written to the world.
    Paste(Box<PasteJob<'static>>),
}

impl Pending {
    /// Advances the edit by one tick's worth of blocks.
    ///
    /// Returns the recorded change set once the edit is over, and `None` while
    /// there is more to do. `budget` is passed in rather than stored because
    /// `blocks-per-tick` is re-read each tick, so a config reload takes effect on
    /// the edit already running.
    fn advance(&mut self, budget: usize) -> Option<(ChangeSet, bool)> {
        // Destructured so the world is borrowed mutably and the registry immutably at
        // the same time, which is the pair of borrows a pattern edit needs: one to
        // write the block, one to name it.
        let Self {
            extent,
            registry,
            limit,
            work,
            ..
        } = self;
        match work {
            Work::Edit(job) => {
                let resolver: &dyn BlockResolver = registry;
                edit_job::tick(job.as_mut(), extent, resolver, budget, *limit).map(|progress| {
                    let limited =
                        matches!(progress.outcome, edit_job::Outcome::TooManyBlocks { .. });
                    (progress.changes, limited)
                })
            }
            Work::Paste(job) => {
                let reason = job.step(extent, budget);
                reason
                    .finished()
                    .then(|| (job.changes(), reason == StopReason::TooManyBlocks))
            }
        }
    }

    /// Records the finished change set and tells the player what it did.
    ///
    /// The one line this edit produces. It is here rather than at queue time because
    /// the caption is templated on the block count, and the block count is what the
    /// edit is only now in a position to report.
    fn finish(self, server: &Server, changes: &ChangeSet, limited: bool) {
        if limited {
            send(
                server,
                &self.player,
                &format!(
                    "Block change limit {} reached; the edit is incomplete ({} blocks changed).",
                    self.limit.unwrap_or(0),
                    changes.affected()
                ),
            );
        }
        if self.extent.denied_blocks() > 0 {
            send(
                server,
                &self.player,
                &format!(
                    "WorldGuard refused {} protected block changes.",
                    self.extent.denied_blocks()
                ),
            );
        }
        if changes.is_empty() {
            // Nothing was written, so there is nothing to undo. FAWE likewise keeps
            // no history entry for an edit that changed no blocks, which is what
            // stops `//set air` over already-empty air from eating an undo.
            //
            // The caption is not sent either. An edit that changed nothing has no
            // count to report, and the captions that would carry one say so for a
            // zero-block edit that never happened (`worldedit.set.done` is guarded
            // precisely for this; the rest are simply not worth a line).
            return;
        }

        let (level, dimension) = self.extent.identity();
        let mut changes = changes.clone();
        changes.set_world(level.to_owned(), dimension.to_owned());
        if let Some(session) = state::sessions().get_mut(&self.player)
            && session.id() == self.session_id
        {
            session.set_edit_world(level.to_owned());
            session.history_mut().remember(changes.clone());
        }

        if !limited && let Some(caption) = self.caption.render(changes.affected()) {
            send(server, &self.player, &caption);
        }
    }
}

/// The edits currently in flight.
///
/// Empty until a command queues one, and a `Mutex<Vec<_>>` rather than a `OnceLock`
/// because there is nothing to construct: the queue is only appended to and drained.
enum Queued {
    Edit(Pending),
    Replay(ReplayPending),
    Clear { player: String, session_id: u64 },
}

static QUEUE: Mutex<Vec<Queued>> = Mutex::new(Vec::new());
static ADVANCING: AtomicBool = AtomicBool::new(false);

struct TickGuard;

impl Drop for TickGuard {
    fn drop(&mut self) {
        ADVANCING.store(false, Ordering::Release);
    }
}

/// Queues a pattern edit to be stepped a tick at a time, saying nothing when it finishes.
///
/// The limit is applied here rather than by the caller so there is one place it can
/// be set: `EditJob::with_limit` is what actually enforces it, and a caller that
/// passed a limit only to the queue would get an edit that writes without bound and
/// a message claiming it was capped.
///
/// Every caller of this one already told the player what it queued — a brush stroke
/// reports itself on the click — so the drain has to stay silent or the player reads
/// two lines for one gesture. A region edit that wants FAWE's own caption uses
/// [`enqueue_edit_saying`].
pub fn enqueue_edit(
    player: &str,
    extent: PumpkinExtent,
    positions: Box<dyn Iterator<Item = BlockPos> + Send + 'static>,
    pattern: Pattern,
    mask: Option<Mask>,
    limit: Option<usize>,
) {
    enqueue_edit_saying(
        player,
        extent,
        positions,
        pattern,
        mask,
        limit,
        Caption::Silent,
    );
}

/// [`enqueue_edit`], with the caption the finished edit prints.
///
/// The caption travels with the job rather than being printed by the caller, because
/// it is templated on the number of blocks the edit changed and that number does not
/// exist until the last tick of the edit has run.
pub fn enqueue_edit_saying(
    player: &str,
    extent: PumpkinExtent,
    positions: Box<dyn Iterator<Item = BlockPos> + Send + 'static>,
    pattern: Pattern,
    mask: Option<Mask>,
    limit: Option<usize>,
    caption: Caption,
) {
    let extent = extent.with_actor(player);
    let registry = extent.registry().clone();
    let mut job = EditJob::new(positions, pattern);
    if let Some(mask) = mask {
        job = job.with_mask(mask);
    }
    if let Some(limit) = limit {
        job = job.with_limit(limit);
    }
    push(Pending {
        player: player.to_string(),
        session_id: state::sessions().entry(player).id(),
        registry,
        extent,
        limit,
        caption,
        work: Work::Edit(Box::new(job)),
    });
}

/// Queues a paste to be stepped a tick at a time.
///
/// A paste of a clipboard the player just copied is a few thousand blocks and
/// usually fits in one budget, but a large build does not, and stopping it
/// half-written with no way to finish would be worse than the tick it costs.
///
/// Silent on finish, as every caller of it reports the paste itself when it queues it:
/// FAWE's `/paste` prints `worldedit.paste.pasted` (`ClipboardCommands.java:598`)
/// and then the copy's own status messages (`:600`), and this plugin's caller has
/// already said what it pasted and where.
pub fn enqueue_paste(
    player: &str,
    extent: PumpkinExtent,
    steps: Box<dyn Iterator<Item = (BlockPos, BlockValue)> + Send + 'static>,
    limit: Option<usize>,
) {
    let extent = extent.with_actor(player);
    let registry = extent.registry().clone();
    let mut job = PasteJob::new(steps);
    if let Some(limit) = limit {
        job = job.with_limit(limit);
    }
    push(Pending {
        player: player.to_string(),
        session_id: state::sessions().entry(player).id(),
        registry,
        extent,
        limit,
        caption: Caption::Silent,
        work: Work::Paste(Box::new(job)),
    });
}

/// Appends to the queue.
fn push(pending: Pending) {
    lock().push(Queued::Edit(pending));
}

struct ReplayActive {
    extent: PumpkinExtent,
    job: HistoryReplay,
}

struct ReplayPending {
    player: String,
    session_id: u64,
    undo: bool,
    remaining: usize,
    completed: usize,
    changed: usize,
    active: Option<ReplayActive>,
}

/// Queue history replay behind edits already requested by this player.
pub fn enqueue_replay(player: &str, undo: bool, times: usize) {
    let session_id = state::sessions().entry(player).id();
    lock().push(Queued::Replay(ReplayPending {
        player: player.to_owned(),
        session_id,
        undo,
        remaining: times,
        completed: 0,
        changed: 0,
        active: None,
    }));
}

/// Clear history in queue order, so it cannot race a replay or an edit finishing.
pub fn enqueue_clear_history(player: &str) {
    let session_id = state::sessions().entry(player).id();
    lock().push(Queued::Clear {
        player: player.to_owned(),
        session_id,
    });
}

fn session_exists(player: &str, session_id: u64) -> bool {
    state::sessions()
        .get(player)
        .is_some_and(|session| session.id() == session_id)
}

impl ReplayPending {
    fn advance(&mut self, server: &Server, budget: usize) -> bool {
        let direction = if self.undo {
            crate::commands::Direction::Undo
        } else {
            crate::commands::Direction::Redo
        };
        if !session_exists(&self.player, self.session_id) {
            return true;
        }
        if self.active.is_none() {
            let changes = {
                let mut sessions = state::sessions();
                let Some(session) = sessions.get_mut(&self.player) else {
                    return true;
                };
                if session.id() != self.session_id {
                    return true;
                }
                session.history_mut().begin_replay(self.undo)
            };
            let Some(changes) = changes else {
                let message = if self.completed == 0 {
                    direction.nothing_left().to_owned()
                } else {
                    format!(
                        "{} ({} blocks changed).",
                        direction.replayed(self.completed),
                        self.changed
                    )
                };
                send(server, &self.player, &message);
                return true;
            };
            let world = changes.world().and_then(|(level, dimension)| {
                server
                    .get_all_worlds()
                    .into_iter()
                    .find(|world| world.get_name() == level && world.get_dimension() == dimension)
            });
            let Some(world) = world else {
                if let Some(session) = state::sessions().get_mut(&self.player)
                    && session.id() == self.session_id
                {
                    session
                        .history_mut()
                        .finish_replay(self.undo, changes, ChangeSet::new());
                }
                send(
                    server,
                    &self.player,
                    "The recorded world and dimension are not loaded; history was retained.",
                );
                return true;
            };
            self.active = Some(ReplayActive {
                extent: PumpkinExtent::new(world, state::config().world_height)
                    .with_actor(&self.player),
                job: HistoryReplay::new(changes, self.undo),
            });
        }
        let active = self.active.as_mut().expect("replay was initialized");
        if !active.job.step(&mut active.extent, budget).finished() {
            return false;
        }
        let active = self.active.take().expect("replay was initialized");
        let (refused, applied) = active.job.into_results();
        let refused_count = refused.affected();
        self.changed += applied.affected();
        if let Some(session) = state::sessions().get_mut(&self.player)
            && session.id() == self.session_id
        {
            session
                .history_mut()
                .finish_replay(self.undo, refused, applied);
        } else {
            return true;
        }
        if refused_count > 0 {
            send(
                server,
                &self.player,
                &format!(
                    "History replay changed {} blocks; {refused_count} writes were refused and remain available to retry.",
                    self.changed
                ),
            );
            return true;
        }
        self.completed += 1;
        self.remaining -= 1;
        if self.remaining == 0 {
            send(
                server,
                &self.player,
                &format!(
                    "{} ({} blocks changed).",
                    direction.replayed(self.completed),
                    self.changed
                ),
            );
            true
        } else {
            false
        }
    }
}

/// The server tick, counted by [`tick`].
///
/// Exists so an event handler can tell "this click just happened" from "this click
/// happened a moment ago". The vanilla client sends an arm-swing packet along with the
/// use-item-on packet for a single right click, and the two arrive as two separate
/// `PlayerInteractEvent`s; without a clock there is no way to tell the second one from
/// a genuine left click. See `OnWandClick`.
///
/// Counting here rather than asking the host is deliberate: `tick` is already scheduled
/// to run every tick whether or not work is queued, so this stays accurate without a
/// second registration and without the handler having to notice anything.
static TICK: AtomicU64 = AtomicU64::new(0);

/// The current tick.
///
/// Monotonic for the lifetime of the process. `fetch_add` wraps in principle, but at
/// twenty ticks a second that is over seventeen million years, and the comparison it
/// feeds is against a tick counted from the same origin, so a wrap is not a reachable
/// case worth handling.
#[must_use]
pub fn now() -> u64 {
    TICK.load(Ordering::Relaxed)
}

/// Advances the oldest queued edit by one tick's budget.
///
/// Registered as a one-tick repeating task. It runs whether or not anything is
/// queued, so the idle path is a lock and an emptiness check; stopping and
/// restarting the task would mean racing the scheduler to notice a new edit
/// arriving in between, which is more machinery than the check it saves.
pub fn tick(server: Server) {
    if ADVANCING.swap(true, Ordering::Acquire) {
        return;
    }
    let _guard = TickGuard;
    TICK.fetch_add(1, Ordering::Relaxed);
    let settings = state::config();
    // Taken out rather than held, because writing a block is a host call.
    let mut pending = {
        let mut queue = lock();
        if queue.is_empty() {
            return;
        }
        queue.remove(0)
    };
    let finished = match &mut pending {
        Queued::Edit(edit) => {
            if !session_exists(&edit.player, edit.session_id) {
                true
            } else if let Some((changes, limited)) = edit.advance(settings.blocks_per_tick) {
                // Finish consumes the job below.
                if let Queued::Edit(edit) = pending {
                    edit.finish(&server, &changes, limited);
                }
                return;
            } else {
                false
            }
        }
        Queued::Replay(replay) => replay.advance(&server, settings.blocks_per_tick),
        Queued::Clear { player, session_id } => {
            let cleared = {
                let mut sessions = state::sessions();
                if let Some(session) = sessions.get_mut(player)
                    && session.id() == *session_id
                {
                    session.history_mut().clear();
                    true
                } else {
                    false
                }
            };
            if cleared {
                send(&server, player, "History cleared.");
            }
            true
        }
    };
    if !finished {
        lock().insert(0, pending);
    }
}

/// Sends `message` to a connected player.
///
/// By name rather than through a stored sender handle, because a `CommandSender` is
/// a reference to one connection and an edit can outlive it. A `None` lookup means the
/// player left before the message was delivered.
pub(crate) fn send(server: &Server, player: &str, message: &str) {
    if let Some(handle) = server.get_player_by_name(player) {
        handle.send_system_message(TextComponent::text(message), false);
    }
}

/// Walks a cuboid, which is what a selection-shaped edit iterates.
///
/// A named helper so the `'static` and `Send` bounds the queue needs are stated once
/// rather than rediscovered at each call site.
#[must_use]
pub fn walk(region: CuboidRegion) -> Box<dyn Iterator<Item = BlockPos> + Send + 'static> {
    Box::new(RegionWalker::new(region.boxed()))
}

/// How many edits are in flight, for the startup summary.
#[must_use]
pub fn queued() -> usize {
    lock().len()
}

/// Locks the queue, recovering from a poisoned one.
///
/// A panic inside a command handler poisons the lock and every later edit would then
/// fail. Recovering the data is better than refusing every edit for the rest of the
/// process's life.
fn lock() -> MutexGuard<'static, Vec<Queued>> {
    QUEUE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An edit that changed nothing produces no line at all, whatever its caption.
    ///
    /// This is the rule that makes one gesture one message. The queue used to print
    /// "`<player>` changed nothing." when an edit wrote no blocks — an invented line, on
    /// top of the invented one the command printed when it queued the edit — and it did
    /// it after a caption claiming a completion count, which is worse than saying
    /// nothing: the player is told an edit happened, and then that it did not.
    ///
    /// The rule is in [`Pending::finish`], which returns before both the history write
    /// and the caption when nothing changed. It sits in the source rather than in a
    /// helper for the same reason `commands::tests::rotate_and_flip_describe_the_
    /// clipboard_copy` reads its captions out of the file: `finish` needs a `Pending`
    /// and a `Server`, and there is nothing pure left to call.
    #[test]
    fn an_empty_change_set_is_not_reported_at_all() {
        let shipped = include_str!("jobs.rs")
            .split_once("#[cfg(test)]")
            .expect("this file has a test module")
            .0;

        assert!(
            shipped.contains("if changes.is_empty() {"),
            "an edit that wrote nothing must return before the caption is rendered"
        );

        // What it branches on. A change set that was never written is empty, and the
        // count it reports is then zero — which is the number the caption would have
        // carried had the edit reached the render.
        let empty = ChangeSet::new();
        assert!(empty.is_empty());
        assert_eq!(empty.affected(), 0);
    }

    /// Java guards `//set`'s own print with `if (affected != 0)`
    /// (`RegionCommands.java:124`), so the caption is rendered only once there is a
    /// count to put in it.
    ///
    /// An unguarded caption is *not* silently turned into a guarded one: FAWE prints
    /// `worldedit.replace.replaced` for a zero-block replace, and suppressing that would
    /// be a second, silent divergence. The silence for an empty change set comes from
    /// `finish`, which is a different rule; see
    /// [`an_empty_change_set_is_not_reported_at_all`].
    #[test]
    fn a_guarded_caption_is_the_one_java_guards() {
        assert_eq!(
            Caption::IfChanged("Operation completed ({0}).").render(1),
            Some("Operation completed (1).".to_string())
        );
        assert_eq!(
            Caption::IfChanged("Operation completed ({0}).").render(0),
            None
        );
        assert_eq!(
            Caption::Always("{0} blocks have been replaced.").render(0),
            Some("0 blocks have been replaced.".to_string()),
            "an unguarded caption still renders at zero"
        );
    }

    /// The placeholder is replaced and nothing else is.
    ///
    /// The captions are full sentences with a full stop, a set of parentheses and a
    /// parenthesised phrase of their own, so a substitution that trims or reorders
    /// would quietly rewrite the caption rather than fill it.
    #[test]
    fn only_the_placeholder_is_replaced() {
        assert_eq!(
            Caption::Always("Center set. ({0} blocks changed)").render(1200),
            Some("Center set. (1200 blocks changed)".to_string())
        );
        assert_eq!(
            Caption::IfChanged("Operation completed ({0}).").render(1),
            Some("Operation completed (1).".to_string()),
            "a count of one is not pluralised, because FAWE does not pluralise"
        );
        assert_eq!(
            Caption::Always("{0} blocks have been created.").render(1),
            Some("1 blocks have been created.".to_string())
        );
    }

    /// A caption that says nothing stays silent whatever the count.
    ///
    /// This is what the brush, `//paste`, `//cut`, `//move` and `//stack` queue: each of
    /// them reports itself the moment it queues, so a line here would be a second one.
    #[test]
    fn a_silent_caption_is_silent_for_every_count() {
        for affected in [0, 1, 1200, usize::MAX] {
            assert_eq!(Caption::Silent.render(affected), None);
        }
    }
}
