//! WorldEdit for Pumpkin: selection, edit and history commands as a Wasm component.
//!
//! ## What is here
//!
//! | Crate | What it holds |
//! |:--|:--|
//! | `worldedit-core` | The engine. Selection shapes, patterns, masks, generated shapes, the clipboard and undo history. No dependency on Pumpkin, so it is tested on the host target. |
//! | `worldedit-pumpkin` | This crate. The world adapter, per-player session state, configuration, argument reading, the commands, and the queue that advances an edit a tick at a time. |
//!
//! ## What is not verified
//!
//! The commands are registered and the engine is tested, but nothing here has been run
//! against a live server: the world adapter and the queue have been compiled, not
//! executed. See the README's "What is missing" for the exact list, because a partial
//! command surface is worse than a documented one — an admin needs to know which of
//! FAWE's commands are safe to run.
//!
//! `missing_docs` is not enabled here, unlike in `worldedit-core`: the
//! `register_plugin!` macro expands to a function this crate cannot document, and
//! the plugin side is thin enough that the engine's lint is where the value is.

use pumpkin_plugin_api::{
    Context, Plugin, PluginMetadata, Result,
    events::{
        BlockBreakEvent, EventData, EventHandler, EventPriority, PlayerInteractEvent,
        PlayerLeaveEvent,
    },
    scheduler::schedule_repeating_task,
    text::TextComponent,
    wit::pumpkin::plugin::{common::Hand, event::InteractAction},
};
use tracing::{error, info, warn};

/// Reading WorldEdit's arguments out of the command tree.
pub mod arguments;
/// The brushes: shapes that follow the block you are looking at.
pub mod brush;
/// The commands.
pub mod commands;
/// The plugin's `config.yml`.
pub mod config;
/// Driving an edit across ticks.
pub mod edit_job;
/// The queue of edits in flight.
pub mod jobs;
/// Permission node names.
pub mod nodes;
/// Per-player selection, clipboard and history.
pub mod session;
/// The plugin's shared state.
pub mod state;
/// The Pumpkin world, as the engine sees it.
pub mod world;

mod block_entity;
mod protection;
/// Serves WorldGuard's selection queries over the host's IPC interface.
pub mod wgipc;

#[cfg(target_arch = "wasm32")]
use pumpkin_plugin_api::register_plugin;

/// The plugin name.
///
/// Also the only permission namespace this plugin may register: the host refuses a
/// node that is not prefixed with the plugin name, matched case sensitively, and an
/// unregistered node is denied to everyone including operators.
pub const PLUGIN: &str = "WorldEdit";

pub struct WorldEdit;

impl Plugin for WorldEdit {
    fn new() -> Self {
        Self
    }

    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: PLUGIN.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            authors: vec!["Rennex".to_string()],
            description: "Selection, edit and history commands for Pumpkin, ported from \
                          FastAsyncWorldEdit."
                .to_string(),
            dependencies: Vec::new(),
            // Only the data folder. No sockets, no environment.
            permissions: config::capabilities(),
        }
    }

    fn handle_ipc_message(
        &self,
        sender: pumpkin_plugin_api::ipc::PluginId,
        message: pumpkin_plugin_api::ipc::IpcMessage,
    ) -> Result<pumpkin_plugin_api::ipc::IpcMessage, String> {
        // Answering an unknown caller would let any plugin read another
        // player's selection, which is player data.
        if sender != wgipc::WORLDGUARD {
            return Err(format!("{PLUGIN} does not serve IPC for '{sender}'"));
        }
        if message == [1, 3] {
            protection::require_worldguard();
            return Ok(vec![1, 2]);
        }
        let request = wgipc::decode_request(&message)?;
        Ok(wgipc::handle(&request))
    }

    fn on_load(&self, context: Context) -> Result<()> {
        let folder = config::data_folder(&context);

        // The shipped config is written once and never rewritten, so an admin's
        // comments survive. A failure here is worth reporting but not worth refusing
        // to load over: WorldEdit still works, it just uses the defaults.
        match config::write_default(&folder) {
            Ok(true) => info!("wrote the default config to {folder}"),
            Ok(false) => {}
            Err(problem) => warn!("{problem}"),
        }

        let (settings, problems) = config::Config::load(&folder);
        for problem in &problems {
            error!("{folder}/config.yml: {problem}");
        }
        if !problems.is_empty() {
            warn!("using the default settings for the settings that could not be read");
        }

        // Installed through `state::install` rather than a private lock here, so
        // that `state::config()` and `state::sessions()` read the settings that were
        // just loaded. Two separate stores would mean the config was silently ignored.
        let _ = state::install(state::State::new(settings));
        // Recorded so `//schem` can find the folder without keeping the `Context`, which
        // a plugin should not hold past load.
        state::set_folder(&context);
        // The folder is created now rather than on the first save: `//schem list` is
        // what a player tries first, and it would otherwise fail on a fresh install.
        if let Err(problem) = commands::ensure_schem_folder() {
            warn!("{problem}");
        }

        // A player who leaves takes their selection and history with them, matching
        // FAWE's `history.delete-on-logout`, which is true by default.
        register_events(&context)?;

        // The nodes before the commands: a command whose node was refused is
        // registered but invisible to everybody, operators included.
        let node_count = nodes::register_all(&context);

        let roots = commands::all();
        let command_count = roots.len();
        for entry in roots {
            // Each tree gets the one node the host will attach, which is the coarse
            // category check. The finer per-subcommand check is inside the handler;
            // see the commands module for why that is the only option on this host.
            context.register_command(entry.command, entry.node);
        }

        // One tick, so an edit spends `blocks-per-tick` per server tick rather than
        // blocking it. Runs whether or not anything is queued; the idle path is a
        // lock and an emptiness check.
        let task = schedule_repeating_task(1, 1, jobs::tick);
        state::set_task(task);
        info!("registered the edit queue as task {task}");

        let settings = state::config();
        info!(
            "{PLUGIN} {} loaded: {command_count} command(s), {node_count} permission node(s), \
             history-size {}, block-limit {}, blocks-per-tick {}, world-height {}",
            env!("CARGO_PKG_VERSION"),
            settings.history_size,
            settings.block_limit().unwrap_or(0),
            settings.blocks_per_tick,
            settings.world_height,
        );

        Ok(())
    }

    fn on_unload(&self, _context: Context) -> Result<()> {
        // The scheduler task is cancelled rather than left to fire into a plugin that
        // is gone. A queued edit that never finishes is dropped with it: the change
        // set was only half written and was never recorded, so `//undo` cannot have
        // promised anything about it.
        let task = state::take_task();
        if let Some(task) = task {
            pumpkin_plugin_api::scheduler::cancel_task(task);
            info!(
                "cancelled the edit queue, task {task}, with {} edit(s) still queued",
                jobs::queued()
            );
        }
        info!("{PLUGIN} unloaded");
        Ok(())
    }
}

struct OnLeave;

impl EventHandler<PlayerLeaveEvent> for OnLeave {
    fn handle(
        &self,
        _server: pumpkin_plugin_api::Server,
        event: EventData<PlayerLeaveEvent>,
    ) -> EventData<PlayerLeaveEvent> {
        state::forget(&event.player.get_name());
        event
    }
}

fn say(player: &pumpkin_plugin_api::Player, text: &str) {
    player.send_system_message(TextComponent::text(text), false);
}

const WAND_REACH: f64 = 100.0;

struct OnWandClick;

impl EventHandler<PlayerInteractEvent> for OnWandClick {
    fn handle(
        &self,
        _server: pumpkin_plugin_api::Server,
        mut event: EventData<PlayerInteractEvent>,
    ) -> EventData<PlayerInteractEvent> {
        // A Bedrock client never produces a *block* click. Its use-item-on packet is turned
        // straight into a block placement and never reaches a plugin, so the only interact
        // event it raises is the air one, and that carries no position. The block the
        // player meant is the one under their crosshair, so it is found by tracing rather
        // than read off the event. A host that does report a position still uses it, since
        // there the trace could disagree with what was actually clicked.
        let right = matches!(
            event.action,
            InteractAction::RightClickBlock | InteractAction::RightClickAir
        );
        let left = matches!(event.action, InteractAction::LeftClickBlock);
        if !right && !left {
            // `LeftClickAir` is the arm-swing half of a right click, and on Bedrock it is
            // also the only thing a left click produces besides the break itself, which
            // `OnWandBreak` handles. Tracing for it here would turn every idle swing into
            // a selection point.
            return event;
        }

        let position = match event.clicked_pos {
            Some(clicked) => world::from_wit(clicked),
            None => {
                let Some(target) = event.player.ray_trace_block(WAND_REACH, false) else {
                    // Aiming at nothing is not something a player needs telling about: it
                    // is indistinguishable, to them, from aiming past the world.
                    return event;
                };
                world::from_wit(target.pos)
            }
        };

        let name = event.player.get_name();
        let held = event
            .player
            .get_inventory()
            .get_item_in_hand(Hand::Right)
            .map(|stack| stack.get_registry_key());

        let tick = jobs::now();
        let outcome = state::sessions().with(&name, |session| {
            // Another report of the click beside this one. Silent: the one that got here
            // first already set the point and already said so.
            if session.is_repeat_click(position, tick) {
                return Outcome::Echo;
            }
            // Silence, in every case below. FAWE binds its wand to an *item*: the tool is
            // invoked by the server handing it the block click, and a player holding
            // anything else simply never triggers it. There is no "you have no wand" state
            // in FAWE to report, because a player who never ran `//wand` is not a wand user
            // having a bad time - they are a player mining.
            //
            // So the only thing this handler is allowed to say anything about is a click it
            // actually consumed. An earlier version answered "No wand is set. Run
            // /worldedit wand first." to every block anyone broke, which is a plugin
            // talking to a player who did not ask it anything.
            if session.wand().is_none() {
                return Outcome::NotAWand;
            }
            if held.is_none() {
                return Outcome::NotAWand;
            }
            if !same_item(
                held.as_deref().unwrap_or_default(),
                session.wand().unwrap_or_default(),
            ) {
                return Outcome::NotAWand;
            }
            // Left is pos1, right is pos2.
            if right {
                session.set_pos2(position);
            } else {
                session.set_pos1(position);
            }
            // Recorded only once the click is known to be a real one, so a dropped echo
            // cannot make the next real left click look like an echo of itself.
            session.record_click(position, tick, right);
            Outcome::Set
        });

        match outcome {
            Outcome::Set => {
                // Reported and cancelled. The report names the point that just moved
                // rather than describing the selection, which is what a player who has
                // just clicked is asking about.
                let summary =
                    state::sessions().with(&name, |s| commands::wand_summary(s, position, !right));
                say(&event.player, &summary);
                event.cancelled = true;
            }
            // Said nothing: the right click that caused it has already spoken, and a
            // second line would read as the player having clicked twice.
            Outcome::Echo => {}
            // Said nothing either, and deliberately. Every one of these is a player who is
            // not using WorldEdit: no wand equipped, an empty hand, or some other item.
            // FAWE's wand is bound to an item, so those clicks never reach a WorldEdit tool
            // at all and there is nothing for FAWE to report. One enum variant rather than
            // three, because the three differ in nothing a player can act on and splitting
            // them is what invites one of them to grow a message later.
            Outcome::NotAWand => {}
        }
        event
    }
}

struct OnWandBreak;

impl EventHandler<BlockBreakEvent> for OnWandBreak {
    fn handle(
        &self,
        _server: pumpkin_plugin_api::Server,
        mut event: EventData<BlockBreakEvent>,
    ) -> EventData<BlockBreakEvent> {
        // A block broken by a piston or a creeper is nobody's selection point.
        let Some(player) = event.player.as_ref() else {
            return event;
        };
        let name = player.get_name();
        let held = player
            .get_inventory()
            .get_item_in_hand(Hand::Right)
            .map(|stack| stack.get_registry_key());
        let position = world::from_wit(event.block_pos);
        let tick = jobs::now();

        let reported = state::sessions().with(&name, |session| {
            let Some(wand) = session.wand() else {
                return false;
            };
            if !same_item(held.as_deref().unwrap_or_default(), wand) {
                return false;
            }
            // The window is the break-time rather than the packet gap: this is one gesture
            // that arrives repeatedly, not several gestures that arrived together.
            if session.is_repeat_break(position, tick) {
                return false;
            }
            session.set_pos1(position);
            session.record_click(position, tick, false);
            true
        });

        // Cancelled on every pass, reported only on the first. Doing it the other way round
        // would let a de-duplication miss break the block, which is the one outcome a
        // selection wand must never produce.
        if reported
            || state::sessions().with(&name, |session| {
                session
                    .wand()
                    .is_some_and(|wand| same_item(held.as_deref().unwrap_or_default(), wand))
            })
        {
            event.cancelled = true;
        }
        if reported {
            let summary =
                state::sessions().with(&name, |s| commands::wand_summary(s, position, true));
            say(player, &summary);
        }
        event
    }
}

/// Why a click with a wand did or did not become a selection point.
///
/// Two outcomes rather than a `bool`: a click this handler consumed has something to
/// report and a click it ignored has to stay silent, and those are the only two things
/// that happen. Everything in between is "not a wand user", which is not an outcome at
/// all - it is the absence of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// A point was set.
    Set,
    /// The click was not made with a wand, so it was none of this plugin's business.
    ///
    /// Covers no wand equipped, an empty hand, and any other item. FAWE never announces
    /// these, because its wand is an item binding rather than a session setting, and a
    /// player who has not asked for WorldEdit should never be told anything by it.
    NotAWand,
    /// The arm-swing half of a right click that has already been handled.
    Echo,
}

/// Whether two item names name the same item.
///
/// The namespace is optional: a wand recorded as `wooden_axe` and an item the server
/// reports as `minecraft:wooden_axe` are the same thing, and comparing them raw would
/// make the wand silently never fire — exactly the failure a click handler cannot show
/// anybody.
fn same_item(held: &str, wand: &str) -> bool {
    fn bare(name: &str) -> &str {
        name.strip_prefix("minecraft:").unwrap_or(name)
    }
    bare(held) == bare(wand)
}

/// Runs an equipped brush when its owner right-clicks a block.
///
/// The wand handler above and this one both answer to the same event, and the ordering
/// between them decides a real conflict: a player who has both a wand and a brush equipped
/// and right-clicks would otherwise get whichever handler registered first, silently.
///
/// The wand wins. It is the deliberate act of picking a selection point, and a brush is
/// usually left equipped from earlier work, whereas a brush click while a wand is held is
/// far more often an accident than an intent. Neither handler can tell the difference, so
/// the rule has to be fixed rather than discovered.
///
/// ## Where the click lands
///
/// FAWE traces from the player's eyes along their look vector to a maximum distance, and
/// this uses the host's own `ray-trace-block` rather than computing it from yaw and pitch,
/// so a brush lands on the same block FAWE would pick even where the two disagree.
///
/// Left clicks do nothing. Only right-click paints, matching `BlockTool`, and leaving
/// left-click alone means an equipped brush does not stop a player from mining.
struct OnBrushClick;

impl EventHandler<PlayerInteractEvent> for OnBrushClick {
    fn handle(
        &self,
        _server: pumpkin_plugin_api::Server,
        mut event: EventData<PlayerInteractEvent>,
    ) -> EventData<PlayerInteractEvent> {
        // Same reason the wand handler traces: Bedrock reports this click as an *air* click
        // with no position, because its use-item-on packet is turned into a block placement
        // before any plugin sees it. `RightClickBlock` exists only on the Java path, so
        // matching only that left every brush unreachable on the platform this plugin runs
        // on.
        if !matches!(
            event.action,
            InteractAction::RightClickBlock | InteractAction::RightClickAir
        ) {
            return event;
        }
        let name = event.player.get_name();

        let equipped = state::sessions().with(&name, |session| session.brush().cloned());
        let Some(brush) = equipped else {
            return event;
        };
        // A wand click is a deliberate selection; it is not also a brush stroke. The test is
        // the *item in hand*, not whether the player has ever run `//wand`: there is no way
        // to put the wand down, so testing the session meant one `//wand` silently disabled
        // brushes for that player for the rest of the session, even after they dropped it.
        let held = event
            .player
            .get_inventory()
            .get_item_in_hand(Hand::Right)
            .map(|stack| stack.get_registry_key());
        let holding_wand = state::sessions().with(&name, |session| {
            session
                .wand()
                .is_some_and(|wand| same_item(held.as_deref().unwrap_or_default(), wand))
        });
        if holding_wand {
            return event;
        }

        let settings = state::config();
        let Some(target) = event.player.ray_trace_block(BRUSH_REACH, false) else {
            // Looking at nothing is not an error the player needs telling about: they
            // aimed past the world, which is as far as they can tell is the same as aiming
            // past a wall. Cancelling anyway would swallow the swing.
            return event;
        };
        let centre = world::from_wit(target.pos);

        // The brush's geometry is resolved against a read-only view of the world, then
        // the writes are queued. Queueing rather than writing straight is what puts a
        // brush stroke in the undo history where `//undo` can reach it.
        //
        // `probe_of` borrows an extent, so a throwaway one is built here and dropped
        // before the queue is handed an extent of its own: the queue needs ownership, and
        // the world handle inside is not `Clone`.
        let (writes, pattern_text) = {
            let extent = world::PumpkinExtent::new(event.player.get_world(), settings.world_height);
            let probe = brush::probe_of(&extent);
            let steps = state::sessions().with(&name, |session| {
                session
                    .clipboard()
                    .map(|holder| holder.clipboard().paste_steps(holder.transform(), centre))
            });
            let affected = brush::apply(&brush, centre, None, None, &probe);
            (
                brush::route(affected, steps.unwrap_or_default()),
                brush.pattern_text().map(str::to_owned),
            )
        };

        // The limit is the player's own `//limit` when they have one, and the
        // configuration's otherwise — the same resolution `//set` does, so a brush stroke
        // and a region edit obey one budget.
        let limit = state::sessions()
            .with(&name, |session| session.limit())
            .and_then(|n| usize::try_from(n).ok())
            .or_else(|| settings.block_limit());
        let mask = state::sessions().with(&name, |session| session.mask().cloned());
        let mut queued = false;
        // The count is read before `writes` is moved into the queue, because the parts of
        // it are consumed one at a time and the report has to know the whole.
        let blocks = writes.total();

        if let Some(text) = &pattern_text {
            let extent = world::PumpkinExtent::new(event.player.get_world(), settings.world_height);
            match worldedit_core::pattern::parse(text, extent.registry()) {
                Ok(pattern) => {
                    // A pattern brush fills every position its geometry chose, and
                    // respects the player's standing `//gmask` exactly as `//set` does.
                    let positions = writes.positions.into_iter();
                    jobs::enqueue_edit(
                        &name,
                        extent,
                        Box::new(positions),
                        pattern,
                        mask.clone(),
                        limit,
                    );
                    queued = true;
                }
                Err(problem) => {
                    // The pattern was checked when the brush was equipped, so reaching
                    // here means the server's registry changed underneath us. Refusing is
                    // right: writing blocks with a pattern nobody can name would be worse
                    // than doing nothing.
                    error!("{name}'s brush names a pattern that no longer resolves: {problem}");
                    return event;
                }
            }
        }

        // `//smooth` clears as well as fills, and a job carries one pattern, so the
        // clear is its own edit. Queued after the fill so a stroke's blocks read in the
        // order they were decided.
        if !writes.air.is_empty() {
            let extent = world::PumpkinExtent::new(event.player.get_world(), settings.world_height);
            let air = worldedit_core::pattern::parse("minecraft:air", extent.registry())
                .expect("a server that cannot resolve air cannot run a world");
            jobs::enqueue_edit(
                &name,
                extent,
                Box::new(writes.air.into_iter()),
                air,
                mask.clone(),
                limit,
            );
            queued = true;
        }

        if !writes.paste.is_empty() {
            let extent = world::PumpkinExtent::new(event.player.get_world(), settings.world_height);
            jobs::enqueue_paste(&name, extent, Box::new(writes.paste.into_iter()), limit);
            queued = true;
        }

        // Only swallow the click if it actually did something. Cancelling a click that
        // changed nothing stops the player using their axe for no reason, which is a worse
        // outcome than the brush appearing to misfire.
        if queued {
            // Report the stroke as the block count, the way a region edit's own report
            // reads, so a brush stroke and `//set` produce the same kind of answer.
            say(
                &event.player,
                &format!(
                    "Brush set {blocks} block{}.",
                    if blocks == 1 { "" } else { "s" }
                ),
            );
            event.cancelled = true;
        }
        event
    }
}

/// How far a brush reaches, in blocks.
///
/// FAWE's `checkMaxBrushRadius` defaults to 5 and the server config can raise it. This is
/// a ray trace rather than a radius, so it is a sight distance instead; 100 is well past
/// the point where a player is aiming at a block they can see, and short enough not to
/// paint through a hill they did not know was there.
const BRUSH_REACH: f64 = 100.0;

/// Registers the leave handler.
///
/// Separate from `on_load` so the wiring is in one place and readable.
fn register_events(context: &Context) -> Result<()> {
    let leave = match context.register_event_handler::<PlayerLeaveEvent, _>(
        OnLeave,
        EventPriority::Normal,
        false,
    ) {
        Ok(id) => {
            info!("registered the leave handler as {id}");
            Ok(())
        }
        Err(error) => Err(format!("could not register the leave handler: {error}")),
    };
    // Blocking, unlike the others: this is the only thing on this host that can stop a
    // left click from breaking the block, and it cannot do that unless it is allowed to
    // write `cancelled` back.
    let mut last = leave;
    last = last.and(
        match context.register_event_handler::<BlockBreakEvent, _>(
            OnWandBreak,
            EventPriority::Normal,
            true,
        ) {
            Ok(id) => {
                info!("registered the wand break handler as {id}");
                Ok(())
            }
            Err(error) => Err(format!(
                "could not register the wand break handler: {error}"
            )),
        },
    );
    // Two handlers on one event, so both are registered and the host calls them in
    // registration order. `OnWandClick` is registered first deliberately: a wand click
    // sets a selection point, and a brush stroke under the same click would silently
    // change the block the selection was just made against. See `OnBrushClick`.
    last = last.and(
        match context.register_event_handler::<PlayerInteractEvent, _>(
            OnWandClick,
            EventPriority::Normal,
            false,
        ) {
            Ok(id) => {
                info!("registered the wand handler as {id}");
                Ok(())
            }
            Err(error) => Err(format!("could not register the wand handler: {error}")),
        },
    );
    last.and(
        match context.register_event_handler::<PlayerInteractEvent, _>(
            OnBrushClick,
            EventPriority::Normal,
            false,
        ) {
            Ok(id) => {
                info!("registered the brush handler as {id}");
                Ok(())
            }
            Err(error) => Err(format!("could not register the brush handler: {error}")),
        },
    )
}

// The `init-plugin` export the host looks for. Gated to `wasm32` so a host-target
// test build does not export it, which would collide with any other plugin crate
// linked into the same test binary.
#[cfg(target_arch = "wasm32")]
register_plugin!(WorldEdit);
