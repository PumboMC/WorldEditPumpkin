//! The commands.
//!
//! ## Why each `//name` is its own root, and why the root has no literal of its own
//!
//! WorldEdit's shorthand is not a tree. `//set` is one Bukkit command label, `//copy`
//! is another, and `/worldedit set` is a third that reaches the same handler. The
//! same shape is reproduced here rather than approximated with a single `//` root.
//!
//! Every one is registered twice: as a bare `set`, which is what a player ends up
//! typing, and as `/set`, which is what a console types and which the host mirrors
//! back to `//set`. The bare one is the one that matters, and it is not obvious. The
//! client removes a leading slash before the server sees the line and
//! `handle_command` removes another, so `//set stone` is dispatched as `set stone` -
//! no slash anywhere in it. A root called `/set` cannot match that text. Registering
//! only the slashed spelling is why every `//` command answered "unknown command"
//! while `/worldedit set` worked.
//!
//! The root hangs its arguments off the command node itself rather than wrapping them
//! in a literal named after the command. That wrapper is the second half of the same
//! bug: the name was then required twice, once as the command and once as a
//! subcommand, and the second had no token left to match. One builder now describes
//! both shapes - see [`Attach`].
//!
//! ## Where the permission checks live
//!
//! `context.register_command` attaches exactly one node to a whole tree, so
//! [`nodes::USE`] gates `/worldedit` and every `//` root, and each subcommand checks
//! its own category node ([`nodes::EDIT`], [`nodes::SHAPE`] and so on) inside its
//! handler. Per-node permission is not an option: `command-node.require-with-handler-id`
//! is a stub on this host that returns `Err("require_with_handler_id not implemented")`
//! and traps the command.
//!
//! ## Argument order, and where it had to change
//!
//! FAWE takes a pattern *first* and trailing numbers after it, as in
//! `//sphere stone 5`. A greedy string argument swallows the rest of the line, so
//! Brigadier cannot express `[pattern] [radius...]`; it can only express
//! `[radius...] [pattern]`.
//!
//! The shape commands keep FAWE's order and give the pattern a `single-word`
//! argument instead, registering one branch per arity so the dispatcher can pick the
//! one that consumes the whole line. A `single-word` still accepts
//! `oak_log[axis=y,waterlogged=false]` in full, so the only pattern it cannot take is
//! one containing a space. Every other `//` command puts its greedy pattern last, or
//! has no trailing arguments at all, so it stays greedy.
//!
//! `//replace` is the one command that had to have its argument count changed to fit
//! that: FAWE takes the mask `from` optionally and leaves it out of the common
//! spelling, so `//replace stone` replaces everything in the selection that is not
//! air. An optional argument cannot precede a required greedy one, so the arities are
//! split into two branches under `//replace` and told apart by how many words follow.
//! The default is FAWE's `ExistingBlockMask`, *not* the global mask: `//gmask` is a
//! separate command, and it is applied when the edit is queued rather than here.
//!
//! `//rotate` only turns the clipboard about Y. FAWE also takes an X and a Z angle,
//! which would need three-axis rotation in the engine's transform.
use crate::{
    arguments::{
        Sender, block_pos, block_position, boolean, double, fail, flag, integer, integer_type,
        number, optional_integer, optional_number, optional_word, quotable, require, single_word,
        suggest_blocks, suggest_words, text, word,
    },
    config::Config,
    jobs::{self, Caption},
    nodes,
    session::Session,
    state,
    world::{PumpkinExtent, Registry},
};
use pumpkin_plugin_api::{
    Server, World as PumpkinWorld,
    command::{
        Arg, ArgumentType, Command, CommandError, CommandNode, CommandSender, ConsumedArgs,
        StringType,
    },
    commands::CommandHandler,
    text::TextComponent,
};
use std::marker::PhantomData;
use worldedit_core::{
    block::{BlockResolver, BlockValue},
    clipboard::ClipboardHolder,
    extent::Extent,
    mask::{self, Mask, MaskContext},
    math::BlockPos,
    pattern::{self, Pattern},
    region::CuboidRegion,
    shape::{self, Bounds},
    transform::{BlockAxis, Transform},
};
/// What a command worked out: the message to send, or why it did nothing.
///
/// A string rather than [`CommandError`] because the host renders
/// `CommandError::CommandFailed` as `Syntax error: Unexpected "argument"`
/// with the sentence repeated and quote marks around it. That is a parser error being
/// used to carry an explanation, and it reads as one. Everything user-facing goes
/// through [`answer`] instead.
type Said = Result<String, String>;
/// A WorldEdit subcommand.
trait Sub {
    fn node() -> &'static str;

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said;
}
/// Wraps a [`Sub`] for the plugin API.
///
/// Every handler is the same three lines — check the node, do the work, answer — and
/// writing them out once per command is how one of them ends up skipping its check.
struct Handler<T>(PhantomData<T>);
impl<T: Sub + Send + Sync> CommandHandler for Handler<T> {
    fn handle(
        &self,

        sender: CommandSender,

        server: Server,

        args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let said =
            require(&sender, &server, T::node()).and_then(|()| T::run(&sender, &server, &args));

        Ok(answer(&sender, said))
    }
}
/// A handler ready to attach to a node.
fn sub<T: Sub + Send + Sync + 'static>() -> Handler<T> {
    Handler(PhantomData)
}
/// Sends a command's result to the player.
///
/// An empty caption is how a command says "I did it, and someone else is saying the
/// rest": [`queue`] returns one, because its caption is templated on the block count the
/// edit has not yet produced and can only be rendered when the queue drains. Sending it
/// would put a blank line in the chat where FAWE would have said nothing at all.
fn answer(sender: &CommandSender, said: Said) -> i32 {
    match said {
        Ok(message) if !message.is_empty() => {
            sender.send_message(TextComponent::text(&message));

            0
        }

        Ok(_) => 0,

        Err(problem) => fail(sender, problem),
    }
}
/// A root command: its tree, and the node the host gates it on.
pub struct Root {
    /// The permission node attached to the whole tree.
    pub node: &'static str,

    /// The command itself.
    pub command: Command,
}
/// The names a `//` shorthand is registered under.
///
/// Split out from [`root`] because the naming is the one piece of the registration that
/// can be checked without a running host. Building a `Command` panics on the host test
/// target, so these are deliberately plain functions over a string.
///
/// `bare` is the name a *player* ends up typing after the host has taken its slashes.
/// The client removes one leading slash before the server sees the line and
/// `CommandDispatcher::handle_command` removes another, so `//set stone` is dispatched as
/// `set stone` - with no slash left on it at all. A root called `/set` cannot match that;
/// only a bare `set` can. This is the single fact that made every `//` command answer
/// "unknown command" while `/worldedit set` worked.
///
/// `slashed` is the same name with one slash. It is there for the console, which types
/// into the dispatcher without a client in front of it and so arrives as `//set stone`;
/// the host mirrors this spelling back to `//set` by itself, which is how that path is
/// covered without registering a third root.
fn bare_name(name: &str) -> String {
    name.trim_start_matches("//").to_string()
}
/// The single-slash spelling, which the host mirrors to `//name` in turn.
fn slashed_name(name: &str) -> String {
    name.replacen("//", "/", 1)
}
/// Builds the roots for one `//name` shorthand.
///
/// `name` is the `//`-spelled label from [`Spec::names`], including any aliases, so
/// `//pos1` and `//1` each get roots of their own.
///
/// ## Why two roots per name
///
/// One for the bare spelling, which is what a player's `//name` becomes by the time it
/// reaches the dispatcher, and one for the single-slash spelling, which is what a
/// console types and which the host mirrors back to `//name`. Registering only one of
/// them leaves a whole class of input unreachable, and the two cannot be merged: a name
/// with a slash in it is a different literal to the one without.
///
/// ## Why an alias is a whole root rather than a dispatcher alias
///
/// `register_with_aliases` builds a bare node carrying the handler and none of the
/// arguments, so `//1 10 64 20` would set a position at the player's feet and drop the
/// coordinates. Each alias is built from the same builder instead.
///
/// ## Why the tree is attached to the command itself
///
/// A root has no subcommand token left to match. `//set stone` arrives as `set stone`;
/// `set` is the command name and `stone` is the pattern, so anything hung below the
/// root has to be the pattern argument itself. Wrapping the arguments in a literal
/// called `set` - which is what hanging `set_node` under the root used to do - asks the
/// dispatcher to match a second `set` that is not there, and the command then runs
/// nothing at all without reporting an error.
///
/// `/worldedit set stone` is the opposite shape and still needs its literal: there
/// `worldedit` is the command name and `set` really is a second token. That is why one
/// builder describes both trees rather than the root reusing the `/worldedit` subtree.
fn root(spec: &Spec, name: &str) -> Vec<Root> {
    [bare_name(name), slashed_name(name)]
        .into_iter()
        .map(|registered| Root {
            node: spec.node,

            command: (spec.short)(Command::new(&[registered], spec.description)),
        })
        .collect()
} // ## Plumbing 
/// The sender's world, as something the engine can read and write.
fn here(sender: &CommandSender, settings: &Config) -> Result<PumpkinExtent, String> {
    let world = Sender::of(sender)
        .world
        .ok_or_else(|| missing_world().to_string())?;

    Ok(PumpkinExtent::new(world, settings.world_height).with_actor(&sender.get_name()))
}
/// The world the player is standing in, as a handle.
fn standing_in(sender: &CommandSender) -> Result<PumpkinWorld, String> {
    Sender::of(sender)
        .world
        .ok_or_else(|| missing_world().to_string())
}
/// What FAWE says when a command needs a world and the sender has none.
///
/// `worldedit.error.missing-world` (`strings.json:253`), which is what
/// `MissingWorldException` (`MissingWorldException.java:30`) and `ParserContext:202` raise.
/// It names the command that fixes it, which the previous wording did not.
fn missing_world() -> &'static str {
    "You need to provide a world (Try //world)"
}
/// What FAWE says when a command needs a player and the sender is not one.
///
/// `worldedit.command.player-only` (`strings.json:555`), raised by
/// `PlatformCommandManager:838` for any command whose signature names a `Player`. This is
/// the `//wand` case, which declares `Player player` (`SelectionCommands.java:330`).
fn player_only() -> &'static str {
    "This command must be used with a player."
}

/// The captions a queued edit prints when it drains, one per FAWE key.
///
/// Every caption here is the `strings.json` value verbatim, `{0}` included, because the
/// `{0}` is what [`jobs::Caption`] fills with the edit's real changed-block count once
/// the last tick has run. They are named constants rather than inline literals so a
/// test can render one without a running host, which is the same reason
/// [`wand_caption`] and [`count_caption`] are functions.
///
/// A key that repeats another key's text — `worldedit.walls.changed` and
/// `worldedit.faces.changed` both read "{0} blocks have been changed." — still gets its
/// own constant, because they are different strings that happen to render alike and a
/// test has to be able to name which one a command uses.
mod caption {
    use super::Caption;

    /// `worldedit.set.done` (`strings.json:483`), printed by `RegionCommands.set`
    /// (`:125`) and, through it, by `//air` (`:140-142`).
    ///
    /// The one caption in this list Java guards: `if (affected != 0)`
    /// (`RegionCommands.java:124`). So `//set air` over air says nothing at all.
    pub(super) const SET_DONE: Caption = Caption::IfChanged("Operation completed ({0}).");

    /// `worldedit.replace.replaced` (`strings.json:491`), printed by
    /// `RegionCommands.replace` (`:318`).
    pub(super) const REPLACED: Caption = Caption::Always("{0} blocks have been replaced.");

    /// `worldedit.replacenear.replaced` (`strings.json:466`), printed by
    /// `UtilityCommands.replaceNear` (`:527`). A different key from [`REPLACED`] that
    /// happens to render identically.
    pub(super) const REPLACED_NEAR: Caption = Caption::Always("{0} blocks have been replaced.");

    /// `worldedit.overlay.overlaid` (`strings.json:498`), printed by
    /// `RegionCommands.overlay` (`:336`).
    pub(super) const OVERLAID: Caption = Caption::Always("{0} blocks have been overlaid.");

    /// `worldedit.center.changed` (`strings.json:500`), printed by
    /// `RegionCommands.center` (`:393`). The count sits inside the caption's own
    /// parentheses and there is no full stop.
    pub(super) const CENTER_CHANGED: Caption = Caption::Always("Center set. ({0} blocks changed)");

    /// `worldedit.walls.changed` (`strings.json:496`), printed by
    /// `RegionCommands.walls` (`:425`).
    pub(super) const WALLS_CHANGED: Caption = Caption::Always("{0} blocks have been changed.");

    /// `worldedit.faces.changed` (`strings.json:497`), printed by
    /// `RegionCommands.faces` (`:444`).
    pub(super) const FACES_CHANGED: Caption = Caption::Always("{0} blocks have been changed.");

    /// `worldedit.hollow.changed` (`strings.json:505`), printed by
    /// `RegionCommands.hollow` (`:873`).
    pub(super) const HOLLOW_CHANGED: Caption = Caption::Always("{0} blocks have been changed.");

    /// `worldedit.line.changed` (`strings.json:485`), printed by
    /// `RegionCommands.line` (`:258`).
    pub(super) const LINE_CHANGED: Caption = Caption::Always("{0} blocks have been changed.");

    /// `worldedit.fill.created` (`strings.json:459`), printed by
    /// `UtilityCommands.fill` (`:249`). The key says "created"; the caption says
    /// "filled". The caption wins.
    pub(super) const FILLED: Caption = Caption::Always("{0} blocks have been filled.");

    /// `worldedit.fillr.created` (`strings.json:460`), printed by
    /// `UtilityCommands.fillr` (`:339`). Also "filled", and also a different key from
    /// [`FILLED`].
    pub(super) const FILLED_RECURSIVE: Caption = Caption::Always("{0} blocks have been filled.");

    /// `worldedit.removeabove.removed` (`strings.json:463`), printed by
    /// `UtilityCommands.removeAbove` (`:439`).
    ///
    /// Not `worldedit.remove.removed` (`:473`), which is "{0} entities have been marked
    /// for removal." and belongs to `//remove` — a command that deletes *entities*
    /// (`UtilityCommands.java:742-757`), which this plugin does not have.
    pub(super) const REMOVED_ABOVE: Caption = Caption::Always("{0} blocks have been removed.");

    /// `worldedit.removebelow.removed` (`strings.json:464`), printed by
    /// `UtilityCommands.removeBelow` (`:465`).
    pub(super) const REMOVED_BELOW: Caption = Caption::Always("{0} blocks have been removed.");

    /// `worldedit.sphere.created` (`strings.json:527`), printed by
    /// `GenerationCommands.sphere` (`:313`). `//hsphere` delegates to it (`:263`).
    pub(super) const SPHERE_CREATED: Caption = Caption::Always("{0} blocks have been created.");

    /// `worldedit.cyl.created` (`strings.json:524`), printed by
    /// `GenerationCommands.cyl` (`:195`) and by `//hcyl` (`:148`).
    pub(super) const CYL_CREATED: Caption = Caption::Always("{0} blocks have been created.");

    /// `worldedit.cone.created` (`strings.json:522`), printed by
    /// `GenerationCommands.cone` (`:242`). `printInfo` there rather than `print`; it is
    /// still the caption the player reads.
    pub(super) const CONE_CREATED: Caption = Caption::Always("{0} blocks have been created.");

    /// `worldedit.pyramid.created` (`strings.json:534`), printed by
    /// `GenerationCommands.pyramid` (`:450`). `//hpyramid` delegates to it (`:425`).
    pub(super) const PYRAMID_CREATED: Caption = Caption::Always("{0} blocks have been created.");
}
/// Where a command that needs a position puts it: the argument, or the sender's feet.
///
/// FAWE's "placement position". Reading it in one place is what keeps `//sphere`
/// centred on the player and `//paste` anchored to the clipboard origin from drifting
/// apart.
fn anchor(sender: &CommandSender, args: &ConsumedArgs, name: &str) -> Result<BlockPos, String> {
    if present(args, name) {
        let [x, y, z] = block_position(args, name)?;

        return Ok(BlockPos::new(x, y, z));
    }

    feet(sender)
}
/// The sender's own block position.
///
/// FAWE's `LocalSession.getPlacementPosition` (`LocalSession.java:1086-1097`) reads the
/// actor's block location and throws `IncompleteRegionException` for a sender that has
/// none, which the command converter prints as `worldedit.error.incomplete-region`
/// (`strings.json:241`). That is the caption the player actually sees for `//sphere` from
/// the console, so it is the one used here too.
///
/// Note that it is *not* `worldedit.command.player-only` (:555). That caption is raised
/// only for a command whose signature names a `Player`
/// (`PlatformCommandManager.java:834-840`), and none of the commands that reach here do:
/// `GenerationCommands.sphere` (`GenerationCommands.java:273-276`),
/// `UtilityCommands.fill` (`:225-227`) and `GenerationCommands.cone` (`:206`) all take an
/// `Actor`. A console sender fails this path as a *missing selection*, not as a missing
/// player, which is exactly what `getPlacementPosition` throws.
fn feet(sender: &CommandSender) -> Result<BlockPos, String> {
    let [x, y, z] = Sender::of(sender)
        .position
        .ok_or_else(|| missing_selection().to_string())?;

    Ok(BlockPos::new(x, y, z))
}
/// Whether an argument was actually supplied.
///
/// `get_value` answers a key it does not have with an empty string, so emptiness is
/// how absence is spelled. Needed because `block_position` cannot tell "an absent position"
/// from "a malformed one", and a caller that treats both as "absent" would otherwise silently ignore a typo.
fn present(args: &ConsumedArgs, name: &str) -> bool {
    match args.get_value(name) {
        Arg::Simple(value) => !value.is_empty(),

        Arg::BlockPos(_) | Arg::Num(_) | Arg::Bool(_) => true,

        _ => false,
    }
}
/// The player's selection, or the message FAWE gives when there is not one.
fn selection(sender: &CommandSender) -> Result<CuboidRegion, String> {
    selection_for(&sender.get_name())
}

/// [`selection`], by player name, for the callers that have only the name.
fn selection_for(player: &str) -> Result<CuboidRegion, String> {
    state::sessions()
        .get(player)
        .and_then(Session::selection)
        .ok_or_else(|| missing_selection().to_string())
}
/// What FAWE says when a command needs a selection and there is not one.
///
/// `worldedit.error.incomplete-region` (`strings.json:241`), which is the caption
/// `IncompleteRegionException` prints — the exception `LocalSession.getSelection`
/// (`LocalSession.java:827`) throws while the selector is undefined.
fn missing_selection() -> &'static str {
    "Make a region selection first."
}
/// The mask an edit runs under: the command's own, the global one, or both.
///
/// FAWE intersects rather than replacing, so `//gmask stone` still narrows a
/// `//replace dirt stone`. Replacing would throw the command's own mask away for the sake
/// of the global one, which is the opposite of what a standing `//gmask` is for.
fn mask_for(player: &str, own: Option<Mask>) -> Option<Mask> {
    let global = state::sessions()
        .get(player)
        .and_then(Session::mask)
        .cloned();

    match (own, global) {
        (Some(own), Some(global)) => Some(Mask::And(vec![own, global])),

        (Some(own), None) => Some(own),

        (None, global) => global,
    }
}
/// The most blocks this player's next edit may change.
///
/// `//limit 0` is an explicit "zero" and must not fall back to the configured
/// one, which is why the session stores `Option<i64>` rather than `Option<usize>`.
fn limit_for(player: &str) -> Option<usize> {
    match state::sessions().get(player).and_then(Session::limit) {
        Some(explicit) => usize::try_from(explicit).ok().filter(|n| *n > 0),

        None => state::config().block_limit(),
    }
}
/// Reads a pattern, naming the block the server does not have.
fn pattern_of(text: &str, resolver: &Registry) -> Result<Pattern, String> {
    pattern::parse(text, resolver).map_err(|problem| problem.to_string())
}
/// Reads a mask, naming the part of it that could not be read.
fn mask_of(text: &str, resolver: &Registry, extent: &PumpkinExtent) -> Result<Mask, String> {
    from_mask(Some(text), resolver, (extent.min_y(), extent.max_y()))
}

/// Reads the `from` mask of a command that takes it optionally.
///
/// FAWE leaves the mask out of the common spelling and defaults it to an
/// `ExistingBlockMask` when it is absent, which is `RegionCommands.replace`'s
/// `if (from == null) { from = new ExistingBlockMask(editSession); }`
/// (RegionCommands.java:311-312). That mask matches every block that is not air, so
/// `//replace stone` fills a selection in without touching the air in it.
///
/// It is not the global mask. `//gmask` is a separate command with its own node, and
/// this default does not consult it; the global mask is applied further down, when the
/// edit is queued.
fn from_mask(from: Option<&str>, resolver: &Registry, bounds: (i32, i32)) -> Result<Mask, String> {
    match from {
        None => Ok(Mask::Existing),
        Some(text) => mask::parse(text, resolver, bounds).map_err(|problem| problem.to_string()),
    }
}
/// Queues a pattern edit over `positions`, to be reported when it drains.
///
/// `extent` is taken by value because the edit queue carries it across ticks, and the
/// world handle inside it is neither `Copy` nor `Clone` so there is no way to hand out
/// a second one. It is borrowed first to read the pattern, then moved into the queue.
///
/// Returns an empty caption on purpose: this used to print "Queued `<player>`-'s edit;
/// it finishes over the next few ticks." here, and the queue printed a second line
/// when the edit finished. Two lines for one gesture, neither of them FAWE's. The
/// caption that *is* FAWE's is templated on the changed-block count, so it can only be
/// rendered in [`jobs::Pending::finish`] — see [`caption`].
fn queue(
    sender: &CommandSender,

    extent: PumpkinExtent,

    positions: Box<dyn Iterator<Item = BlockPos> + Send + 'static>,

    pattern: &str,

    own_mask: Option<Mask>,

    caption: jobs::Caption,
) -> Said {
    let player = sender.get_name();

    let pattern = pattern_of(pattern, extent.registry())?;

    jobs::enqueue_edit_saying(
        &player,
        extent,
        positions,
        pattern,
        mask_for(&player, own_mask),
        limit_for(&player),
        caption,
    );

    Ok(String::new())
} // ## Geometry over a cuboid selection 
/// The middle column of a cuboid, as `//center` fills it.
///
/// The centre is `(min + max) / 2` with the division truncating toward zero, which is
/// what `BlockVector3.div` does. Rounding instead would put a 1�—1�—N selection's
/// column one block off for negative coordinates.
fn center_positions(region: CuboidRegion) -> Vec<BlockPos> {
    let min = region.minimum();

    let max = region.maximum();

    let x = (min.x + max.x) / 2;

    let z = (min.z + max.z) / 2;

    (min.y..=max.y).map(|y| BlockPos::new(x, y, z)).collect()
}
/// The four vertical faces of a cuboid, as `//walls` fills them.
///
/// The top and bottom are excluded, and the corners are emitted once. A selection
/// only two blocks wide or deep has no interior to exclude, so the whole thing is the
/// wall — that is `CuboidRegion.getWalls`' own guard.
fn wall_positions(region: CuboidRegion) -> Vec<BlockPos> {
    let min = region.minimum();

    let max = region.maximum();

    let dimensions = region.dimensions();

    if dimensions.x <= 2 || dimensions.z <= 2 {
        return walk_positions(region);
    }

    let mut positions = Vec::new();

    for y in min.y..=max.y {
        for z in min.z..=max.z {
            for x in min.x..=max.x {
                // The X faces run the full depth and so claim the corners. The Z faces

                // start one block in, which is what FAWE's `UNIT_X` offsets do, so a

                // corner is emitted once rather than twice.

                let on_x_face = x == min.x || x == max.x;

                let on_z_face = z == min.z || z == max.z;

                if on_x_face || (on_z_face && x > min.x && x < max.x) {
                    positions.push(BlockPos::new(x, y, z));
                }
            }
        }
    }

    positions
}
/// All six faces of a cuboid, as `//faces` fills them.
///
/// Every block with any coordinate on the selection's boundary, so edges and corners
/// are emitted once.
fn face_positions(region: CuboidRegion) -> Vec<BlockPos> {
    let min = region.minimum();

    let max = region.maximum();

    let mut positions = Vec::new();

    for x in min.x..=max.x {
        for y in min.y..=max.y {
            for z in min.z..=max.z {
                let on_edge = x == min.x
                    || x == max.x
                    || y == min.y
                    || y == max.y
                    || z == min.z
                    || z == max.z;

                if on_edge {
                    positions.push(BlockPos::new(x, y, z));
                }
            }
        }
    }

    positions
}
/// Every block of a cuboid, in the order a walk visits them.
fn walk_positions(region: CuboidRegion) -> Vec<BlockPos> {
    jobs::walk(region).collect()
}
/// The chunk the player is standing in, as `//chunk` selects it.
///
/// A chunk is 16�—16 in X and Z, so its origin is the position with the low four bits
/// cleared. It spans the whole world height.
fn chunk_region(position: BlockPos, bounds: Bounds) -> CuboidRegion {
    CuboidRegion::new(
        BlockPos::new(position.x & !15, bounds.min_y, position.z & !15),
        BlockPos::new(
            (position.x & !15) + 15,
            bounds.max_y,
            (position.z & !15) + 15,
        ),
    )
}
/// A selection grown or shrunk by a per-axis amount, where positive grows.
///
/// `None` when shrinking would invert an axis, which is what FAWE's
/// `RegionOperationException` reports.
fn adjust(region: CuboidRegion, dx: i32, dy: i32, dz: i32, grow: bool) -> Option<CuboidRegion> {
    let min = region.minimum();

    let max = region.maximum();

    let (new_min, new_max) = if grow {
        (min.offset(-dx, -dy, -dz), max.offset(dx, dy, dz))
    } else {
        (min.offset(dx, dy, dz), max.offset(-dx, -dy, -dz))
    };

    if new_min.x > new_max.x || new_min.y > new_max.y || new_min.z > new_max.z {
        return None;
    }

    Some(CuboidRegion::new(new_min, new_max))
}
/// The eight compass directions, as the sign each one moves a selection.
const CARDINALS: [(&str, (i32, i32, i32)); 4] = [
    ("north", (0, 0, -1)),
    ("south", (0, 0, 1)),
    ("west", (-1, 0, 0)),
    ("east", (1, 0, 0)),
];
/// Which way a direction word names, as the sign each axis moves a selection.
///
/// FAWE's `MultiDirection`, which is spelled as compass points rather than axes:
/// `north`, `south`, `east`, `west`, `up`, `down`, any diagonal of a horizontal pair
/// (`northeast`), and the relative names `me`, `forward`, `back`, `left` and `right`.
///
/// A horizontal direction is resolved against the player's yaw by the caller, because
/// `me` is a cardinal facing rather than a fixed one. `None` means the word is not a
/// direction, which the caller reports rather than guessing.
fn parse_direction(text: &str, yaw: f32) -> Option<(i32, i32, i32)> {
    let name = text.to_ascii_lowercase();

    match name.as_str() {
        // `Direction.AIM` is `"me"`, which FAWE resolves to the cardinal direction the

        // player is facing. It is horizontal: `//contract 5` while looking north takes

        // five off the north and south ends, not off the top and the bottom.
        "me" | "aim" | "m" | "f" | "forward" => Some(facing(yaw)),

        "b" | "back" => Some(facing(yaw + 180.0)),

        "l" | "left" => Some(facing(yaw - 90.0)),

        "r" | "right" => Some(facing(yaw + 90.0)),

        "up" | "u" | "down" | "d" | "downleft" | "downright" | "upleft" | "upright" => {
            Some(if name.starts_with('d') {
                (0, -1, 0)
            } else {
                (0, 1, 0)
            })
        }

        // A diagonal is a horizontal pair spelled together, which is how FAWE's name

        // table is built.
        "nw" | "northwest" | "north-west" => Some((-1, 0, -1)),

        "ne" | "northeast" => Some((1, 0, -1)),

        "sw" | "southwest" => Some((-1, 0, 1)),

        "se" | "southeast" => Some((1, 0, 1)),

        _ => CARDINALS
            .iter()
            // Prefixes are accepted, as FAWE's table does: `nor` is `north`, and `n`
            // is north. This stays unambiguous because the diagonals were matched by
            // the exact arm above, and no cardinal is a prefix of another cardinal.
            .find(|(cardinal, _)| !name.is_empty() && cardinal.starts_with(&name))
            .map(|(_, signs)| *signs),
    }
}
/// The axis a horizontal facing runs along, which is the axis `//flip` mirrors.
///
/// A facing north or south moves along Z, so mirroring "Z" means negating Z. A facing of zero on an axis — which is what a
/// diagonal or a vertical facing gives — falls back to X, because there is no axis
/// to prefer and `//flip` has to pick one.
fn facing_axis(direction: (i32, i32, i32)) -> BlockAxis {
    if direction.2 != 0 {
        BlockAxis::Z
    } else {
        BlockAxis::X
    }
}
/// The cardinal direction a player of the given yaw is facing.
///
/// Minecraft yaw is 0 looking south and increases clockwise, so each of the four
/// sectors is 90 degrees wide. Wrapped first because a yaw accumulated by movement can
/// be any value at all.
fn facing(yaw: f32) -> (i32, i32, i32) {
    let yaw = yaw.rem_euclid(360.0);

    match yaw {
        45.0..=135.0 => (-1, 0, 0),

        135.0..=225.0 => (0, 0, -1),

        225.0..=315.0 => (1, 0, 0),

        _ => (0, 0, 1),
    }
}
/// Contracts a cuboid along the axes a direction names.
///
/// The direction's sign selects the axes and the amount is a magnitude: `//contract 5
/// north` takes five off both the north and the south end of the Z axis. Treating the
/// sign as a signed amount instead would make `//contract 5 north` *grow* the
/// selection, because a negative delta is what `Contract` treats as an expansion.
fn contract(region: CuboidRegion, direction: (i32, i32, i32), amount: i32) -> Option<CuboidRegion> {
    adjust(
        region,
        direction.0.abs() * amount,
        direction.1.abs() * amount,
        direction.2.abs() * amount,
        false,
    )
}
/// Moves a cuboid along the axes a direction names.
fn shift(region: CuboidRegion, direction: (i32, i32, i32), amount: i32) -> CuboidRegion {
    CuboidRegion::new(
        region.minimum().offset(
            direction.0 * amount,
            direction.1 * amount,
            direction.2 * amount,
        ),
        region.maximum().offset(
            direction.0 * amount,
            direction.1 * amount,
            direction.2 * amount,
        ),
    )
}
/// A block's state id as a `u16` from a name, for `//info`.
fn describe(extent: &PumpkinExtent, position: BlockPos) -> String {
    let state = extent.get_block(position);

    match extent.registry().block_name(state.state) {
        Some(name) => format!("{} (block state {})", name, state.state),

        None => format!("unknown (block state {})", state.state),
    }
}
/// What FAWE says after a wand click, and only after a wand click.
///
/// WorldEdit's `explainPrimarySelection` / `explainSecondarySelection` name the point that
/// was just set, then add the region's volume when the selection is now complete. It is a
/// different sentence from `selection_summary`, which describes the selection without
/// saying what just changed — and the difference is the point, because a player who has
/// just clicked wants to know which of the two corners moved.
///
/// The position is passed in rather than read back from the session so the sentence can
/// name the *clicked* block even if the session went on to reorder it. pub(crate)
pub(crate) fn wand_summary(session: &Session, clicked: BlockPos, primary: bool) -> String {
    let complete = session
        .selection()
        .map(|region| region.dimensions().volume());

    let which = if primary { "First" } else { "Second" };

    match complete {
        Some(volume) => format!("{which} position set to {} ({volume}).", show(clicked)),

        None => format!("{which} position set to {}.", show(clicked)),
    }
}
/// What FAWE prints after a selection changes: its size and its corners. pub(crate)
fn selection_summary(session: &Session) -> String {
    let Some(region) = session.selection() else {
        let point = session.pos1().or(session.pos2());

        return match point {
            Some(position) => format!("First position is at {}, one more needed.", show(position)),

            None => "No selection.".to_string(),
        };
    };

    let dimensions = region.dimensions();

    format!(
        "Selection is {}x{}x{} blocks ({} blocks), from {} to {}.",
        dimensions.x,
        dimensions.y,
        dimensions.z,
        dimensions.volume(),
        show(region.minimum()),
        show(region.maximum())
    )
}
/// A position as `(x, y, z)`.
fn show(position: BlockPos) -> String {
    format!("({}, {}, {})", position.x, position.y, position.z)
}
/// Replaces the player's selection and reports it.
fn select(sender: &CommandSender, first: Option<BlockPos>, second: Option<BlockPos>) -> Said {
    let summary = state::sessions().with(&sender.get_name(), |session| {
        if let Some(position) = first {
            session.set_pos1(position);
        }

        if let Some(position) = second {
            session.set_pos2(position);
        }

        selection_summary(session)
    });

    Ok(summary)
} // ## Selection commands  // ## Player-relative utilities // // These take a radius or an apothem rather than a selection, and work around the block // the player is standing in. FAWE puts them in `UtilityCommands` rather than // `RegionCommands` for exactly that reason, and every one of them resolves its shape from // the placement position — which is [`feet`] here. 
/// `//overlay <pattern>` — lay a pattern on the top block of every column in the selection.
///
/// FAWE's `overlayCuboidBlocks` walks the flat region with a `SurfaceRegionFunction`
/// wrapped in a `RegionOffset` of `+1`, so the block that changes is the one *above* the
/// surface rather than the surface itself. Overwriting the surface would be `//replace`,
/// and the difference is the whole point: the terrain keeps its own top block and gains
/// a new one.
struct Overlay;
impl Sub for Overlay {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        let pattern = word(args, "pattern")?;

        let bounds = extent.bounds();

        let air = extent.air_state();

        let (low, high) = (region.minimum(), region.maximum());

        let mut positions = Vec::new();

        for x in low.x..=high.x {
            for z in low.z..=high.z {
                let surface = (low.y..=high.y)
                    .rev()
                    .find(|y| extent.get_block(BlockPos::new(x, *y, z)).state != air);

                if let Some(y) = surface {
                    let above = BlockPos::new(x, y + 1, z);

                    // `//overlay` cannot reach past the top of the world, and the +1 is

                    // what would.

                    if bounds.holds(above) {
                        positions.push(above);
                    }
                }
            }
        }

        queue(
            sender,
            extent,
            Box::new(positions.into_iter()),
            &pattern,
            None,
            caption::OVERLAID,
        )
    }
}
/// `//naturalize` — grass on top, three dirt, stone below.
///
/// FAWE's `Naturalizer` is a `LayerFunction`: the region is flattened to its Y range, each
/// column is walked bottom-up, and a block is replaced by depth rather than by position.
/// Depth 0 is the top of the column, so the grass layer is a column's own topmost block.
///
/// Only grass, dirt and stone are touched — FAWE's `BlockTypeMask` — so a build on the
/// surface keeps its cobbled plaza rather than being overwritten.
struct Naturalize;
impl Sub for Naturalize {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, _args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        let (low, high) = (region.minimum(), region.maximum());

        // Columns are grouped by the block they become, so each pattern is one edit job

        // and a column still comes out grass, dirt, dirt, dirt, stone from the top.

        let mut by_pattern: std::collections::BTreeMap<&str, Vec<BlockPos>> =
            std::collections::BTreeMap::new();

        for x in low.x..=high.x {
            for z in low.z..=high.z {
                let mut depth = 0;

                // Walked down from the top, so depth counts from there exactly as FAWE's

                // LayerFunction does.

                for y in (low.y..=high.y).rev() {
                    let position = BlockPos::new(x, y, z);

                    let Some(name) = extent
                        .registry()
                        .block_name(extent.get_block(position).state)
                    else {
                        continue;
                    };

                    if !NATURALIZE_MASK.contains(&name) {
                        continue;
                    }

                    by_pattern
                        .entry(match depth {
                            0 => "The apothem has to be a whole number.",

                            1..=3 => "The apothem has to be no more than the radius.",

                            _ => "The apothem is too large for the radius.",
                        })
                        .or_default()
                        .push(position);

                    depth += 1;
                }
            }
        }

        let player = sender.get_name();

        let mut changed = 0;

        for (fill, positions) in by_pattern {
            let pattern = pattern_of(fill, extent.registry())?;

            changed += positions.len();

            jobs::enqueue_edit(
                &player,
                here(sender, &settings)?,
                Box::new(positions.into_iter()),
                pattern,
                None,
                limit_for(&player),
            );
        }

        Ok(format!("Naturalized {changed} blocks."))
    }
}
/// The blocks `//naturalize` rewrites, as FAWE's `Naturalizer` names them in its mask.
const NATURALIZE_MASK: [&str; 3] = ["minecraft:grass_block", "minecraft:dirt", "minecraft:stone"];
/// `//fill <pattern> <radius> [depth] [direction]` — fill a cylinder around the player.
///
/// FAWE's radius is clamped to at least one and the depth defaults to one, so
/// `//fill stone 3` puts a one-deep ring around your feet.
struct Fill;
impl Sub for Fill {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let radius = number(args, "radius")?.max(1.0);

        let depth = optional_integer(args, "depth", 1)?;

        // Direction defaults to down, which is what makes `//fill` a fill and not a

        // bridge: the cylinder extends away from your feet rather than through them.

        let (dx, dy, dz) = direction_arg_default(sender, args, "direction", (0, -1, 0))?;

        let bounds = extent.bounds();

        let centre = feet(sender)?;

        let mut positions = Vec::new();

        for anchor in disk(centre, radius, bounds) {
            for step in 0..depth {
                let position = anchor.offset(dx * step, dy * step, dz * step);

                if bounds.holds(position) {
                    positions.push(position);
                }
            }
        }

        queue(
            sender,
            extent,
            Box::new(positions.into_iter()),
            &word(args, "pattern")?,
            None,
            caption::FILLED,
        )
    }
}
/// `//fillr <pattern> <radius> [depth]` — fill from the surface upward.
///
/// The difference from `//fill` is the origin: `//fill` starts at your feet and extends
/// along the direction, while this starts at the highest solid block and works up, so
/// filling a pond from a boat works instead of filling the boat.
struct FillRaise;
impl Sub for FillRaise {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let radius = number(args, "radius")?.max(1.0);

        let depth = optional_integer(args, "depth", 1)?;

        let centre = feet(sender)?;

        let bounds = extent.bounds();

        let air = extent.air_state();

        let mut positions = Vec::new();

        for anchor in disk(centre, radius, bounds) {
            // The surface is found by walking up from the feet, so a pond fills from its

            // own water line rather than from the boat.

            let Some(top) = (centre.y..=bounds.max_y).find(|y| {
                extent
                    .get_block(BlockPos::new(anchor.x, *y, anchor.z))
                    .state
                    != air
            }) else {
                continue;
            };

            for step in 0..depth {
                let position = anchor.with_y(top - depth + 1 + step);

                if bounds.holds(position) {
                    positions.push(position);
                }
            }
        }

        queue(
            sender,
            extent,
            Box::new(positions.into_iter()),
            &word(args, "pattern")?,
            None,
            caption::FILLED_RECURSIVE,
        )
    }
}
/// The disc of columns `radius` blocks around `centre`, at the centre's own height.
fn disk(centre: BlockPos, radius: f64, bounds: Bounds) -> Vec<BlockPos> {
    let reach = radius.ceil() as i32;

    let mut out = Vec::new();

    for x in -reach..=reach {
        for z in -reach..=reach {
            if f64::from(x * x + z * z) > radius * radius {
                continue;
            }

            let position = BlockPos::new(centre.x + x, centre.y, centre.z + z);

            if bounds.holds(position) {
                out.push(position);
            }
        }
    }

    out
}
/// `//removeabove [apothem] [height]` and `//removebelow [apothem] [height]`.
///
/// One
/// type for both: FAWE's two are the same air-filled cuboid anchored at the player,
/// differing only in whether the height counts up from the feet or down from them.
struct Remove<const ABOVE: bool>;
impl<const ABOVE: bool> Sub for Remove<ABOVE> {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        // Clamped to at least one, matching FAWE: a size of zero is not a cuboid and the

        // command would otherwise silently do nothing when asked to do something.

        let apothem = optional_integer(args, "apothem", 1)?.max(1);

        let bounds = extent.bounds();

        let height = optional_integer(args, "height", 0)?
            .max(0)
            .min(bounds.max_y - bounds.min_y + 1);

        let feet = feet(sender)?;

        let (low, high) = if ABOVE {
            (
                feet.offset(-apothem + 1, 0, -apothem + 1),
                feet.offset(apothem - 1, height, apothem - 1),
            )
        } else {
            (
                feet.offset(-apothem + 1, -height, -apothem + 1),
                feet.offset(apothem - 1, 0, apothem - 1),
            )
        };

        queue(
            sender,
            extent,
            Box::new(jobs::walk(CuboidRegion::new(low, high))),
            "The apothem has to be at least one.",
            None,
            // FAWE has one key per direction: `worldedit.removeabove.removed` (:463) and
            // `worldedit.removebelow.removed` (:464). They render identically.
            if ABOVE {
                caption::REMOVED_ABOVE
            } else {
                caption::REMOVED_BELOW
            },
        )
    }
}
/// `//overlay <pattern>`
fn overlay_node<T: Attach>(top: T) -> T {
    top.then(block_arg("pattern", quotable()).execute(sub::<Overlay>()))
}
/// `//naturalize`
fn naturalize_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Naturalize>())
}
/// `//fill <pattern> <radius> [depth] [direction]`
fn fill_node<T: Attach>(top: T) -> T {
    let pattern = || block_arg("pattern", quotable());

    let radius = || CommandNode::argument("radius", double());

    let depth = || CommandNode::argument("depth", integer_type());

    let direction = || CommandNode::argument("direction", single_word());

    let done = || sub::<Fill>();

    top.then(
        pattern()
            .execute(done())
            .then(radius().execute(done()))
            .then(depth().execute(done()))
            .then(direction().execute(done())),
    )
}
/// `//fillr <pattern> <radius> [depth]`
fn fillr_node<T: Attach>(top: T) -> T {
    let pattern = || block_arg("pattern", quotable());

    let radius = || CommandNode::argument("radius", double());

    let depth = || CommandNode::argument("depth", integer_type());

    let done = || sub::<FillRaise>();

    top.then(
        pattern()
            .execute(done())
            .then(radius().execute(done()))
            .then(depth().execute(done())),
    )
}
/// `//removeabove [apothem] [height]` and `//removebelow`
fn remove_node<T: Sub + Send + Sync + 'static, R: Attach>(top: R) -> R {
    // Named `apothem` and `height` because that is what the handler reads them by. A tree
    // that calls them something else parses perfectly and then finds nothing, which is
    // what happened here: the numbers were accepted and silently ignored.
    top.execute(sub::<T>()).then(
        CommandNode::argument("apothem", integer_type())
            .execute(sub::<T>())
            .then(CommandNode::argument("height", integer_type()).execute(sub::<T>())),
    )
}
/// A block argument, with block completion attached.
///
/// The name is a parameter rather than baked in, and that is the whole point. A helper
/// that hard-coded `pattern` silently renamed `//replace`'s `from` and `to` and
/// `//replacenear`'s `from`; both handlers went on reading the names they had always
/// read, so both commands answered as if their own arguments were missing. Spelling the
/// name at every call site leaves no helper that *can* drift the tree from the handler.
///
/// Every block-taking command goes through here rather than calling
/// `CommandNode::argument` directly, so completion cannot be forgotten on the one
/// command somebody notices it missing on.
fn block_arg(name: &'static str, kind: &'static ArgumentType) -> CommandNode {
    suggest_blocks(CommandNode::argument(name, kind))
}
/// `//pos1 [position]`
struct Pos1;
impl Sub for Pos1 {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let position = anchor(sender, args, "position")?;

        select(sender, Some(position), None)
    }
}
/// `//pos2 [position]`
struct Pos2;
impl Sub for Pos2 {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let position = anchor(sender, args, "position")?;

        select(sender, None, Some(position))
    }
}
/// `//sel [selector]`
///
/// Only the cuboid is selectable, which is what `//pos1` and `//pos2` already give, so
/// the argument exists to accept `clear` and to say plainly that the others are not
/// here rather than reporting an unknown subcommand.
struct Sel;
impl Sub for Sel {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let (clear, caption) = sel_action(optional_word(args, "selector")?.as_deref())?;

        if clear {
            state::sessions().with(&sender.get_name(), Session::clear_selection);
        }

        Ok(caption.to_string())
    }
}

/// What `//sel <selector>` does: whether it drops the selection, and what it prints.
///
/// `SelectionCommands.select` (SelectionCommands.java:783-796) reads the *absence* of a
/// selector as "clear", and `cuboid` as "switch to a cuboid selector", which keeps the
/// points already chosen. Both captions are verbatim from `strings.json`:
/// `worldedit.select.cleared` (:436) and `worldedit.select.cuboid.message` (:437).
fn sel_action(selector: Option<&str>) -> Result<(bool, &'static str), String> {
    match selector {
        // `clear` is this plugin's own spelling, not FAWE's: it is the only way of
        // saying "no selector" other than saying nothing at all.
        None | Some("" | "clear") => Ok((true, selection_cleared())),

        Some("cuboid") => Ok((
            false,
            "Cuboid: left click for point 1, right click for point 2",
        )),

        Some(q) => Err(format!(
            "'{q}' is not a selection type this plugin has. Only 'cuboid' and 'clear' \
             are; set the corners with //pos1 and //pos2."
        )),
    }
}
/// What `//sel` and `//desel` both print when the selection is dropped.
///
/// `worldedit.select.cleared` (`strings.json:436`), printed by `SelectionCommands.select`
/// (`SelectionCommands.java:786`). `//desel` is an alias of `/sel` in FAWE
/// (`SelectionCommands.java:770`) — the same handler, so the same caption.
fn selection_cleared() -> &'static str {
    "Selection cleared."
}
/// `//desel`
struct Desel;
impl Sub for Desel {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, _args: &ConsumedArgs) -> Said {
        state::sessions().with(&sender.get_name(), Session::clear_selection);

        Ok(selection_cleared().to_string())
    }
}
/// `//outset <amount>` and `//inset <amount>`
///
/// One
/// type for both: outset pushes every face out and inset pulls it in, and they differ
/// only in the sign of the amount. FAWE spells them separately (`/outset` and `/inset`)
/// and gives both the same `-h`/`-v` switches.
struct Resize<const OUTSET: bool>;
impl<const OUTSET: bool> Sub for Resize<OUTSET> {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let amount = integer(args, "amount")?;

        // `-h` moves only the horizontal faces and `-v` only the vertical one, so a

        // column of blocks can be widened without growing taller. Both are switches,

        // which Brigadier cannot spell as `-h`, so they arrive as booleans.

        let horizontal = flag(args, "--horizontal");

        let vertical = flag(args, "--vertical");

        let (x, y, z) = if horizontal && !vertical {
            (amount, 0, amount)
        } else if vertical && !horizontal {
            (0, amount, 0)
        } else {
            (amount, amount, amount)
        };

        let region = selection(sender)?;

        let Some(resized) = adjust(region, x, y, z, OUTSET) else {
            return Err(if OUTSET {
                "Grew".to_string()
            } else {
                "Shrank".to_string()
            });
        };

        if OUTSET {
            apply_selection(sender, resized, "Region outset.")
        } else {
            apply_selection(sender, resized, "Region inset.")
        }
    }
}
/// `//contract <amount> [direction]`
struct Contract;
impl Sub for Contract {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let amount = integer(args, "amount")?;

        let direction = direction_arg(sender, args)?;

        let region = selection(sender)?;

        let Some(contracted) = contract(region, direction, amount) else {
            return Err("The amount has to be no more than half the selection, or there would be nothing left.".to_string());
        };

        apply_selection(
            sender,
            contracted,
            contract_caption(region.dimensions().volume() - contracted.dimensions().volume()),
        )
    }
}
/// What `//contract` prints: how many blocks the selection lost.
///
/// `worldedit.contract.contracted` (`strings.json:421`), printed by
/// `SelectionCommands.contract` (`SelectionCommands.java:419`) with `oldSize - newSize` —
/// the two region volumes, not the amount typed.
fn contract_caption(removed: usize) -> String {
    format!("Region contracted {removed} blocks.")
}
/// `//shift <amount> [direction]`
struct Shift;
impl Sub for Shift {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let amount = integer(args, "amount")?;

        let direction = direction_arg(sender, args)?;

        let region = selection(sender)?;

        apply_selection(sender, shift(region, direction, amount), "Region shifted.")
    }
}
/// The direction an axis command works along, defaulting to the way the player faces.
///
/// `//contract 5` and `//shift 5` both default to `aim`, which FAWE resolves against
/// the player's yaw, so the same command moves a selection along whichever of the four
/// ways the player happens to be looking. That is why the yaw has to be read here
/// rather than defaulted to an axis.
fn direction_arg(sender: &CommandSender, args: &ConsumedArgs) -> Result<(i32, i32, i32), String> {
    let (text, yaw) = match optional_word(args, "direction")? {
        None => ("me".to_string(), yaw_of(sender)),

        Some(text) => (text, yaw_of(sender)),
    };

    parse_direction(&text, yaw).ok_or_else(|| direction_error(&text))
}
/// [`direction_arg`] with a fallback direction rather than the player's facing.
///
/// Used where the default is *not* `aim`: `//fill` defaults to `down`, which is a
/// direction in its own right rather than "aim".
fn direction_arg_default(
    sender: &CommandSender,

    args: &ConsumedArgs,

    name: &str,

    fallback: (i32, i32, i32),
) -> Result<(i32, i32, i32), String> {
    match optional_word(args, name)? {
        None => Ok(fallback),

        Some(text) => parse_direction(&text, yaw_of(sender)).ok_or_else(|| direction_error(&text)),
    }
}
/// What a direction word that means nothing is told to the player.
///
/// One message rather than one per command, because the set of accepted words is the
/// same everywhere and three variants of it drift apart.
fn direction_error(text: &str) -> String {
    format!("'{text}' is not a direction. Use north, south, east, west, up or down.")
}
/// The sender's yaw, or south if they have none.
///
/// A console sender has no facing, and a selection command from the console is only
/// meaningful with an explicit direction, so south is a placeholder that the command
/// name never resolves to by itself.
fn yaw_of(sender: &CommandSender) -> f32 {
    sender.as_player().map_or(0.0, |player| player.get_yaw())
}
/// Stores a new selection and reports it.
///
/// `caption` is the finished line rather than a fragment, because not every caption ends in
/// a full stop at a fixed place: `worldedit.contract.contracted` carries a block count
/// between the words.
fn apply_selection(
    sender: &CommandSender,
    region: CuboidRegion,
    caption: impl Into<String>,
) -> Said {
    state::sessions().with(&sender.get_name(), |session| {
        session.set_pos1(region.minimum());

        session.set_pos2(region.maximum());
    });

    Ok(caption.into())
}
/// `//size`
struct Size;
impl Sub for Size {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        // `//size -c` reports the clipboard instead of the selection. FAWE's switch is

        // spelled `-c`; a `bool` argument here parses `true`/`false`, so the spelling is

        // longhand and documented as such.

        //
        // Which branch runs is decided by the switch alone. It used to be decided by

        // whether the player had a session at all, so a player who had never run a

        // WorldEdit command was told `//size` takes `--clipboard` or nothing.

        let player = sender.get_name();

        if flag(args, "--clipboard") {
            return state::sessions().get(&player).map_or_else(
                || Err(empty_clipboard()),
                |session| {
                    session.clipboard().map_or_else(
                        || Err(empty_clipboard()),
                        |holder| {
                            let clipboard = holder.clipboard();

                            Ok(clipboard_size(clipboard.region(), clipboard.origin()))
                        },
                    )
                },
            );
        }

        // FAWE asks the session for the selection first (`SelectionCommands.java:677`) and

        // only then prints the type, so a player with no selection is refused before any

        // line is emitted — which is what `selection` does.

        let region = selection(sender)?;

        Ok(size_report(region))
    }
}
/// `worldedit.error.empty-clipboard` (`strings.json:268`), which is what FAWE's
/// `EmptyClipboardException` reaches the player with for `//size -c` with nothing copied.
fn empty_clipboard() -> String {
    "Your clipboard is empty. Use //copy first.".to_string()
}

/// What `//size` prints about the selection, one caption per line.
///
/// The four captions come from `SelectionCommands.size` (`SelectionCommands.java:679-694`)
/// and are `worldedit.size.type` (`strings.json:428`), the selector's own info lines
/// (`worldedit.selection.cuboid.info.pos1` / `.pos2`, :628-629), `worldedit.size.size`
/// (:429), `worldedit.size.distance` (:430) and `worldedit.size.blocks` (:431).
///
/// The lines are joined with newlines rather than sent as four messages because
/// [`Said`] carries one string; a newline is what the chat renderer breaks on, so the
/// player still sees four lines.
fn size_report(region: CuboidRegion) -> String {
    let minimum = region.minimum();

    let maximum = region.maximum();

    let dimensions = region.dimensions();

    [
        // `CuboidRegionSelector.getTypeName` is "cuboid" (`CuboidRegionSelector.java:252-254`).
        format!("Type: {}", "cuboid"),
        // The info lines carry the stored points, which `learnChanges` has normalised to
        // the region's own corners (`CuboidRegionSelector.java:238-241`).
        format!("Position 1: {}", show(minimum)),
        format!("Position 2: {}", show(maximum)),
        // `BlockVector3.toString` is `"(" + x + ", " + y + ", " + z + ")"`
        // (`BlockVector3.java:914-916`), brackets and all.
        format!(
            "Size: {}",
            show(BlockPos::new(dimensions.x, dimensions.y, dimensions.z))
        ),
        format!("Cuboid distance: {}", java_double(cuboid_distance(region))),
        format!("# of blocks: {}", region.dimensions().volume()),
    ]
    .join("\n")
}
/// What `//size -c` prints about the clipboard.
///
/// `worldedit.size.offset` (`strings.json:427`), printed by `SelectionCommands.size`
/// (`SelectionCommands.java:665-671`) as `name: size @ origin (blocks)`.
///
/// The size is written `x*y*z` and the origin `x,y,z` — both built by string concatenation
/// in Java (`SelectionCommands.java:661-662`), not by any `toString`, so they carry no
/// brackets even though `BlockVector3.toString` would. The name is the holder's index, as
/// Java writes `Integer.toString(index)` for a holder with no URI (`:653`); this plugin
/// has exactly one holder and it is always index 0.
fn clipboard_size(region: CuboidRegion, origin: BlockPos) -> String {
    let d = region.dimensions();

    format!(
        "0: {}*{}*{} @ {},{},{} ({} blocks)",
        d.x,
        d.y,
        d.z,
        origin.x,
        origin.y,
        origin.z,
        d.x as u64 * d.y as u64 * d.z as u64,
    )
}
/// The distance between a region's two opposite corners.
///
/// `Vector3.distance` (`Vector3.java:500-503`) is the Euclidean distance, and
/// `distanceSq` (:511-517) subtracts the `int` components first, so this does the same
/// rather than converting to `f64` and risking a different rounding for large selections.
fn cuboid_distance(region: CuboidRegion) -> f64 {
    let minimum = region.minimum();

    let maximum = region.maximum();

    let (dx, dy, dz) = (
        i64::from(maximum.x - minimum.x),
        i64::from(maximum.y - minimum.y),
        i64::from(maximum.z - minimum.z),
    );

    ((dx * dx + dy * dy + dz * dz) as f64).sqrt()
}

/// A `double` the way Java writes one, for `worldedit.size.distance`.
///
/// `TextComponent.of(double)` goes through `Double.toString`, which never leaves a whole
/// number without a fractional part — a 10-wide flat selection is 99.0, not 99 — and which
/// switches to scientific notation outside `[1e-3, 1e7)`. Rust's `Display` does neither,
/// so the caption would read "Cuboid distance: 99" where FAWE reads "99.0".
///
/// Both sides round-trip to the same `double`, so the digits agree; only the shape of the
/// decimal differs.
///
/// `pub(crate)` because [`crate::arguments::outside_range`] formats a command argument's
/// declared bound the same way, and a second formatter for `Double.toString` is the kind
/// of near-duplicate that drifts.
pub(crate) fn java_double(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }

    let magnitude = value.abs();

    if magnitude != 0.0 && !(1e-3..1e7).contains(&magnitude) {
        // Java writes `1.0E10`, with a digit either side of the point; Rust's `LowerExp`
        // writes `1e10`. The mantissa is fixed up to match.
        let written = format!("{value:e}");

        let Some((mantissa, exponent)) = written.split_once('e') else {
            return written;
        };

        return format!(
            "{}.0E{exponent}",
            mantissa.strip_suffix(".0").unwrap_or(mantissa)
        );
    }

    let written = format!("{value}");

    if written.contains('.') {
        written
    } else {
        format!("{written}.0")
    }
}
/// `//count [mask]`
struct Count;
impl Sub for Count {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        let mask = match optional_word(args, "mask")? {
            // FAWE's `//count` with no argument counts what is there, not air.
            None => Mask::Existing,

            Some(text) => mask_of(&text, extent.registry(), &extent)?,
        };

        let mut context = MaskContext::new(&extent, extent.registry());

        let matched = jobs::walk(region)
            .filter(|position| mask.test(&mut context, *position))
            .count();

        Ok(count_caption(matched))
    }
}
/// What `//count` prints for a number of matching blocks.
///
/// `worldedit.count.counted` (`strings.json:432`), printed by `SelectionCommands.count`
/// (`SelectionCommands.java:711`). FAWE prints only the count; it does not mention the
/// selection's volume or restate the mask, so this does not either.
fn count_caption(matched: usize) -> String {
    format!("Counted: {matched}")
}
/// `//chunk`
struct Chunk;
impl Sub for Chunk {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, _args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let position = feet(sender)?;

        apply_selection(
            sender,
            chunk_region(position, extent.bounds()),
            chunk_caption(position),
        )
    }
}
/// What `//chunk` prints: the chunk coordinates it selected.
///
/// `worldedit.chunk.selected` (`strings.json:416`), printed by `SelectionCommands.chunk`
/// (`SelectionCommands.java:299-304`) with `minChunk`'s own three components — the chunk,
/// not the blocks. The chunk is 16 wide in X and Z and 256 tall in Y, which is
/// `ChunkStore.toChunk3d`'s `shr(4, 8, 4)` (`ChunkStore.java:47-65`).
fn chunk_caption(position: BlockPos) -> String {
    let chunk = chunk_of(position);

    format!("Chunk selected: {}, {}, {}", chunk.x, chunk.y, chunk.z)
}
/// The chunk a block belongs to, as `ChunkStore.toChunk3d` computes it.
///
/// Arithmetic shifts, so a block below the origin lands in chunk `-1` rather than `0`,
/// which is what Java's `>>` on a negative `int` does.
fn chunk_of(position: BlockPos) -> BlockPos {
    BlockPos::new(position.x >> 4, position.y >> 8, position.z >> 4)
} // ## Edit commands 
/// `//set <pattern>`
struct Set;
impl Sub for Set {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        let pattern = text(args, "pattern")?;
        let pattern = set_pattern(&pattern);
        queue(
            sender,
            extent,
            jobs::walk(region),
            pattern,
            None,
            // Java guards this one: `if (affected != 0)` (RegionCommands.java:124).
            caption::SET_DONE,
        )
    }
}
fn set_pattern(pattern: &str) -> &str {
    pattern
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            pattern
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
        .unwrap_or(pattern)
}
/// `//air`
struct Air;
impl Sub for Air {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, _args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        queue(
            sender,
            extent,
            jobs::walk(region),
            "air",
            None,
            // `//air` reaches `RegionCommands.set` itself (`:140-142`), so it gets
            // `//set`'s caption and `//set`'s guard.
            caption::SET_DONE,
        )
    }
}
/// `//replace [from] <to>`
struct Replace;
impl Sub for Replace {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        // `from` and `to` are both plain word arguments, so a one-word line satisfies
        // either branch of the tree and the dispatcher has nothing to prefer between
        // them: it can read the word as `from` and leave `to` empty. Only one of the
        // two readings can be right, because `to` is never left out on purpose, so an
        // empty `to` beside a `from` means the word was written as the pattern — which
        // is what `//replace stone` means.
        let (from, to) = match (optional_word(args, "from")?, text(args, "to")) {
            (Some(from), Ok(to)) => (Some(from), to),
            (Some(pattern), Err(_)) => (None, pattern),
            (None, to) => (None, to?),
        };

        let from = from_mask(
            from.as_deref(),
            extent.registry(),
            (extent.min_y(), extent.max_y()),
        )?;

        queue(
            sender,
            extent,
            jobs::walk(region),
            &to,
            Some(from),
            caption::REPLACED,
        )
    }
}
/// `//center <pattern>`
struct Center;
impl Sub for Center {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        queue(
            sender,
            extent,
            Box::new(center_positions(region).into_iter()),
            &text(args, "pattern")?,
            None,
            caption::CENTER_CHANGED,
        )
    }
}
/// `//walls <pattern>`
struct Walls;
impl Sub for Walls {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        queue(
            sender,
            extent,
            Box::new(wall_positions(region).into_iter()),
            &text(args, "pattern")?,
            None,
            caption::WALLS_CHANGED,
        )
    }
}
/// `//faces <pattern>`
struct Faces;
impl Sub for Faces {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        queue(
            sender,
            extent,
            Box::new(face_positions(region).into_iter()),
            &text(args, "pattern")?,
            None,
            caption::FACES_CHANGED,
        )
    }
} // ## Generated shapes 
/// `//sphere` and `//hsphere`.
///
/// A `const` rather than two structs because the two differ only in whether the shape
/// is hollow, and a pair of near-identical handlers is a pair that can drift.
struct Sphere<const HOLLOW: bool>;
impl<const HOLLOW: bool> Sub for Sphere<HOLLOW> {
    fn node() -> &'static str {
        nodes::SHAPE
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let center = feet(sender)?;

        // FAWE takes one radius or three. Two is refused by the tree rather than

        // guessed at here, because which axis it meant is genuinely ambiguous.

        let (rx, ry, rz) = if present(args, "radius3") {
            (
                number(args, "radius")?,
                number(args, "radius2")?,
                number(args, "radius3")?,
            )
        } else {
            let radius = number(args, "radius")?;

            (radius, radius, radius)
        };

        let radii = (rx.max(0.0), ry.max(0.0), rz.max(0.0));

        let positions = shape::sphere(center, radii, !HOLLOW, extent.bounds());

        queue(
            sender,
            extent,
            Box::new(positions.into_iter()),
            &word(args, "pattern")?,
            None,
            caption::SPHERE_CREATED,
        )
    }
}
/// `//cyl` and `//hcyl`.
struct Cyl<const HOLLOW: bool>;
impl<const HOLLOW: bool> Sub for Cyl<HOLLOW> {
    fn node() -> &'static str {
        nodes::SHAPE
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let center = feet(sender)?;

        // The arity says what each number is: one radius, then an X/Z pair, then a

        // height, then `//hcyl`'s thickness. FAWE resolves the same way, which is why

        // `//hcyl stone 5 3 2` is a three-block-tall cylinder and not a two-block

        // thick shell.

        let radius = number(args, "radius")?;

        let (rx, rz) = if present(args, "radius2") {
            (radius.max(1.0), number(args, "radius2")?.max(1.0))
        } else {
            (radius.max(1.0), radius.max(1.0))
        };

        let height = optional_integer(args, "height", 1)?;

        let thickness = optional_number(args, "thickness", 0.0)?;

        // Only //hcyl has a thickness. Java's /cyl has no such parameter at all -
        // GenerationCommands.java:191 calls makeCylinder(pos, pattern, rx, rz,
        // height, !hollow) - so a fifth number on //cyl is a mistyped extra argument,
        // not a shell width. The shared builder registers the node for both commands
        // (see cyl_node), so it has to be refused here rather than silently building a
        // hollow band out of a command that means a solid cylinder.
        if !HOLLOW && present(args, "thickness") {
            return Err(
                "//cyl has no thickness argument. Use //hcyl for a hollow cylinder.".to_string(),
            );
        }
        if HOLLOW && (thickness > rx || thickness > rz) {
            return Err("The thickness has to be no more than the radius.".to_string());
        }

        let positions =
            shape::cylinder(center, (rx, rz), height, HOLLOW, thickness, extent.bounds());

        queue(
            sender,
            extent,
            Box::new(positions.into_iter()),
            &word(args, "pattern")?,
            None,
            caption::CYL_CREATED,
        )
    }
}
/// `//line <pattern> [thickness]`
///
/// Between the selection's two corners, which is what `drawLine` is given.
struct Line;
impl Sub for Line {
    fn node() -> &'static str {
        nodes::SHAPE
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        let thickness = optional_number(args, "thickness", 0.0)?;

        if thickness < 0.0 {
            return Err("The thickness has to be zero or more.".to_string());
        }

        let positions = shape::line(region.minimum(), region.maximum(), thickness, false, false);

        queue(
            sender,
            extent,
            Box::new(positions.into_iter()),
            &word(args, "pattern")?,
            None,
            caption::LINE_CHANGED,
        )
    }
} // ## Clipboard commands 
/// `//copy`
struct Copy;
impl Sub for Copy {
    fn node() -> &'static str {
        nodes::CLIPBOARD
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        // FAWE anchors the clipboard where the player is standing, which is what makes

        // a later `//paste` land under their feet by default.

        let origin = anchor(sender, args, "position")?;

        let clipboard = worldedit_core::clipboard::Clipboard::capture(&extent, region, origin);

        let blocks = clipboard.len();

        state::sessions().with(&sender.get_name(), |session| {
            session.set_clipboard(ClipboardHolder::new(clipboard))
        });

        Ok(copy_caption(blocks))
    }
}
/// What `//copy` prints for a number of copied blocks.
///
/// `worldedit.operation.affected.block` (`strings.json:619`), one of the three status
/// lines `ClipboardCommands.copy` prints: `copy.getStatusMessages().forEach(actor::print)`
/// (`ClipboardCommands.java:172`) resolves to `ForwardExtentCopy.getStatusMessages`
/// (`ForwardExtentCopy.java:549-563`), whose first entry is that caption.
///
/// The audit brief named `fawe.worldedit.copy.command.copy` ("{0} blocks were copied.")
/// instead. That key is real, but `/copy` never reads it — it belongs to `/lazycopy`
/// (`ClipboardCommands.java:202`) and to `CopyPastaBrush.java:95`. Both keys render
/// "{0} blocks affected" and "{0} blocks were copied." respectively; the call site decides.
fn copy_caption(blocks: usize) -> String {
    format!("{blocks} blocks affected")
}
/// `//cut [pattern]`
struct Cut;
impl Sub for Cut {
    fn node() -> &'static str {
        nodes::CLIPBOARD
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        let origin = anchor(sender, args, "position")?;

        // The pattern is what is left behind rather than copied away. Air is the

        // default, so `//cut` clears the selection the way a cut-and-paste in a text

        // editor would.

        let leave = optional_word(args, "pattern")?.unwrap_or_else(|| "air".to_string());

        let blocks = region.dimensions().volume();

        state::sessions().with(&sender.get_name(), |session| {
            session.set_clipboard(ClipboardHolder::new(
                worldedit_core::clipboard::Clipboard::capture(&extent, region, origin),
            ));
        });

        // The air-fill carries no caption: FAWE's `//cut` never prints a `worldedit.set.done`
        // for it either. `ClipboardCommands.cut` builds a `ForwardExtentCopy` and prints
        // `fawe.tips.tip.lazycut` (`:284`) and `copy.getStatusMessages()` (`:286`), both
        // about the copy — and `ForwardExtentCopy.getStatusMessages`
        // (`ForwardExtentCopy.java:549-563`) is where `worldedit.operation.affected.block`
        // lives. This plugin's `//cut` reports its own line below, so a second caption
        // when the fill drained would be the second line for one cut.
        queue(
            sender,
            extent,
            jobs::walk(region),
            &leave,
            None,
            Caption::Silent,
        )?;

        Ok(format!(
            "Cut {blocks} blocks; the clipboard is anchored at {}.",
            show(origin)
        ))
    }
}
/// `//paste [position]`
struct Paste;
impl Sub for Paste {
    fn node() -> &'static str {
        nodes::CLIPBOARD
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let where_to = anchor(sender, args, "position")?;

        let player = sender.get_name();

        let prepared: Option<(Vec<(BlockPos, BlockValue)>, BlockPos)> =
            state::sessions().with(&player, |session| {
                let holder = session.clipboard()?;

                let clipboard = holder.clipboard();

                let origin = clipboard.origin();

                // The shift moves the clipboard's origin to where the paste is wanted,

                // which is the player's feet unless a position was given. The rotation

                // is applied about the origin first, so `//rotate 90` then pasting beside

                // the player lands the copy sideways of them rather than rotated around

                // wherever it was copied.

                let shift = where_to.offset(-origin.x, -origin.y, -origin.z);

                Some((clipboard.paste_steps(holder.transform(), shift), origin))
            });

        let Some((steps, origin)) = prepared else {
            return Err("Nothing to paste. //copy or //cut first.".to_string());
        };

        let count = steps.len();

        jobs::enqueue_paste(
            &player,
            extent,
            Box::new(steps.into_iter()),
            limit_for(&player),
        );

        Ok(format!(
            "Queued a paste of {count} blocks at {}; the clipboard is anchored at {}.",
            show(where_to),
            show(origin)
        ))
    }
}
/// `//rotate <angle>`
struct Rotate;
impl Sub for Rotate {
    fn node() -> &'static str {
        nodes::CLIPBOARD
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let angle = number(args, "angle")?;

        if !angle.is_finite() {
            return Err("The angle has to be a number of degrees.".to_string());
        }

        let changed = state::sessions().with(&sender.get_name(), |session| {
            let Some(holder) = session.clipboard_mut() else {
                return false;
            };

            // A positive yaw turns clockwise and `RotateY` turns

            // counter-clockwise, hence the negation.

            holder.push_transform(Transform::RotateY(-angle));

            true
        });

        if changed {
            Ok("The clipboard copy has been rotated.".to_string())
        } else {
            Err("Nothing to rotate. //copy or //cut first.".to_string())
        }
    }
}
/// `//flip [direction]`
struct Flip;
impl Sub for Flip {
    fn node() -> &'static str {
        nodes::CLIPBOARD
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        // FAWE's default is `Direction.AIM` — the direction the player is looking —

        // not the X axis. The mirror is built as `direction.abs() * -2 + 1`, which

        // for a horizontal facing flips the axis that facing runs along: looking north

        // or south flips Z, looking east or west flips X.

        let axis = match optional_word(args, "direction")?.as_deref() {
            None => facing_axis(facing(yaw_of(sender))),

            Some("x") | Some("west") | Some("east") => BlockAxis::X,

            Some("y") | Some("up") | Some("down") => BlockAxis::Y,

            Some("z") | Some("north") | Some("south") => BlockAxis::Z,

            Some(other) => {
                return Err(format!(
                    "{other} is not a direction. Use north, south, east, west, up or down, or x, y or z; with no argument the clipboard is mirrored across the way you are facing."
                ));
            }
        };

        let changed = state::sessions().with(&sender.get_name(), |session| {
            let Some(holder) = session.clipboard_mut() else {
                return false;
            };

            holder.push_transform(Transform::Flip(axis));

            true
        });

        if changed {
            Ok("The clipboard copy has been flipped.".to_string())
        } else {
            Err("Nothing to flip. //copy or //cut first.".to_string())
        }
    }
}
/// `//clearclipboard`
struct ClearClipboard;
impl Sub for ClearClipboard {
    fn node() -> &'static str {
        nodes::CLIPBOARD
    }

    fn run(sender: &CommandSender, _server: &Server, _args: &ConsumedArgs) -> Said {
        let cleared = state::sessions().with(&sender.get_name(), |session| {
            let had = session.clipboard().is_some();

            session.clear_clipboard();

            had
        });

        Ok(if cleared {
            "The clipboard is empty.".to_string()
        } else {
            "There was nothing on the clipboard.".to_string()
        })
    }
} // ## History commands 
/// `//undo [times]`
struct Undo;
impl Sub for Undo {
    fn node() -> &'static str {
        nodes::HISTORY
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        replay_history(sender, _server, args, Direction::Undo)
    }
}
/// `//redo [times]`
struct Redo;
impl Sub for Redo {
    fn node() -> &'static str {
        nodes::HISTORY
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        replay_history(sender, _server, args, Direction::Redo)
    }
}
/// Which way a history replay runs.
#[derive(Clone, Copy)]
pub(crate) enum Direction {
    /// Put the original blocks back.
    Undo,

    /// Put the replaced blocks back.
    Redo,
}

impl Direction {
    /// What a replay that undid or redid `steps` edits prints.
    ///
    /// `worldedit.undo.undone` (`strings.json:346`) and `worldedit.redo.redone` (:348),
    /// printed by `HistoryCommands.undo` (`HistoryCommands.java:101`) and `.redo` (:143).
    /// The count is the number of *edits* replayed, not of blocks, which is why FAWE calls
    /// them "available edits" and why this does not mention the changed block count.
    pub(crate) fn replayed(self, steps: usize) -> String {
        match self {
            Direction::Undo => format!("Undid {steps} available edits."),

            Direction::Redo => format!("Redid {steps} available edits."),
        }
    }

    /// What a replay with nothing left to do prints.
    ///
    /// `worldedit.undo.none` (`strings.json:347`) and `worldedit.redo.none` (:349). FAWE
    /// prints these for every empty case, including a player who has never edited: the
    /// loop at `HistoryCommands.java:90-99` runs, undoes nothing, and falls through to
    /// `timesUndone > 0`.
    pub(crate) fn nothing_left(self) -> &'static str {
        match self {
            Direction::Undo => "Nothing left to undo.",

            Direction::Redo => "Nothing left to redo.",
        }
    }
}
/// Queues the player's history replay behind earlier work.
fn replay_history(
    sender: &CommandSender,

    _server: &Server,

    args: &ConsumedArgs,

    direction: Direction,
) -> Said {
    let times = optional_integer(args, "times", 1)?.max(1);
    let player = sender.get_name();
    jobs::enqueue_replay(
        &player,
        matches!(direction, Direction::Undo),
        times as usize,
    );
    Ok("History replay queued.".to_string())
}
/// `//clearhistory`
struct ClearHistory;
impl Sub for ClearHistory {
    fn node() -> &'static str {
        nodes::HISTORY
    }

    fn run(sender: &CommandSender, _server: &Server, _args: &ConsumedArgs) -> Said {
        jobs::enqueue_clear_history(&sender.get_name());

        Ok("History clearing queued.".to_string())
    }
} // ## Settings and information 
/// `//gmask [mask]`
struct Gmask;
impl Sub for Gmask {
    fn node() -> &'static str {
        nodes::LIMIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let player = sender.get_name();

        match optional_word(args, "mask")? {
            // An absent or empty mask clears it, which is `//gmask` with no

            // argument in FAWE.
            None => {
                state::sessions().with(&player, |session| session.set_mask(None));

                Ok(gmask_caption(false).to_string())
            }

            Some(text) => {
                let settings = state::config();

                let extent = here(sender, &settings)?;

                let mask = mask_of(&text, extent.registry(), &extent)?;

                state::sessions().with(&player, |session| session.set_mask(Some(mask)));

                Ok(gmask_caption(true).to_string())
            }
        }
    }
}
/// What `//gmask` prints, with and without an argument.
///
/// `worldedit.gmask.disabled` (`strings.json:331`) and `worldedit.gmask.set` (:332), printed
/// by `GeneralCommands.gmask` (`GeneralCommands.java:409` and `:412`). `worldedit.gmask.set`
/// has no `{0}`: FAWE does not echo the mask back, so neither does this.
fn gmask_caption(set: bool) -> &'static str {
    if set {
        "Global mask set."
    } else {
        "Global mask disabled."
    }
}
/// `//limit [amount]`
struct Limit;
impl Sub for Limit {
    fn node() -> &'static str {
        nodes::LIMIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let player = sender.get_name();

        // The number `//limit` reports is the one the session now carries, so it has to be
        // read out of `Config::block_limit` rather than `Config::block_limit()`: the latter
        // folds every non-positive value to `None`, and `0` is what this plugin's shipped
        // `config.yml` sets.
        let configured = state::config().block_limit.unwrap_or(0);

        // An absent amount clears the player's own limit so the configured one takes

        // over again, which is what `//limit` with no argument does in FAWE.

        if !present(args, "amount") {
            state::sessions().with(&player, |session| session.set_limit(None));

            return Ok(limit_caption(configured, configured));
        }

        let amount = integer(args, "amount")?.max(-1);

        // Zero is how WorldEdit spells "no limit", and it has to be remembered as an

        // explicit choice rather than as "unset": collapsing the two would let

        // `//limit 0` silently re-impose the configured cap on the next command.

        state::sessions().with(&player, |session| {
            session.set_limit(Some(i64::from(amount)))
        });

        Ok(limit_caption(i64::from(amount), configured))
    }
}
/// What `//limit` prints.
///
/// `worldedit.limit.set` (`strings.json:316`) always, and
/// `worldedit.limit.return-to-default` (:317) when the new value differs from the
/// configured one. `GeneralCommands.limit` (`GeneralCommands.java:196-200`) builds the
/// second half as `append(space()).append(caption)`, so the space between the two is
/// added by the caller and the caption itself does not start with one. The `max(-1, limit)`
/// it clamps with is why `//limit -5` reports `-1`.
fn limit_caption(limit: i64, configured: i64) -> String {
    let mut caption = format!("Block change limit set to {limit}.");

    if limit != configured {
        caption.push_str(" (Use //limit to go back to the default.)");
    }

    caption
}
/// `//world [name]`
struct WorldCmd;
impl Sub for WorldCmd {
    fn node() -> &'static str {
        nodes::LIMIT
    }

    fn run(_sender: &CommandSender, server: &Server, args: &ConsumedArgs) -> Said {
        let name = match optional_word(args, "name")? {
            None => return Ok(world_caption(None)),

            Some(name) => name,
        };

        match server.get_world_by_name(&name) {
            // Refused rather than accepted and ignored. An admin typing `//world nether`

            // and then editing their own overworld would be worse than an error.

            //
            // FAWE accepts the name here and stores it as an override

            // (`GeneralCommands.world`, `GeneralCommands.java:360`); it has no equivalent

            // refusal to borrow a wording from. Reported rather than imitated.
            Some(_) => Err(format!("There is no world called {name}.")),

            None => Err(format!(
                "'{name}' is loaded, but an edit always applies to the world you are standing in. Stand in '{name}' and run the command again."
            )),
        }
    }
}
/// What `//world` prints with and without a name.
///
/// `worldedit.world.remove` (`strings.json:344`) and `worldedit.world.set` (:345), printed
/// by `GeneralCommands.world` (`GeneralCommands.java:362` and `:364`).
///
/// A bare `//world` is the *removal* case, not a query: FAWE's `@Arg(def = "") World world`
/// is `null` there, and `null` is what `setWorldOverride(null)` is given
/// (`GeneralCommands.java:360`). So it says the override was removed, not which world you
/// are standing in. `worldedit.world.set` has no full stop after "default" — the caption
/// runs on without one.
fn world_caption(over: Option<&str>) -> String {
    match over {
        None => "Removed world override.".to_string(),

        Some(name) => {
            format!("Set the world override to {name}. (Use //world to go back to default)")
        }
    }
}
/// `//schem load <file>` / `//schem save <file>` / `//schem list` / `//schem paste <file>`
struct Schem;
impl Sub for Schem {
    fn node() -> &'static str {
        nodes::CLIPBOARD
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let sub = word(args, "action")?;

        let settings = state::config();

        match sub.as_str() {
            "save" => schem_save(sender, args, &settings),

            "load" => schem_load(sender, args, &settings, false),

            "paste" => schem_load(sender, args, &settings, true),

            "list" => schem_list(),

            other => Err(format!(
                "{other} is not a //schem action. Use save, load, paste or list."
            )),
        }
    }
}
/// Where schematics live.
///
/// Inside the plugin's own data folder rather than the server's working directory: the
/// guest is granted `fs.read.data` and `fs.write.data` and nothing else, so any other
/// path would fail at the WASI boundary with an error that says nothing useful.
fn schem_folder() -> std::path::PathBuf {
    std::path::PathBuf::from(state::data_folder()).join("schematics")
}
/// Makes sure the schematics folder exists before anything tries to read it.
///
/// Called once at load rather than on first use, because the first thing a player does on
/// a fresh install is `//schem list` and that would otherwise fail with a missing
/// directory — which reads as "schematics are broken" rather than "you have none yet".
pub fn ensure_schem_folder() -> Result<(), String> {
    let folder = schem_folder();

    match std::fs::create_dir_all(&folder) {
        Ok(()) => Ok(()),

        Err(error) => Err(format!("could not create {}: {error}", folder.display())),
    }
}
/// Turns a name a player typed into a path inside the folder.
///
/// A name containing a separator, or one that walks upwards, is refused rather than
/// sanitised: silently rewriting it would save the file under a name the player did not
/// ask for and then not find it.
fn schem_path(name: &str) -> Result<std::path::PathBuf, String> {
    if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
        return Err(format!(
            "{name} is not a usable file name. Use a plain name like house.schem; schematics live in the plugin's own folder."
        ));
    }

    let file = if name.ends_with(".schem") {
        name.to_string()
    } else {
        format!("{name}.schem")
    };

    Ok(schem_folder().join(file))
}
/// `//schem save <file>`
fn schem_save(sender: &CommandSender, args: &ConsumedArgs, settings: &Config) -> Said {
    let region = selection(sender)?;

    let extent = here(sender, settings)?;

    // FAWE anchors a saved schematic at the player by default, which is what makes a

    // later `//schem load` + `//paste` put it back under their feet.

    let origin = anchor(sender, args, "position")?;

    let clipboard = worldedit_core::clipboard::Clipboard::capture(&extent, region, origin);

    let blocks = clipboard.len();

    // The resolver is what turns a state id back into the name a schematic stores, and

    // it is the only thing that knows the names. A state it cannot name would be written

    // as air, which is why this is an error rather than a fallback.

    let registry = extent.registry();

    let name_of = |state: u16| registry.block_name(state).map(str::to_string);

    let bytes = worldedit_core::schematic::write(&clipboard, &name_of)
        .map_err(|problem| format!("could not write the schematic: {problem}"))?;

    let path = schem_path(&word(args, "file")?)?;

    let folder = schem_folder();

    std::fs::create_dir_all(&folder)
        .map_err(|error| format!("could not create {}: {error}", folder.display()))?;

    std::fs::write(&path, &bytes)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;

    Ok(format!(
        "Saved {blocks} blocks to {}.",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    ))
}
/// `//schem load <file>` and `//schem paste <file>`.
///
/// `load` leaves the schematic on the clipboard for a later `//paste`, which is what
/// FAWE's `//schem load` does. `paste` does the paste in the same step, which saves a
/// round trip but is not a WorldEdit command.
fn schem_load(
    sender: &CommandSender,

    args: &ConsumedArgs,

    settings: &Config,

    paste_now: bool,
) -> Said {
    let player = sender.get_name();

    let path = schem_path(&word(args, "file")?)?;

    let bytes = std::fs::read(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;

    let extent = here(sender, settings)?;

    let clipboard = worldedit_core::schematic::read(&bytes, extent.registry())
        .map_err(|problem| problem.to_string())?;

    let blocks = clipboard.len();

    if !paste_now {
        state::sessions().with(&player, |session| {
            session.set_clipboard(ClipboardHolder::new(clipboard));
        });

        return Ok(format!(
            "Loaded {blocks} blocks from {}. //paste puts them down.",
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        ));
    }

    let where_to = anchor(sender, args, "position")?;

    let holder = ClipboardHolder::new(clipboard);

    let origin = holder.clipboard().origin();

    let shift = where_to.offset(-origin.x, -origin.y, -origin.z);

    let steps = holder.clipboard().paste_steps(holder.transform(), shift);

    jobs::enqueue_paste(
        &player,
        extent,
        Box::new(steps.into_iter()),
        limit_for(&player),
    );

    Ok(format!(
        "Queued a paste of {blocks} blocks from {} at {}.",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        show(where_to)
    ))
}
/// `//schem list`
fn schem_list() -> Said {
    let folder = schem_folder();

    let entries = match std::fs::read_dir(&folder) {
        Ok(entries) => entries,

        // No folder yet simply means nothing has been saved.
        Err(_) => return Ok("No schematics have been saved yet.".to_string()),
    };

    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "schem"))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    names.sort_unstable();

    if names.is_empty() {
        return Ok("No schematics have been saved yet.".to_string());
    }

    Ok(format!(
        "{} schematic(s): {}",
        names.len(),
        names.join(", ")
    ))
}
/// `//hollow <thickness> [pattern]`
///
/// FAWE floods air in from the six faces of the selection and replaces whatever the
/// flood does not reach, so what the thickness leaves behind is a shell of whatever was
/// already open to the air rather than a shell of the selection's bounding box. A
/// selection of nothing but stone is emptied outright, at any thickness.
///
/// A thickness of 0 is a thickness of 1: the loop that grows the flood is
/// `for (int i = 1; i < thickness; ++i)` (EditSession.java:3383), which runs for neither.
struct Hollow;
impl Sub for Hollow {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        let thickness = integer(args, "thickness")?;

        if thickness < 0 {
            return Err("The thickness has to be zero or more.".to_string());
        }

        let pattern = optional_word(args, "pattern")?.unwrap_or_else(|| "air".to_string());

        // FAWE's default mask is a `SolidBlockMask` (RegionCommands.java:867-869), which
        // tests a movement blocker (SolidBlockMask.java:34). The flood stops at anything
        // that answers true, so this is the whole reason `//hollow` can see a block at
        // all. Borrowed from the extent and finished before it is moved into the queue.
        let registry = extent.registry();
        let solid = |position: BlockPos| registry.is_solid(extent.get_block(position).state);
        let positions = shape::hollow_out(region, thickness, &solid);

        queue(
            sender,
            extent,
            Box::new(positions.into_iter()),
            &pattern,
            None,
            caption::HOLLOW_CHANGED,
        )
    }
}
/// `//wand [item]`
///
/// Gives the player an item to select with. Left-clicking a block sets pos1, right-clicking
/// sets pos2. The opposite of FAWE, deliberately; see `OnWandClick` in the crate root for
/// why, and note that the reply below has to say the same thing the code does.
struct Wand;
impl Sub for Wand {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        // FAWE's default is a wooden axe, which is odd enough to be unlikely to be in
        // a player's inventory already.
        let item =
            optional_word(args, "item")?.unwrap_or_else(|| "minecraft:wooden_axe".to_string());

        let player = sender
            .as_player()
            .ok_or_else(|| player_only().to_string())?;

        let inventory = player.get_inventory();

        inventory.set_item_in_hand(
            pumpkin_plugin_api::wit::pumpkin::plugin::common::Hand::Right,
            Some(pumpkin_plugin_api::ItemStack::new(&item, 1)),
        );

        let name = sender.get_name();

        state::sessions().with(&name, |session| session.set_wand(Some(item.clone())));

        Ok(wand_caption().to_string())
    }
}
/// What `//wand` prints once the wand is in the player's hand.
///
/// `worldedit.wand.selwand.info` (`strings.json:418`), printed by
/// `SelectionCommands.wand` (`SelectionCommands.java:358`) once the item is given. The
/// navigation wand has its own caption (`worldedit.wand.navwand.info`, :420), which this
/// plugin does not hand out.
fn wand_caption() -> &'static str {
    "Left click: select pos #1; Right click: select pos #2"
}
/// `//replacenear <radius> [from] <to>`
///
/// FAWE's order, with the mask before the pattern; see `replace_near_node`.
///
/// A cube of side `2 * radius + 1` around the player, rather than around the
/// selection. The radius is clamped to at least one, which is what FAWE does, so
/// `//replacenear 0` still touches the block beside you.
struct ReplaceNear;
impl Sub for ReplaceNear {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let radius = integer(args, "radius")?.max(1);

        let to = word(args, "to")?;

        // The mask is optional and comes second, so the pattern is read first and the

        // mask only when it is there. FAWE's default is "everything that is not air",

        // which is what makes `//replacenear 5 stone` replace the structure around you

        // and leave the air alone.

        let from = from_mask(
            optional_word(args, "from")?.as_deref(),
            extent.registry(),
            (extent.min_y(), extent.max_y()),
        )?;

        let centre = feet(sender)?;

        let region = CuboidRegion::new(
            centre.offset(-radius, -radius, -radius),
            centre.offset(radius, radius, radius),
        );

        queue(
            sender,
            extent,
            Box::new(jobs::walk(region)),
            &to,
            Some(from),
            caption::REPLACED_NEAR,
        )
    }
}
/// `//up <distance>`
///
/// Moves the player up, blocking with glass under them if they would otherwise fall
/// back down — which is the point of the command, and what FAWE's `ascendUpwards` does.
struct Up;
impl Sub for Up {
    fn node() -> &'static str {
        nodes::SELECTION
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let distance = integer(args, "distance")?;

        let player = sender
            .as_player()
            .ok_or_else(|| "Moved up {distance} block(s) to {}.".to_string())?;

        let settings = state::config();

        let mut extent = here(sender, &settings)?;

        let (x, y, z) = player.get_position();

        let from = BlockPos::new(x.floor() as i32, y.floor() as i32, z.floor() as i32);

        let to = from.offset(0, distance, 0);

        // Glass under the destination is what keeps the player up. Written straight

        // rather than through the queue: it is one block, and the glass has to be in

        // place in the same tick as the teleport or the player falls straight back.

        let glass = pattern_of("minecraft:glass", extent.registry())?;

        let under = to.offset(0, -1, 0);

        extent.set_block(under, &glass.next(under));

        let world = standing_in(sender)?;

        player.teleport((x, y + f64::from(distance), z), None, None, world);

        Ok(format!("Moved up {distance} block(s) to {}.", show(to)))
    }
}
/// `//move <count> [direction] [pattern]`
///
/// Moves the *selection's own contents*, which is what FAWE's `//move` does — it takes a
/// `@Selection Region`, not a clipboard. An earlier version of this moved the clipboard,
/// which is a different command FAWE does not have; `//move 5 up` on a build with
/// something selected has to lift the build, not complain there is no clipboard.
struct Move;
impl Sub for Move {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let world = standing_in(sender)?;

        let extent = PumpkinExtent::new(world, settings.world_height);

        let region = selection(sender)?;

        let count = integer(args, "count")?;

        if count < 1 {
            return Err("The distance has to be at least one block.".to_string());
        }

        let (dx, dy, dz) = direction_arg(sender, args)?;

        let displacement = BlockPos::new(dx * count, dy * count, dz * count);

        // What is left in the hole. FAWE's default is air, and it names the argument

        // "air".

        let leave = optional_word(args, "pattern")?.unwrap_or_else(|| "air".to_string());

        let pairs = worldedit_core::relocate::move_pairs(region, displacement);

        // Read every source block before writing any one of them. The pairs overlap the

        // source whenever the displacement is smaller than the region, which is the usual

        // case, so writing as it read would lift some blocks twice.

        let source = worldedit_core::relocate::positions(region);

        let mut steps = Vec::with_capacity(pairs.len());

        for pair in &pairs {
            let block = extent.get_block(pair.from);

            steps.push((pair.to, block));
        }

        let player = sender.get_name();

        let moved = steps.len();

        // The pattern is resolved before the extent is handed over: the arguments are

        // evaluated left to right, so moving the extent into the queue first would leave

        // nothing to resolve the pattern against.

        let leave_pattern = pattern_of(&leave, extent.registry())?;

        // The hole is filled as its own edit rather than as part of the copy, because a

        // copy carries one pattern per block and a hole is a different one.

        jobs::enqueue_edit(
            &player,
            extent,
            Box::new(source.into_iter()),
            leave_pattern,
            mask_for(&player, None),
            limit_for(&player),
        );

        // The moved blocks go into a second job, so a second extent is needed: the world

        // handle inside it is not `Clone`, and the first one has already moved.

        jobs::enqueue_paste(
            &player,
            PumpkinExtent::new(standing_in(sender)?, settings.world_height),
            Box::new(steps.into_iter()),
            limit_for(&player),
        );

        Ok(format!("Moved {moved} blocks {count} block(s)."))
    }
}
/// `//stack <count> [direction] [distance]`
///
/// Stacks the *selection's own contents* along a direction, which is what FAWE's
/// `//stack` does — it too takes a `@Selection Region`. A clipboard has no need of
/// stacking: pasting it repeatedly is what `//paste -a -s` already arranges.
struct Stack;
impl Sub for Stack {
    fn node() -> &'static str {
        nodes::EDIT
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let region = selection(sender)?;

        let count = integer(args, "count")?;

        if count < 1 {
            return Err("The count has to be at least one copy.".to_string());
        }

        let (dx, dy, dz) = direction_arg(sender, args)?;

        let distance = optional_integer(args, "distance", 1)?;

        let offset = BlockPos::new(dx * distance, dy * distance, dz * distance);

        if worldedit_core::relocate::would_stack_overlap(region, offset) {
            // Refused rather than attempted. An overlapping stack copies the copy, so

            // each pass would stack onto the previous one's writes and the result would

            // depend on iteration order — which is not a stack and not what anyone

            // asked for.

            return Err(














  "The copies would overlap each other. Stack by at least the region's size in the direction you are stacking, or move the direction."


















  .to_string(),










  );
        }

        let player = sender.get_name();

        let pairs = worldedit_core::relocate::stack_pairs(region, offset, count);

        // Read once, write `count` times: every pair re-reads the same source, and

        // reading it fresh per copy would be correct only because the copies cannot

        // overlap — which is exactly the case just refused.

        let mut steps = Vec::with_capacity(pairs.len());

        for pair in &pairs {
            let block = extent.get_block(pair.from);

            steps.push((pair.to, block));
        }

        let blocks = steps.len();

        jobs::enqueue_paste(
            &player,
            extent,
            Box::new(steps.into_iter()),
            limit_for(&player),
        );

        Ok(format!("Queued {blocks} blocks over {count} copies."))
    }
}
/// `//pyramid <pattern> <size>` and `//hpyramid`
struct Pyramid<const HOLLOW: bool>;
impl<const HOLLOW: bool> Sub for Pyramid<HOLLOW> {
    fn node() -> &'static str {
        nodes::SHAPE
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let size = integer(args, "size")?;

        let base = feet(sender)?;

        let positions = pyramid_blocks(base, size, HOLLOW);

        queue(
            sender,
            extent,
            Box::new(positions.into_iter()),
            &word(args, "pattern")?,
            None,
            caption::PYRAMID_CREATED,
        )
    }
}

/// The blocks `//pyramid` builds, from a player's own feet.
///
/// The base layer is at the player's own block: FAWE hands
/// `session.getPlacementPosition(actor)` straight to `makePyramid`
/// (`GenerationCommands.java:445-446`) and that is the actor's block unadjusted
/// (`LocalSession.java:1086-1097`). It used to be dropped one first, which buried every
/// pyramid a block into the ground.
fn pyramid_blocks(feet: BlockPos, size: i32, hollow: bool) -> Vec<BlockPos> {
    shape::pyramid(feet, size, hollow)
}

/// `//cone <pattern> <radius> [height]`
struct Cone<const HOLLOW: bool>;
impl<const HOLLOW: bool> Sub for Cone<HOLLOW> {
    fn node() -> &'static str {
        nodes::SHAPE
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let height = present(args, "height").then(|| integer(args, "height"));

        let (radius, height) = cone_shape(number(args, "radius")?, height.transpose()?);

        let base = feet(sender)?;

        let positions = cone_blocks(base, radius, height, HOLLOW, extent.bounds());

        queue(
            sender,
            extent,
            Box::new(positions.into_iter()),
            &word(args, "pattern")?,
            None,
            caption::CONE_CREATED,
        )
    }
}

/// `//cone`'s radius and height.
///
/// The height defaults to one, not to the radius: `@Arg(desc = "The height of the
/// cone", def = "1")` (`GenerationCommands.java:212-213`), so `//cone stone 5` is one
/// flat disc. The radius is clamped to one for the same reason `//cyl`'s is —
/// `radiusX = radiusZ = Math.max(1, radii.get(0))` (`GenerationCommands.java:222`) —
/// and without the clamp a radius below one builds nothing at all, silently.
fn cone_shape(radius: f64, height: Option<i32>) -> (f64, i32) {
    (radius.max(1.0), height.unwrap_or(1))
}

/// The blocks `//cone` builds, from a player's own feet.
///
/// The base is the player's own block, as for [`pyramid_blocks`]: FAWE passes
/// `session.getPlacementPosition(actor)` to `makeCone` untouched
/// (`GenerationCommands.java:237-238`).
fn cone_blocks(
    feet: BlockPos,
    radius: f64,
    height: i32,
    hollow: bool,
    bounds: Bounds,
) -> Vec<BlockPos> {
    shape::cone(feet, (radius, radius), height, hollow, bounds)
}
/// The brushes, as commands.
///
/// FAWE registers each brush as its own command rather than as a subcommand of
/// `//brush`, so `//sphere stone 5` and `//set 3` are top-level. These follow it: each
/// one equips a brush and does nothing to the world yet. `/brush unbind` clears it.
mod brushes {

    use super::*;

    use crate::brush::Brush;

    ///
    /// `tool.setPrimary` and prints "brush equipped"; the tool runs from the click
    fn equip(sender: &CommandSender, brush: Brush) -> Said {
        let name = sender.get_name();

        state::sessions().with(&name, |session| session.set_brush(brush.clone()));

        Ok(format!(
            "Equipped the {} brush. Right-click a block to use it; /brush unbind to stop.",
            brush.name()
        ))
    }

    pub struct Sphere;

    impl Sub for Sphere {
        fn node() -> &'static str {
            nodes::EDIT
        }

        fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
            let extent = here(sender, &state::config())?;

            // The pattern is resolved now rather than on the click so a typo is reported

            // by the command the player typed, not by the click they make next.

            let pattern = word(args, "pattern")?;

            pattern_of(&pattern, extent.registry())?;

            equip(
                sender,
                Brush::Sphere {
                    pattern: pattern.into(),

                    radius: optional_number(args, "radius", 2.0)?,

                    hollow: false,
                },
            )
        }
    }

    /// Replaces what is already there rather than filling air, which is the whole
    pub struct Set;

    impl Sub for Set {
        fn node() -> &'static str {
            nodes::EDIT
        }

        fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
            equip(
                sender,
                Brush::Set {
                    radius: optional_number(args, "radius", 5.0)?,

                    hollow: false,
                },
            )
        }
    }

    pub struct Cylinder;

    impl Sub for Cylinder {
        fn node() -> &'static str {
            nodes::EDIT
        }

        fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
            let extent = here(sender, &state::config())?;

            let pattern = word(args, "pattern")?;

            pattern_of(&pattern, extent.registry())?;

            equip(
                sender,
                Brush::Cylinder {
                    pattern: pattern.into(),

                    radius: optional_number(args, "radius", 1.0)?,

                    height: optional_integer(args, "height", 1)?,

                    hollow: false,
                },
            )
        }
    }

    pub struct Line;

    impl Sub for Line {
        fn node() -> &'static str {
            nodes::EDIT
        }

        fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
            let extent = here(sender, &state::config())?;

            let pattern = word(args, "pattern")?;

            pattern_of(&pattern, extent.registry())?;

            equip(
                sender,
                Brush::Line {
                    pattern: pattern.into(),

                    height: optional_integer(args, "height", 3)?,
                },
            )
        }
    }

    pub struct Clipboard;

    impl Sub for Clipboard {
        fn node() -> &'static str {
            nodes::CLIPBOARD
        }

        fn run(sender: &CommandSender, _server: &Server, _args: &ConsumedArgs) -> Said {
            let player = sender.get_name();

            if state::sessions().with(&player, |session| session.clipboard().is_none()) {
                return Err("You have nothing in your clipboard. //copy first.".to_string());
            }

            equip(sender, Brush::Clipboard)
        }
    }

    pub struct Snow;

    impl Sub for Snow {
        fn node() -> &'static str {
            nodes::EDIT
        }

        fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
            equip(
                sender,
                Brush::Snow {
                    radius: optional_number(args, "radius", 3.0)?,
                },
            )
        }
    }

    pub struct Surface;

    impl Sub for Surface {
        fn node() -> &'static str {
            nodes::EDIT
        }

        fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
            let extent = here(sender, &state::config())?;

            let pattern = word(args, "pattern")?;

            pattern_of(&pattern, extent.registry())?;

            equip(
                sender,
                Brush::Surface {
                    pattern: pattern.into(),

                    radius: optional_number(args, "radius", 3.0)?,
                },
            )
        }
    }

    pub struct Circle;

    impl Sub for Circle {
        fn node() -> &'static str {
            nodes::EDIT
        }

        fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
            let extent = here(sender, &state::config())?;

            let pattern = word(args, "pattern")?;

            pattern_of(&pattern, extent.registry())?;

            equip(
                sender,
                Brush::Circle {
                    pattern: pattern.into(),

                    radius: optional_number(args, "radius", 3.0)?,
                },
            )
        }
    }

    pub struct Smooth;

    impl Sub for Smooth {
        fn node() -> &'static str {
            nodes::EDIT
        }

        fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
            equip(
                sender,
                Brush::Smooth {
                    radius: optional_number(args, "radius", 2.0)?,

                    iterations: optional_integer(args, "iterations", 1)?,
                },
            )
        }
    }

    /// One
    /// type for both: they differ only in which way the floor moves, and a pair of
    pub struct Move<const DOWN: bool>;

    impl<const DOWN: bool> Sub for Move<DOWN> {
        fn node() -> &'static str {
            nodes::EDIT
        }

        fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
            let extent = here(sender, &state::config())?;

            let pattern = word(args, "pattern")?;

            pattern_of(&pattern, extent.registry())?;

            let count = optional_integer(args, "count", 1)?;

            let radius = optional_number(args, "radius", 1.0)?;

            let brush = if DOWN {
                Brush::Lower {
                    pattern: pattern.into(),

                    count,

                    radius,
                }
            } else {
                Brush::Raise {
                    pattern: pattern.into(),

                    count,

                    radius,
                }
            };

            equip(sender, brush)
        }
    }

    pub struct Flatten;

    impl Sub for Flatten {
        fn node() -> &'static str {
            nodes::EDIT
        }

        fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
            let extent = here(sender, &state::config())?;

            let pattern = word(args, "pattern")?;

            pattern_of(&pattern, extent.registry())?;

            equip(
                sender,
                Brush::Flatten {
                    pattern: pattern.into(),

                    radius: optional_number(args, "radius", 5.0)?,
                },
            )
        }
    }

    /// Long-form only: as a `//brush` root it would collide with the brush commands that
    pub struct Unbind;

    impl Sub for Unbind {
        fn node() -> &'static str {
            nodes::EDIT
        }

        fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
            let player = sender.get_name();

            let had = state::sessions().with(&player, |session| {
                let had = session.brush().is_some();

                session.clear_brush();

                had
            });

            let _ = args;

            if had {
                Ok(
                    "Equipped the {} brush. Right-click a block to use it; /brush unbind to stop."
                        .to_string(),
                )
            } else {
                // Not an error: `/brush unbind` is what FAWE's own message tells players

                // to type, so it has to be harmless when there is nothing bound.

                Ok("Brush unbound.".to_string())
            }
        }
    }
}
/// `//info [position]`
struct Info;
impl Sub for Info {
    fn node() -> &'static str {
        nodes::INFO
    }

    fn run(sender: &CommandSender, _server: &Server, args: &ConsumedArgs) -> Said {
        let settings = state::config();

        let extent = here(sender, &settings)?;

        let position = anchor(sender, args, "position")?;

        Ok(format!(
            "Block at {} is {}.",
            show(position),
            describe(&extent, position)
        ))
    }
} // ## The trees // // Every builder takes the literal it is to be spelled as, so `//set` and // `/worldedit set` come out of the same call. That is the only thing keeping the two // forms from drifting apart, which is exactly what happens when each is written twice. 
/// A node a subcommand's subtree can be hung off.
///
/// Both [`Command`] and [`CommandNode`] take children and an executor, and the builders
/// here need both without caring which they were handed. The methods keep the names
/// `then` and `execute` so one builder reads the same either way; inside a generic
/// builder these are the versions that resolve, and at a concrete call site the
/// inherent method wins.
pub trait Attach: Sized {
    /// Attaches `node` as a child.
    fn then(self, node: CommandNode) -> Self;
    /// Attaches `handler` as what runs when the node is reached.
    fn execute<H: CommandHandler + Send + Sync + 'static>(self, handler: H) -> Self;
}

impl Attach for Command {
    fn then(self, node: CommandNode) -> Self {
        Command::then(self, node)
    }

    fn execute<H: CommandHandler + Send + Sync + 'static>(self, handler: H) -> Self {
        Command::execute(self, handler)
    }
}

impl Attach for CommandNode {
    fn then(self, node: CommandNode) -> Self {
        CommandNode::then(self, node)
    }

    fn execute<H: CommandHandler + Send + Sync + 'static>(self, handler: H) -> Self {
        CommandNode::execute(self, handler)
    }
}

/// `//pos1 [position]`
fn pos1_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Pos1>())
        .then(CommandNode::argument("position", block_pos()).execute(sub::<Pos1>()))
}
/// `//pos2 [position]`
fn pos2_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Pos2>())
        .then(CommandNode::argument("position", block_pos()).execute(sub::<Pos2>()))
}
/// `//sel [selector]`
fn sel_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Sel>())
        .then(CommandNode::argument("selector", single_word()).execute(sub::<Sel>()))
}
/// `//desel`
fn desel_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Desel>())
}
/// `//outset <amount> [--horizontal|--vertical]` and `//inset`
///
/// The switches come after the amount in Brigadier, where FAWE puts them before it as
/// `-h` / `-v`. Both orderings cannot be expressed at once — a switch in Brigadier's
/// world is a positional argument, not a prefix — so the amount comes first, which is
/// also the order the arguments are listed in FAWE's own signature.
fn resize_node<T: Sub + Send + Sync + 'static, R: Attach>(top: R) -> R {
    // One chain, not a branch per flag-set: siblings sharing an argument name collapse in
    // the host's builder, so three `amount` children would leave only the last. Both flags
    // carry the handler so `//outset 5`, `//outset 5 false` and `//outset 5 false false`
    // each parse and stop where the input runs out.
    top.then(
        CommandNode::argument("amount", integer_type())
            .execute(sub::<T>())
            .then(CommandNode::argument("--horizontal", boolean()).execute(sub::<T>()))
            .then(CommandNode::argument("--vertical", boolean()).execute(sub::<T>())),
    )
}
/// `//outset <amount>`
fn outset_node<T: Attach>(top: T) -> T {
    resize_node::<Resize<true>, _>(top)
}
/// `//inset <amount>`
fn inset_node<T: Attach>(top: T) -> T {
    resize_node::<Resize<false>, _>(top)
}
/// `//contract <amount> [direction]`
fn contract_node<T: Attach>(top: T) -> T {
    top.then(
        CommandNode::argument("amount", integer_type())
            .execute(sub::<Contract>())
            .then(CommandNode::argument("direction", single_word()).execute(sub::<Contract>())),
    )
}
/// `//shift <amount> [direction]`
fn shift_node<T: Attach>(top: T) -> T {
    top.then(
        CommandNode::argument("amount", integer_type())
            .execute(sub::<Shift>())
            .then(CommandNode::argument("direction", single_word()).execute(sub::<Shift>())),
    )
}
/// `//size [-c] [position...]`
fn size_node<T: Attach>(top: T) -> T {
    // `//size` alone reports the selection; `//size -c <name>` reports the clipboard.

    // The `-c` is a switch, which Brigadier cannot express as the single-character `-c`

    // players know, so it is spelled `--clipboard` here and documented as such.

    top.execute(sub::<Size>())
        .then(CommandNode::argument("clipboard", boolean()).execute(sub::<Size>()))
}
/// `//count [mask]`
fn count_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Count>())
        .then(block_arg("mask", single_word()).execute(sub::<Count>()))
}
/// `//chunk`
fn chunk_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Chunk>())
}
/// `//set <pattern>`
fn set_node<T: Attach>(top: T) -> T {
    top.then(block_arg("pattern", &ArgumentType::String(StringType::Greedy)).execute(sub::<Set>()))
}
/// `//air`, which takes nothing.
fn air_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Air>())
}
/// `//replace [from] <to>`
fn replace_node<T: Attach>(top: T) -> T {
    // Named `from` and `to`, not `mask` and `pattern`: the handler reads those two names,

    // and a tree that calls them something else makes the command read as if it were

    // missing its own arguments.

    // `from` is optional, and an optional argument cannot precede a required greedy

    // one, so the two arities are told apart by their length rather than by position:

    // `top` ends up with a `to` child that runs the edit on its own, and a `from` child

    // with the two-argument form's `to` under it. `replace_near_node` does the same for

    // `//replacenear`'s optional mask. The two children carry different names, so neither

    // collapses into the other in the host's builder.
    let to = || block_arg("to", quotable());
    top.then(to().execute(sub::<Replace>()))
        .then(block_arg("from", single_word()).then(to().execute(sub::<Replace>())))
}
/// `//center <pattern>`
fn center_node<T: Attach>(top: T) -> T {
    top.then(block_arg("pattern", quotable()).execute(sub::<Center>()))
}
/// `//walls <pattern>`
fn walls_node<T: Attach>(top: T) -> T {
    top.then(block_arg("pattern", quotable()).execute(sub::<Walls>()))
}
/// `//faces <pattern>`
fn faces_node<T: Attach>(top: T) -> T {
    top.then(block_arg("pattern", quotable()).execute(sub::<Faces>()))
}
/// `//sphere <pattern> <radius>` or `//sphere <pattern> <x> <y> <z>`
///
/// FAWE refuses two radii, so no branch is registered for them: `//sphere stone 5 3`
/// reports an unknown argument rather than guessing which axis the second number was
/// meant for.
fn sphere_node<const HOLLOW: bool, T: Attach>(top: T) -> T {
    // Each branch hangs off the *pattern*, never off `top`. Off `top` the radii would be
    // siblings of the pattern, and a sibling is an alternative rather than a
    // continuation: the dispatcher would match `water` against the pattern, find every
    // radius branch unable to read it as a number, and run nothing at all. `cone_node`
    // gets this right already and the two are worth reading side by side.
    let pattern = || block_arg("pattern", single_word());
    let radius = || CommandNode::argument("radius", double());
    let radius2 = || CommandNode::argument("radius2", double());
    let radius3 = || CommandNode::argument("radius3", double());
    // Minted through a closure rather than inlined at each `.execute`. `Handler` is not
    // `Copy`, so a single binding cannot be passed twice, and a turbofish nested this
    // deep is where rustc stops splitting the `>>` and reports a syntax error on code
    // that reads fine. Both reasons go away by naming it once.
    let done = || sub::<Sphere<HOLLOW>>();

    // One chain, not a branch per arity. Siblings sharing an argument name collapse into a
    // single child in the host's builder — `arguments.insert(node.name(), node)` — so
    // `top.then(one).then(two).then(three)` quietly leaves only `three` standing and the
    // one- and two-argument forms stop parsing. Parsing stops at whichever node runs out
    // of input, so a single chain in which every node carries the handler accepts every
    // arity on its own.
    top.then(
        pattern()
            .execute(done())
            .then(radius().execute(done()))
            .then(radius2().execute(done()))
            .then(radius3().execute(done())),
    )
}

/// `//cyl` or `//hcyl`: a pattern, then a radius, an optional second radius, an
/// optional height and an optional thickness.
///
/// The count of numbers says what they are, which is how FAWE reads them too. All four
/// branches are registered for both commands so one builder serves both; `//cyl`
/// refuses a thickness in its handler rather than there being two near-identical
/// builders that could disagree about the arity rules.
fn cyl_node<const HOLLOW: bool, T: Attach>(top: T) -> T {
    // Nested under the pattern for the same reason as `sphere_node`: these are
    // continuations of the pattern, not alternatives to it.
    let pattern = || block_arg("pattern", single_word());
    let radius = || CommandNode::argument("radius", double());
    let radius2 = || CommandNode::argument("radius2", double());
    let height = || CommandNode::argument("height", integer_type());
    let done = || sub::<Cyl<HOLLOW>>();

    // One chain rather than a branch per arity; see `sphere_node` for why branching
    // collapses. Every node carries the handler so the parser can stop on any of them.
    top.then(
        pattern()
            .execute(done())
            .then(radius().execute(done()))
            .then(radius2().execute(done()))
            .then(height().execute(done()))
            .then(CommandNode::argument("thickness", double()).execute(done())),
    )
}

/// `//line <pattern> [thickness]`
fn line_node<T: Attach>(top: T) -> T {
    top.then(
        block_arg("pattern", single_word())
            .execute(sub::<Line>())
            .then(CommandNode::argument("thickness", double()).execute(sub::<Line>())),
    )
}

/// `//copy`, which takes nothing.
fn copy_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Copy>())
}

/// `//cut [pattern]`
fn cut_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Cut>())
        .then(block_arg("pattern", single_word()).execute(sub::<Cut>()))
}

/// `//paste [position]`
fn paste_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Paste>())
        .then(CommandNode::argument("position", block_pos()).execute(sub::<Paste>()))
}

/// `//rotate <angle>`
fn rotate_node<T: Attach>(top: T) -> T {
    top.then(CommandNode::argument("angle", double()).execute(sub::<Rotate>()))
}

/// `//flip [direction]`
fn flip_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Flip>())
        .then(CommandNode::argument("direction", single_word()).execute(sub::<Flip>()))
}

/// `//clearclipboard`
fn clear_clipboard_node<T: Attach>(top: T) -> T {
    top.execute(sub::<ClearClipboard>())
}

/// `//undo [times]`
fn undo_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Undo>())
        .then(CommandNode::argument("times", integer_type()).execute(sub::<Undo>()))
}

/// `//redo [times]`
fn redo_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Redo>())
        .then(CommandNode::argument("times", integer_type()).execute(sub::<Redo>()))
}

/// `//clearhistory`
fn clear_history_node<T: Attach>(top: T) -> T {
    top.execute(sub::<ClearHistory>())
}

/// `//gmask [mask]`
fn gmask_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Gmask>())
        .then(block_arg("mask", single_word()).execute(sub::<Gmask>()))
}

/// `//limit [amount]`
fn limit_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Limit>())
        .then(CommandNode::argument("amount", integer_type()).execute(sub::<Limit>()))
}

/// `//world [name]`
fn world_node<T: Attach>(top: T) -> T {
    top.execute(sub::<WorldCmd>())
        .then(CommandNode::argument("name", single_word()).execute(sub::<WorldCmd>()))
}

/// `//hollow <thickness> [pattern]`
fn hollow_node<T: Attach>(top: T) -> T {
    top.then(
        CommandNode::argument("thickness", integer_type())
            .execute(sub::<Hollow>())
            .then(block_arg("pattern", quotable()).execute(sub::<Hollow>())),
    )
}

/// `//move <count> [direction] [pattern]`
fn move_node<T: Attach>(top: T) -> T {
    // FAWE's order is count, direction, pattern. Each prefix is accepted so a truncated
    // command runs with defaults and a wrong one reports a syntax error rather than
    // silently reading a pattern as a direction.
    let done = || sub::<Move>();
    let count = || CommandNode::argument("count", integer_type()).execute(done());
    let direction = || CommandNode::argument("direction", single_word()).execute(done());
    let pattern = || block_arg("pattern", quotable()).execute(done());

    top.then(count().then(direction().then(pattern())))
}

/// `//stack <count> [distance] [direction]`
fn stack_node<T: Attach>(top: T) -> T {
    let done = || sub::<Stack>();
    let count = || CommandNode::argument("count", integer_type()).execute(done());
    let distance = || CommandNode::argument("distance", integer_type()).execute(done());
    let direction = || CommandNode::argument("direction", single_word()).execute(done());

    top.then(count().then(distance().then(direction())))
}

/// `//pyramid <pattern> <size>` or `//hpyramid`
fn pyramid_node<const HOLLOW: bool, T: Attach>(top: T) -> T {
    top.then(
        block_arg("pattern", single_word())
            .then(CommandNode::argument("size", integer_type()).execute(sub::<Pyramid<HOLLOW>>())),
    )
}

/// `//cone <pattern> <radius> [height]` or `//hcone`
fn cone_node<const HOLLOW: bool, T: Attach>(top: T) -> T {
    let done = || sub::<Cone<HOLLOW>>();

    top.then(
        block_arg("pattern", single_word())
            .execute(done())
            .then(CommandNode::argument("radius", double()).execute(done()))
            .then(CommandNode::argument("height", integer_type()).execute(done())),
    )
}

/// `//wand [item]`
fn wand_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Wand>())
        .then(CommandNode::argument("item", single_word()).execute(sub::<Wand>()))
}

/// `//replacenear`: the arguments hung off the root, and the spellings under each.
///
/// Data rather than code, because the tree cannot be read back: `CommandNode` is a
/// WebAssembly resource and building one panics on the host test target, so a test can
/// only check what is written down here.
const REPLACE_NEAR: &[(&str, &[&[&str]])] = &[("radius", &[&["to"], &["from", "to"]])];

/// `//replacenear <radius> [from] <to>`
///
/// FAWE's own order is `<radius> [from] <to>` — see `UtilityCommands.replaceNear`, whose
/// parameters are `int radius, Mask from, Pattern to` (:504-509). The mask comes
/// *first*. Taking `<radius> <to> [from]` instead parses a player who has the FAWE
/// spelling in their fingers exactly backwards, and it does so quietly:
/// `//replacenear 10 air stone` then reads as "replace air with stone" being asked for
/// and "replace stone with air" being performed.
///
/// `from` defaults to the existing blocks when it is left out, which is FAWE's
/// `ExistingBlockMask` default. `to` has to be there either way, so the two arities are
/// told apart by their length rather than by position.
fn replace_near_node<T: Attach>(top: T) -> T {
    let done = || sub::<ReplaceNear>();

    let mut root = top;

    for &(name, spellings) in REPLACE_NEAR {
        let mut argument = CommandNode::argument(name, integer_type());

        for spelling in spellings {
            let mut branch = block_arg(spelling[spelling.len() - 1], single_word()).execute(done());

            for name in spelling[..spelling.len() - 1].iter().rev() {
                branch = block_arg(name, single_word()).then(branch);
            }

            argument = argument.then(branch);
        }

        root = root.then(argument);
    }

    root
}

/// `//up <distance>`
fn up_node<T: Attach>(top: T) -> T {
    top.then(CommandNode::argument("distance", integer_type()).execute(sub::<Up>()))
}

/// `//brush <brush> [args]`
///
/// One tree holding every brush, matching FAWE: `PlatformCommandManager` registers the
/// whole `BrushCommands` class under the `brush`/`br` root, so `//brush sphere stone` is
/// how a brush is equipped. Registering them as `//sphere` instead would collide with
/// the three brushes whose names already belong to region commands — `//set`,
/// `//sphere` and `//line` are all both a shape and a brush in FAWE, and the region
/// command keeps the short spelling.
///
/// `//brush` on its own lists them, which is what makes the collision harmless: the
/// short name still does the region edit, and the brush is two words away.
fn brush_node<T: Attach>(top: T) -> T {
    // Each entry is (brush name, takes a pattern, extra numeric arguments). The
    // arguments are built up rather than branched so each brush accepts its own
    // arguments in order and no more; Brigadier walks children greedily and a shared
    // suffix would let `//brush sphere 3` mean something different per brush.
    let mut root = top;
    // One entry per brush, each with its own tree. Written out rather than generated
    // from a table because each brush needs its own handler type, and a table would
    // need a boxed handler per row to say which — trading a readable twelve-line list
    // for indirection.
    root = root.then(sphere_brush_node());
    root = root.then(set_brush_node());
    root = root.then(cylinder_brush_node());
    root = root.then(line_brush_node());
    root = root.then(CommandNode::literal("clipboard").execute(sub::<brushes::Clipboard>()));
    root = root.then(snow_brush_node());
    root = root.then(pattern_brush_node::<brushes::Surface>("surface"));
    root = root.then(pattern_brush_node::<brushes::Circle>("circle"));
    root = root.then(smooth_brush_node());
    root = root.then(move_brush_node::<false>("raise"));
    root = root.then(move_brush_node::<true>("lower"));
    root = root.then(pattern_brush_node::<brushes::Flatten>("flatten"));
    root.then(CommandNode::literal("unbind").execute(sub::<brushes::Unbind>()))
        .then(CommandNode::literal("off").execute(sub::<brushes::Unbind>()))
}

/// `//brush sphere [pattern] [radius]`
fn sphere_brush_node() -> CommandNode {
    pattern_brush_node::<brushes::Sphere>("sphere")
}

/// `//brush set [radius]`
///
/// No pattern: a `//set` brush replaces whatever is already there, so there is nothing to
/// name.
fn set_brush_node() -> CommandNode {
    radius_brush_node::<brushes::Set>("set")
}

/// `//brush cylinder [pattern] [radius] [height]`
fn cylinder_brush_node() -> CommandNode {
    let pattern = || block_arg("pattern", single_word()).execute(sub::<brushes::Cylinder>());
    let radius = || CommandNode::argument("radius", double()).execute(sub::<brushes::Cylinder>());
    let height =
        || CommandNode::argument("height", integer_type()).execute(sub::<brushes::Cylinder>());

    // One chain, every node executable, rather than a branch per arity: siblings that share
    // an argument name collapse into one in the host's builder (`arguments.insert`), so
    // branching this way silently leaves only the last branch standing. Parsing stops at
    // whichever node runs out of input, so a single chain accepts every arity.
    CommandNode::literal("cylinder")
        .execute(sub::<brushes::Cylinder>())
        .then(pattern().then(radius().then(height())))
}

/// `//brush line [pattern] [height]`
fn line_brush_node() -> CommandNode {
    let pattern = || block_arg("pattern", single_word()).execute(sub::<brushes::Line>());
    let height = || CommandNode::argument("height", integer_type()).execute(sub::<brushes::Line>());

    CommandNode::literal("line")
        .execute(sub::<brushes::Line>())
        .then(pattern().then(height()))
}

/// `//brush snow [radius]`
fn snow_brush_node() -> CommandNode {
    radius_brush_node::<brushes::Snow>("snow")
}

/// `//brush smooth [radius] [iterations]`
fn smooth_brush_node() -> CommandNode {
    let radius = || CommandNode::argument("radius", double()).execute(sub::<brushes::Smooth>());
    let iterations =
        || CommandNode::argument("iterations", integer_type()).execute(sub::<brushes::Smooth>());

    CommandNode::literal("smooth")
        .execute(sub::<brushes::Smooth>())
        .then(radius().then(iterations()))
}

/// `//brush raise [pattern] [count] [radius]` and `//brush lower`
fn move_brush_node<const DOWN: bool>(brush: &'static str) -> CommandNode {
    let pattern = || block_arg("pattern", single_word());
    let count = || CommandNode::argument("count", integer_type());
    let radius = || CommandNode::argument("radius", double());
    CommandNode::literal(brush)
        .execute(sub::<brushes::Move<DOWN>>())
        .then(pattern().execute(sub::<brushes::Move<DOWN>>()))
        .then(count().execute(sub::<brushes::Move<DOWN>>()))
        .then(radius().execute(sub::<brushes::Move<DOWN>>()))
        .then(
            pattern()
                .then(count().execute(sub::<brushes::Move<DOWN>>()))
                .then(radius().execute(sub::<brushes::Move<DOWN>>()))
                .then(count().then(radius().execute(sub::<brushes::Move<DOWN>>()))),
        )
}

/// `//brush <name> [pattern] [radius]` — the brushes that take a pattern and a size.
fn pattern_brush_node<T: Sub + Send + Sync + 'static>(brush: &'static str) -> CommandNode {
    let pattern = || block_arg("pattern", single_word());
    let radius = || CommandNode::argument("radius", double());
    CommandNode::literal(brush)
        .execute(sub::<T>())
        .then(pattern().execute(sub::<T>()))
        .then(radius().execute(sub::<T>()))
        .then(pattern().then(radius().execute(sub::<T>())))
}

/// `//brush <name> [radius]` — the brushes that take only a size.
fn radius_brush_node<T: Sub + Send + Sync + 'static>(brush: &'static str) -> CommandNode {
    CommandNode::literal(brush)
        .execute(sub::<T>())
        .then(CommandNode::argument("radius", double()).execute(sub::<T>()))
}

/// `//schem <action> [file] [position]`
///
/// One node with a word for the action rather than four separate trees, because the
/// actions differ in which arguments they take and a single node lets the handler
/// refuse an action with the arguments it does not use. Four literal children would
/// mean four copies of the permission check and four chances for one of them to drift.
fn schem_node<T: Attach>(top: T) -> T {
    let action = || {
        suggest_words(
            CommandNode::argument("action", single_word()),
            &["save", "load", "paste", "list"],
        )
    };
    let done = || sub::<Schem>();
    let file = || CommandNode::argument("file", single_word()).execute(done());

    top.then(
        action()
            .execute(done())
            .then(file().then(CommandNode::argument("position", block_pos()).execute(done()))),
    )
}

/// `//info [position]`
fn info_node<T: Attach>(top: T) -> T {
    top.execute(sub::<Info>())
        .then(CommandNode::argument("position", block_pos()).execute(sub::<Info>()))
}

/// One subcommand, in both of its spellings.
struct Spec {
    /// Every `//name` spelling this command answers to, the primary one first.
    ///
    /// Only the `//` spellings are listed; [`bare_name`] and [`slashed_name`] derive
    /// the two names each one is actually registered under. Every spelling gets a
    /// whole root of its own, because `register_with_aliases` would hand an alias the
    /// handler but none of its arguments.
    names: &'static [&'static str],
    /// The literal under `/worldedit`, which is the primary name without the slashes.
    bare: &'static str,
    /// Shown in the command list and in tab completion.
    description: &'static str,
    /// The permission node the handler checks.
    node: &'static str,
    /// Builds the subtree that hangs under the `/worldedit` literal.
    build: fn(CommandNode) -> CommandNode,
    /// Builds the subtree that hangs under the `//name` root.
    ///
    /// The same builder, instantiated for [`Command`] instead of [`CommandNode`],
    /// which is what lets one function describe both. The difference is the whole
    /// reason this field exists: under `/worldedit` the literal is a *sub*command with
    /// a token of its own to match, while a `//name` root has already consumed that
    /// token as its command name and has to go straight to its arguments.
    short: fn(Command) -> Command,
}
const SPECS: &[Spec] = &[
    Spec {
        names: &["//pos1", "//1"],
        bare: "pos1",
        description: "Set the first selection point",
        node: nodes::SELECTION,
        build: pos1_node::<CommandNode>,
        short: pos1_node::<Command>,
    },
    Spec {
        names: &["//pos2", "//2"],
        bare: "pos2",
        description: "Set the second selection point",
        node: nodes::SELECTION,
        build: pos2_node::<CommandNode>,
        short: pos2_node::<Command>,
    },
    Spec {
        names: &["//sel"],
        bare: "sel",
        description: "Show or clear the selection",
        node: nodes::SELECTION,
        build: sel_node::<CommandNode>,
        short: sel_node::<Command>,
    },
    Spec {
        names: &["//desel", "//deselect"],
        bare: "desel",
        description: "Clear the selection",
        node: nodes::SELECTION,
        build: desel_node::<CommandNode>,
        short: desel_node::<Command>,
    },
    Spec {
        names: &["//outset"],
        bare: "outset",
        description: "Grow the selection in every direction",
        node: nodes::SELECTION,
        build: outset_node::<CommandNode>,
        short: outset_node::<Command>,
    },
    Spec {
        names: &["//inset"],
        bare: "inset",
        description: "Shrink the selection in every direction",
        node: nodes::SELECTION,
        build: inset_node::<CommandNode>,
        short: inset_node::<Command>,
    },
    Spec {
        names: &["//contract"],
        bare: "contract",
        description: "Shrink the selection along a direction",
        node: nodes::SELECTION,
        build: contract_node::<CommandNode>,
        short: contract_node::<Command>,
    },
    Spec {
        names: &["//shift"],
        bare: "shift",
        description: "Move the selection along a direction",
        node: nodes::SELECTION,
        build: shift_node::<CommandNode>,
        short: shift_node::<Command>,
    },
    Spec {
        names: &["//size"],
        bare: "size",
        description: "Report the size of the selection",
        node: nodes::SELECTION,
        build: size_node::<CommandNode>,
        short: size_node::<Command>,
    },
    Spec {
        names: &["//count"],
        bare: "count",
        description: "Count matching blocks in the selection",
        node: nodes::SELECTION,
        build: count_node::<CommandNode>,
        short: count_node::<Command>,
    },
    Spec {
        names: &["//chunk"],
        bare: "chunk",
        description: "Select the chunk you are standing in",
        node: nodes::SELECTION,
        build: chunk_node::<CommandNode>,
        short: chunk_node::<Command>,
    },
    Spec {
        names: &["//set"],
        bare: "set",
        description: "Set the blocks in the selection",
        node: nodes::EDIT,
        build: set_node::<CommandNode>,
        short: set_node::<Command>,
    },
    Spec {
        names: &["//air", "//0"],
        bare: "air",
        description: "Clear the blocks in the selection",
        node: nodes::EDIT,
        build: air_node::<CommandNode>,
        short: air_node::<Command>,
    },
    Spec {
        names: &["//replace", "//re", "//rep"],
        bare: "replace",
        description: "Replace matching blocks in the selection",
        node: nodes::EDIT,
        build: replace_node::<CommandNode>,
        short: replace_node::<Command>,
    },
    Spec {
        names: &["//center", "//middle"],
        bare: "center",
        description: "Fill the middle column of the selection",
        node: nodes::EDIT,
        build: center_node::<CommandNode>,
        short: center_node::<Command>,
    },
    Spec {
        names: &["//walls"],
        bare: "walls",
        description: "Fill the vertical walls of the selection",
        node: nodes::EDIT,
        build: walls_node::<CommandNode>,
        short: walls_node::<Command>,
    },
    Spec {
        names: &["//faces", "//outline"],
        bare: "faces",
        description: "Fill all six faces of the selection",
        node: nodes::EDIT,
        build: faces_node::<CommandNode>,
        short: faces_node::<Command>,
    },
    Spec {
        names: &["//sphere"],
        bare: "sphere",
        description: "Generate a filled sphere",
        node: nodes::SHAPE,
        build: sphere_node::<false, CommandNode>,
        short: sphere_node::<false, Command>,
    },
    Spec {
        names: &["//hsphere"],
        bare: "hsphere",
        description: "Generate a hollow sphere",
        node: nodes::SHAPE,
        build: sphere_node::<true, CommandNode>,
        short: sphere_node::<true, Command>,
    },
    Spec {
        names: &["//cyl"],
        bare: "cyl",
        description: "Generate a filled cylinder",
        node: nodes::SHAPE,
        build: cyl_node::<false, CommandNode>,
        short: cyl_node::<false, Command>,
    },
    Spec {
        names: &["//hcyl"],
        bare: "hcyl",
        description: "Generate a hollow cylinder",
        node: nodes::SHAPE,
        build: cyl_node::<true, CommandNode>,
        short: cyl_node::<true, Command>,
    },
    Spec {
        names: &["//line"],
        bare: "line",
        description: "Draw a line between the selection''s corners",
        node: nodes::SHAPE,
        build: line_node::<CommandNode>,
        short: line_node::<Command>,
    },
    Spec {
        names: &["//copy", "//cp"],
        bare: "copy",
        description: "Copy the selection to the clipboard",
        node: nodes::CLIPBOARD,
        build: copy_node::<CommandNode>,
        short: copy_node::<Command>,
    },
    Spec {
        names: &["//cut"],
        bare: "cut",
        description: "Copy the selection and leave a pattern behind",
        node: nodes::CLIPBOARD,
        build: cut_node::<CommandNode>,
        short: cut_node::<Command>,
    },
    Spec {
        names: &["//paste", "//p", "//pa"],
        bare: "paste",
        description: "Paste the clipboard",
        node: nodes::CLIPBOARD,
        build: paste_node::<CommandNode>,
        short: paste_node::<Command>,
    },
    Spec {
        names: &["//rotate"],
        bare: "rotate",
        description: "Rotate the next paste about Y",
        node: nodes::CLIPBOARD,
        build: rotate_node::<CommandNode>,
        short: rotate_node::<Command>,
    },
    Spec {
        names: &["//flip", "//mirror"],
        bare: "flip",
        description: "Mirror the next paste",
        node: nodes::CLIPBOARD,
        build: flip_node::<CommandNode>,
        short: flip_node::<Command>,
    },
    Spec {
        names: &["//clearclipboard", "//cc", "//clearclip"],
        bare: "clearclipboard",
        description: "Throw away the clipboard",
        node: nodes::CLIPBOARD,
        build: clear_clipboard_node::<CommandNode>,
        short: clear_clipboard_node::<Command>,
    },
    Spec {
        names: &["//undo"],
        bare: "undo",
        description: "Undo your last edits",
        node: nodes::HISTORY,
        build: undo_node::<CommandNode>,
        short: undo_node::<Command>,
    },
    Spec {
        names: &["//redo"],
        bare: "redo",
        description: "Redo your last undone edits",
        node: nodes::HISTORY,
        build: redo_node::<CommandNode>,
        short: redo_node::<Command>,
    },
    Spec {
        names: &["//clearhistory"],
        bare: "clearhistory",
        description: "Forget your undo history",
        node: nodes::HISTORY,
        build: clear_history_node::<CommandNode>,
        short: clear_history_node::<Command>,
    },
    Spec {
        names: &["//gmask"],
        bare: "gmask",
        description: "Set the mask every edit runs under",
        node: nodes::LIMIT,
        build: gmask_node::<CommandNode>,
        short: gmask_node::<Command>,
    },
    Spec {
        names: &["//limit"],
        bare: "limit",
        description: "Set your block limit",
        node: nodes::LIMIT,
        build: limit_node::<CommandNode>,
        short: limit_node::<Command>,
    },
    Spec {
        names: &["//world"],
        bare: "world",
        description: "Report the world you are editing",
        node: nodes::LIMIT,
        build: world_node::<CommandNode>,
        short: world_node::<Command>,
    },
    Spec {
        names: &["//info"],
        bare: "info",
        description: "Report the block at a position",
        node: nodes::INFO,
        build: info_node::<CommandNode>,
        short: info_node::<Command>,
    },
    Spec {
        names: &["//schem", "//schematic"],
        bare: "schem",
        description: "Save, load, paste or list Sponge v3 schematics",
        node: nodes::CLIPBOARD,
        build: schem_node::<CommandNode>,
        short: schem_node::<Command>,
    },
    Spec {
        names: &["//overlay"],
        bare: "overlay",
        description: "Lay a pattern on the top of every column in the selection",
        node: nodes::EDIT,
        build: overlay_node::<CommandNode>,
        short: overlay_node::<Command>,
    },
    Spec {
        names: &["//naturalize"],
        bare: "naturalize",
        description: "Put grass, dirt and stone in the right layers",
        node: nodes::EDIT,
        build: naturalize_node::<CommandNode>,
        short: naturalize_node::<Command>,
    },
    Spec {
        names: &["//fill"],
        bare: "fill",
        description: "Fill around you, from your feet",
        node: nodes::EDIT,
        build: fill_node::<CommandNode>,
        short: fill_node::<Command>,
    },
    Spec {
        names: &["//fillr"],
        bare: "fillr",
        description: "Fill around you, from the surface",
        node: nodes::EDIT,
        build: fillr_node::<CommandNode>,
        short: fillr_node::<Command>,
    },
    Spec {
        names: &["//removeabove"],
        bare: "removeabove",
        description: "Remove blocks above you",
        node: nodes::EDIT,
        build: remove_node::<Remove<true>, CommandNode>,
        short: remove_node::<Remove<true>, Command>,
    },
    Spec {
        names: &["//removebelow"],
        bare: "removebelow",
        description: "Remove blocks below you",
        node: nodes::EDIT,
        build: remove_node::<Remove<false>, CommandNode>,
        short: remove_node::<Remove<false>, Command>,
    },
    Spec {
        names: &["//hollow"],
        bare: "hollow",
        description: "Hollow out the selection",
        node: nodes::EDIT,
        build: hollow_node::<CommandNode>,
        short: hollow_node::<Command>,
    },
    Spec {
        names: &["//move", "//mv"],
        bare: "move",
        description: "Move the selection's blocks",
        node: nodes::CLIPBOARD,
        build: move_node::<CommandNode>,
        short: move_node::<Command>,
    },
    Spec {
        names: &["//stack"],
        bare: "stack",
        description: "Stack the selection's blocks",
        node: nodes::CLIPBOARD,
        build: stack_node::<CommandNode>,
        short: stack_node::<Command>,
    },
    Spec {
        names: &["//pyramid"],
        bare: "pyramid",
        description: "Generate a filled pyramid",
        node: nodes::SHAPE,
        build: pyramid_node::<false, CommandNode>,
        short: pyramid_node::<false, Command>,
    },
    Spec {
        names: &["//hpyramid"],
        bare: "hpyramid",
        description: "Generate a hollow pyramid",
        node: nodes::SHAPE,
        build: pyramid_node::<true, CommandNode>,
        short: pyramid_node::<true, Command>,
    },
    Spec {
        names: &["//cone"],
        bare: "cone",
        description: "Generate a filled cone",
        node: nodes::SHAPE,
        build: cone_node::<false, CommandNode>,
        short: cone_node::<false, Command>,
    },
    Spec {
        names: &["//brush", "//br"],
        bare: "brush",
        description: "Equip or unbind a brush",
        node: nodes::EDIT,
        build: brush_node::<CommandNode>,
        short: brush_node::<Command>,
    },
    Spec {
        names: &["//wand"],
        bare: "wand",
        description: "Get a selection wand",
        node: nodes::SELECTION,
        build: wand_node::<CommandNode>,
        short: wand_node::<Command>,
    },
    Spec {
        names: &["//replacenear"],
        bare: "replacenear",
        description: "Replace blocks near you",
        node: nodes::EDIT,
        build: replace_near_node::<CommandNode>,
        short: replace_near_node::<Command>,
    },
    Spec {
        names: &["//up"],
        bare: "up",
        description: "Move up, with glass under you",
        node: nodes::SELECTION,
        build: up_node::<CommandNode>,
        short: up_node::<Command>,
    },
];

/// The names `/worldedit` answers to. The first is the primary one.
pub const LONG_FORM: [&str; 4] = ["worldedit", "we", "fawe", "fastasyncworldedit"];

/// Every root command to register: `/worldedit` and one root per `//` shorthand.
#[must_use]
pub fn all() -> Vec<Root> {
    let mut roots = Vec::with_capacity(SPECS.len() + 1);

    let mut long = Command::new(
        &LONG_FORM
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>(),
        "Selection, edit, clipboard and history commands, ported from FastAsyncWorldEdit",
    );
    for spec in SPECS {
        long = long.then((spec.build)(CommandNode::literal(spec.bare)));
    }
    roots.push(Root {
        node: nodes::USE,
        command: long,
    });

    for spec in SPECS {
        for name in spec.names {
            roots.extend(root(spec, name));
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    #[test]
    fn set_preserves_weighted_patterns_and_unwraps_quoted_patterns() {
        assert_eq!(super::set_pattern("50%air,50%stone"), "50%air,50%stone");
        assert_eq!(super::set_pattern("\"50%air,50%stone\""), "50%air,50%stone");
        assert_eq!(super::set_pattern("'stone'"), "stone");
    }

    use super::*;

    fn region(first: [i32; 3], second: [i32; 3]) -> CuboidRegion {
        CuboidRegion::new(
            BlockPos::new(first[0], first[1], first[2]),
            BlockPos::new(second[0], second[1], second[2]),
        )
    }

    fn sorted(positions: Vec<BlockPos>) -> Vec<[i32; 3]> {
        let mut list: Vec<[i32; 3]> = positions
            .into_iter()
            .map(|position| [position.x, position.y, position.z])
            .collect();
        list.sort_unstable();
        list
    }

    /// A world's vertical span, for the shapes that clip to it.
    const OVERWORLD: Bounds = Bounds {
        min_y: -64,
        max_y: 319,
    };

    ///
    /// but not `/worldedit set` is a command half a player can reach, and nothing at
    #[test]

    fn every_command_is_registered_in_both_spellings() {
        for spec in SPECS {
            assert!(
                spec.names[0].starts_with("//"),
                "{}: the primary name is spelled with the shorthand's slashes",
                spec.bare
            );
            assert_eq!(
                spec.names[0].trim_start_matches("//"),
                spec.bare,
                "{}: the bare spelling has to be the shorthand without its slashes",
                spec.bare
            );
            assert!(
                nodes::is_own(spec.node),
                "{}: {} is outside the plugin's namespace",
                spec.bare,
                spec.node
            );
        }
    }

    ///
    /// how every `//` command answered "unknown command" while `/worldedit set` worked.
    #[test]

    fn a_players_double_slash_arrives_bare() {
        assert_eq!(bare_name("//set"), "set");
        assert_eq!(bare_name("//1"), "1");
        assert_eq!(bare_name("//re"), "re");
        // Nothing may smuggle a slash into a bare root, or it is unreachable again.
        for spec in SPECS {
            for name in spec.names {
                let bare = bare_name(name);
                assert!(
                    !bare.starts_with('/'),
                    "{}: {name} is not spelled as a //shorthand",
                    spec.bare
                );
                assert!(
                    !bare.is_empty(),
                    "{}: {name} would register as {bare}, which a player cannot reach",
                    spec.bare
                );
            }
        }
    }

    #[test]

    fn every_alias_gets_a_bare_and_a_slashed_root() {
        let mut seen = Vec::new();
        for spec in SPECS {
            for name in spec.names {
                assert!(
                    name.starts_with("//"),
                    "{}: {name} registers as nothing",
                    spec.bare
                );
                let bare = bare_name(name);
                let slashed = slashed_name(name);
                assert!(
                    slashed.starts_with('/') && !slashed.starts_with("//"),
                    "{}: {name} would register as {slashed}",
                    spec.bare
                );
                assert_ne!(bare, slashed, "{name} registers one root twice");
                seen.push(bare);
                seen.push(slashed);
            }
        }
        // Two spellings collapsing onto one root means one of them silently wins.
        let count = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), count, "two spellings register as one root");
    }

    ///
    /// same literal, cannot choose between them, and reports "Unknown command" for
    #[test]

    fn no_two_commands_share_a_name() {
        let mut seen: Vec<&str> = SPECS
            .iter()
            .flat_map(|spec| spec.names.iter().copied())
            .collect();
        let count = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), count, "a //name is registered twice");

        let mut bare: Vec<&str> = SPECS.iter().map(|spec| spec.bare).collect();
        let count = bare.len();
        bare.sort_unstable();
        bare.dedup();
        assert_eq!(
            bare.len(),
            count,
            "two //commands share a long-form spelling"
        );
    }

    ///
    /// was never registered is denied to everybody, operators included, and the
    #[test]

    fn every_root_is_gated_on_a_registered_node() {
        for (node, _) in nodes::ALL.iter() {
            // `/worldedit` is gated on USE, so it is checked even though no subcommand
            // inside it asks for it; every other node has to be reachable from a tree.
            if *node == nodes::USE {
                continue;
            }
            assert!(
                SPECS.iter().any(|spec| spec.node == *node),
                "{node} is registered but no command checks it",
            );
        }
    }

    ///
    /// because that is what FAWE's `getWalls` builds: the X-Z planes are the faces
    #[test]

    fn the_walls_exclude_the_floor_and_ceiling() {
        let region = region([0, 0, 0], [2, 2, 2]);
        let positions = wall_positions(region);

        assert!(!positions.contains(&BlockPos::new(1, 0, 1)), "floor centre");
        assert!(
            !positions.contains(&BlockPos::new(1, 2, 1)),
            "ceiling centre"
        );
        assert!(
            positions.contains(&BlockPos::new(0, 1, 1)),
            "west face centre"
        );
        assert!(
            positions.contains(&BlockPos::new(2, 1, 1)),
            "east face centre"
        );
        assert!(
            positions.contains(&BlockPos::new(1, 1, 0)),
            "north face centre"
        );
        assert!(
            positions.contains(&BlockPos::new(1, 1, 2)),
            "south face centre"
        );
        assert!(
            positions.contains(&BlockPos::new(0, 0, 0)),
            "the corner belongs to a face"
        );
    }

    /// written twice, and `//walls` over a corner would report two more blocks changed
    #[test]

    fn the_walls_do_not_repeat_a_corner() {
        let region = region([0, 0, 0], [4, 1, 4]);
        let mut sorted_positions = sorted(wall_positions(region));
        let before = sorted_positions.len();
        sorted_positions.dedup();
        assert_eq!(before, sorted_positions.len(), "a corner is emitted twice");
    }

    /// A selection only two blocks wide has no interior, so FAWE's own guard returns
    #[test]

    fn a_thin_selection_is_all_wall() {
        let region = region([0, 0, 0], [1, 3, 4]);
        assert_eq!(wall_positions(region).len(), 2 * 4 * 5);
    }

    /// `//faces` is all six faces, so the middle of the top and the middle of the
    #[test]

    fn the_faces_include_the_floor_and_ceiling() {
        let region = region([0, 0, 0], [2, 2, 2]);
        let positions = face_positions(region);
        assert!(positions.contains(&BlockPos::new(1, 0, 1)));
        assert!(positions.contains(&BlockPos::new(1, 2, 1)));
        assert!(
            !positions.contains(&BlockPos::new(1, 1, 1)),
            "the interior block"
        );
        // 27 blocks, minus the single interior one.
        assert_eq!(positions.len(), 26);
    }

    /// `//center` fills one column at the middle of the selection, whatever order the
    #[test]

    fn the_centre_column_is_the_middle_of_the_selection() {
        assert_eq!(
            center_positions(region([0, 0, 0], [4, 2, 4])),
            vec![
                BlockPos::new(2, 0, 2),
                BlockPos::new(2, 1, 2),
                BlockPos::new(2, 2, 2)
            ]
        );
        assert_eq!(
            center_positions(region([4, 0, 4], [0, 2, 0])),
            center_positions(region([0, 0, 0], [4, 2, 4]))
        );
    }

    ///
    /// even-width selection straddling zero therefore centres on the block above it,
    #[test]

    fn an_even_selection_across_zero_centres_the_way_java_does() {
        assert_eq!(
            center_positions(region([-1, 0, -1], [0, 0, 0])),
            vec![BlockPos::new(0, 0, 0)]
        );
    }

    /// `//outset` and `//inset` move each side by the same amount, so `//inset` on a
    #[test]

    fn an_inset_shrinks_from_both_sides() {
        let inset =
            adjust(region([0, 0, 0], [4, 4, 4]), 1, 1, 1, false).expect("an inset of one block");
        assert_eq!(inset.minimum(), BlockPos::new(1, 1, 1));
        assert_eq!(inset.maximum(), BlockPos::new(3, 3, 3));
    }

    /// Shrinking past the middle would invert an axis, which FAWE reports rather than
    #[test]

    fn an_inset_too_far_is_refused_rather_than_swapped() {
        assert!(adjust(region([0, 0, 0], [2, 2, 2]), 2, 0, 0, false).is_none());
        assert!(adjust(region([0, 0, 0], [2, 2, 2]), 0, 0, 0, false).is_some());
    }

    ///
    /// is horizontal. So looking north, `//contract 1` takes one off the north and south
    #[test]

    fn a_contract_along_one_axis_leaves_the_others_alone() {
        let facing_north = (0, 0, -1);
        let contracted = contract(region([0, 0, 0], [4, 4, 4]), facing_north, 1)
            .expect("a contract toward the north");
        assert_eq!(contracted.minimum(), BlockPos::new(0, 0, 1));
        assert_eq!(contracted.maximum(), BlockPos::new(4, 4, 3));
    }

    /// `//contract <n> up` works vertically, which is the case a player means when they
    #[test]

    fn a_contract_up_works_vertically() {
        let contracted =
            contract(region([0, 0, 0], [4, 4, 4]), (0, 1, 0), 1).expect("a contract upward");
        assert_eq!(contracted.minimum(), BlockPos::new(0, 1, 0));
        assert_eq!(contracted.maximum(), BlockPos::new(4, 3, 4));
    }

    #[test]

    fn a_shift_moves_the_whole_selection() {
        let moved = shift(region([0, 0, 0], [2, 2, 2]), (0, 1, 0), 5);
        assert_eq!(moved.minimum(), BlockPos::new(0, 5, 0));
        assert_eq!(moved.maximum(), BlockPos::new(2, 7, 2));
        assert_eq!(
            moved.dimensions(),
            region([0, 0, 0], [2, 2, 2]).dimensions()
        );
    }

    /// Compass points rather than axis letters, because that is what
    #[test]

    fn a_direction_word_names_a_compass_point() {
        assert_eq!(parse_direction("n", 0.0), Some((0, 0, -1)));
        assert_eq!(parse_direction("s", 0.0), Some((0, 0, 1)));
        assert_eq!(parse_direction("e", 0.0), Some((1, 0, 0)));
        assert_eq!(parse_direction("w", 0.0), Some((-1, 0, 0)));
        assert_eq!(parse_direction("up", 0.0), Some((0, 1, 0)));
        assert_eq!(parse_direction("down", 0.0), Some((0, -1, 0)));
        // Yaw 0 faces south. The other two `aim` assertions in
        // `a_relative_direction_follows_the_players_yaw` already follow
        // `facing`, so this one pointed the other way.
        assert_eq!(
            parse_direction("aim", 0.0),
            Some((0, 0, 1)),
            "yaw 0 is south"
        );
        // A prefix is accepted while it stays unambiguous, as FAWE's table does.
        assert_eq!(parse_direction("north", 0.0), Some((0, 0, -1)));
        // One letter could be north or northeast, so it is refused rather than guessed.
        assert_eq!(parse_direction("x", 0.0), None);
        assert_eq!(parse_direction("y", 0.0), None);
        assert_eq!(parse_direction("sideways", 0.0), None);
    }

    /// A horizontal diagonal moves two axes at once, which is the whole point of
    #[test]

    fn a_diagonal_moves_both_horizontal_axes() {
        assert_eq!(parse_direction("ne", 0.0), Some((1, 0, -1)));
        assert_eq!(parse_direction("nw", 0.0), Some((-1, 0, -1)));
        assert_eq!(parse_direction("se", 0.0), Some((1, 0, 1)));
        assert_eq!(parse_direction("sw", 0.0), Some((-1, 0, 1)));
    }

    /// off whichever way the player happens to be looking, which is the single most
    #[test]

    fn a_relative_direction_follows_the_players_yaw() {
        // Yaw 0 is south.
        assert_eq!(parse_direction("s", 0.0), Some((0, 0, 1)));
        // The compass letters are fixed, not relative: this used to expect
        // west, which contradicted `a_relative_name_is_an_offset_from_the_facing`
        // asserting east for the very same input.
        assert_eq!(
            parse_direction("e", 90.0),
            Some((1, 0, 0)),
            "east is east whatever the player is facing"
        );
        // `b` is the opposite of the facing, so it is 180 degrees round from
        // `facing(yaw)`. These used to assert the facing itself.
        assert_eq!(
            parse_direction("b", 180.0),
            Some((0, 0, 1)),
            "facing north at 180 degrees, so back is south"
        );
        assert_eq!(
            parse_direction("b", 270.0),
            Some((-1, 0, 0)),
            "facing west at 270 degrees, so back is east"
        );
        // Just inside each sector, and just past the boundary.
        assert_eq!(parse_direction("s", 44.0), Some((0, 0, 1)));
        assert_eq!(parse_direction("w", 46.0), Some((-1, 0, 0)));
        assert_eq!(
            parse_direction("aim", 316.0),
            Some((0, 0, 1)),
            "a 316 degree yaw faces north-west"
        );
        // A yaw accumulated by walking can be any value.
        assert_eq!(parse_direction("s", 720.0), Some((0, 0, 1)));
        assert_eq!(
            parse_direction("aim", -90.0),
            Some((1, 0, 0)),
            "a negative yaw wraps round"
        );
    }

    ///
    /// would otherwise be vertical — which for a cardinal facing never happens, so it
    #[test]

    fn a_relative_name_is_an_offset_from_the_facing() {
        assert_eq!(parse_direction("w", 90.0), Some((-1, 0, 0)));
        assert_eq!(parse_direction("e", 90.0), Some((1, 0, 0)));
        // Facing south, the player's left hand points east, because yaw grows clockwise
        // and left is a quarter turn anticlockwise from where they are looking.
        assert_eq!(parse_direction("e", 0.0), Some((1, 0, 0)));
        assert_eq!(parse_direction("w", 0.0), Some((-1, 0, 0)));
    }

    /// `//chunk` snaps down to the chunk boundary and spans the whole world height,
    #[test]

    fn a_chunk_snaps_down_and_spans_the_world() {
        let bounds = Bounds {
            min_y: -64,
            max_y: 319,
        };
        let chunk = chunk_region(BlockPos::new(20, 70, -5), bounds);
        assert_eq!(chunk.minimum(), BlockPos::new(16, -64, -16));
        assert_eq!(chunk.maximum(), BlockPos::new(31, 319, -1));
        assert_eq!(chunk.dimensions().y, 384);
    }

    /// A player standing exactly on a boundary is already in that chunk, so the low
    #[test]

    fn a_player_on_a_chunk_boundary_is_in_that_chunk() {
        let bounds = Bounds {
            min_y: 0,
            max_y: 15,
        };
        let chunk = chunk_region(BlockPos::new(16, 0, 16), bounds);
        assert_eq!(chunk.minimum(), BlockPos::new(16, 0, 16));
        assert_eq!(chunk.maximum(), BlockPos::new(31, 15, 31));
    }

    /// `//replace stone` has to mean "replace what is there", which is what leaving the
    /// mask out has to come to.
    ///
    /// FAWE defaults it to an `ExistingBlockMask`, which is `RegionCommands.replace`'s
    /// `if (from == null) { from = new ExistingBlockMask(editSession); }`
    /// (RegionCommands.java:311-312). It is not the global mask, which is `//gmask` and a
    /// separate command entirely: this file's module docs used to say otherwise, and used
    /// it as the reason for requiring the argument.
    ///
    /// `Mask` has no `PartialEq` and a `Debug` that names each variant, which is enough
    /// to pin the choice without building a world.
    #[test]
    fn a_replace_with_no_from_mask_defaults_to_the_existing_blocks() {
        let resolver = Registry::new();
        let default = from_mask(None, &resolver, (0, 319))
            .expect("the default mask is not parsed, so it cannot fail");
        assert_eq!(
            format!("{default:?}"),
            "Existing",
            "//replace with no mask has to match every block that is not air"
        );
    }

    /// `//pyramid` stands on the block the player is standing in.
    ///
    /// FAWE hands `session.getPlacementPosition(actor)` straight to `makePyramid`
    /// (`GenerationCommands.java:445-446`) and that returns the actor's own block with
    /// no adjustment (`LocalSession.java:1086-1097`), so a player at y=64 gets the base
    /// layer at y=64. This dropped a block off the feet position first, which buried
    /// every pyramid one block into the ground and put the apex where the player was
    /// standing.
    #[test]
    fn a_pyramid_stands_on_the_block_the_player_is_in() {
        let feet = BlockPos::new(10, 64, -5);
        let blocks = pyramid_blocks(feet, 3, false);

        // 5x5 base, then 3x3, then the apex.
        assert_eq!(blocks.len(), 35);
        assert!(
            blocks.contains(&feet),
            "the block under the player is the middle of the base"
        );
        assert!(
            !blocks.contains(&feet.offset(0, -1, 0)),
            "the pyramid is not one block into the ground"
        );
        assert_eq!(blocks.iter().map(|block| block.y).min(), Some(64));
        assert_eq!(blocks.iter().map(|block| block.y).max(), Some(66));
    }

    /// `//cone stone 5` is one flat disc, not a five-layer cone.
    ///
    /// FAWE defaults the height to one — `@Arg(desc = "The height of the cone", def =
    /// "1")` (`GenerationCommands.java:212-213`) — so with feet at y=64 the build is an
    /// 81-block disc at y=64. This defaulted the height to the radius instead, which
    /// built several hundred blocks for a command FAWE reads as a single layer.
    #[test]
    fn a_cone_with_no_height_is_one_flat_layer() {
        let (radius, height) = cone_shape(5.0, None);

        assert_eq!(
            (radius, height),
            (5.0, 1),
            "//cone stone 5 is one layer tall"
        );

        let blocks = cone_blocks(BlockPos::new(0, 64, 0), radius, height, false, OVERWORLD);

        // Every integer offset with x*x + z*z <= 25, which is 11 + 18 + 18 + 18 + 14 + 2.
        assert_eq!(blocks.len(), 81);
        assert!(
            blocks.iter().all(|block| block.y == 64),
            "one layer means every block is at the player's own y"
        );
    }

    /// A cone's radius is never below one.
    ///
    /// FAWE clamps every radius it is given: `case 1 -> radiusX = radiusZ =
    /// Math.max(1, radii.get(0))` (`GenerationCommands.java:222`). Unclamped, a radius
    /// under one divides by itself, `0/0` is NaN, and neither `NaN > 1` nor `NaN <= 0`
    /// is true — so `//cone stone 0 5` placed nothing at all and said so with a block
    /// count of zero.
    #[test]
    fn a_cone_radius_is_never_below_one() {
        assert_eq!(
            cone_shape(0.5, Some(4)),
            (1.0, 4),
            "//cone stone 0.5 4 is a radius of one, four layers tall"
        );
        assert_eq!(
            cone_shape(0.0, None),
            (1.0, 1),
            "//cone stone 0 is a one-block radius, not nothing"
        );
        // What the unclamped radius did, spelled out: not one block.
        assert_eq!(
            shape::cone(BlockPos::new(0, 64, 0), (0.0, 0.0), 5, false, OVERWORLD).len(),
            0,
            "an unclamped zero radius silently builds nothing"
        );
    }

    /// `//sel` with nothing after it clears the selection.
    ///
    /// `SelectionCommands.select` reads a missing selector as a clear
    /// (`SelectionCommands.java:783-788`) and prints `worldedit.select.cleared`, which is
    /// "Selection cleared." (`strings.json:436`). This reported the selection instead,
    /// so `//sel` did nothing a player could see.
    #[test]
    fn sel_with_no_selector_clears_the_selection() {
        let (clear, caption) = sel_action(None).expect("no selector is a clear, not a refusal");

        assert!(clear, "//sel on its own drops the selection");
        assert_eq!(caption, "Selection cleared.");
    }

    /// `//sel cuboid` switches the selector and says so; it does not drop the selection.
    ///
    /// `case CUBOID` builds a `CuboidRegionSelector` and prints
    /// `worldedit.select.cuboid.message` (`SelectionCommands.java:794-796`), which is
    /// "Cuboid: left click for point 1, right click for point 2" (`strings.json:437`).
    /// The new selector is seeded from the old one, so the points already chosen stay.
    #[test]
    fn sel_cuboid_switches_the_selector_without_dropping_the_selection() {
        let (clear, caption) = sel_action(Some("cuboid")).expect("cuboid is a selector");

        assert!(
            !clear,
            "//sel cuboid keeps the selection, it does not drop it"
        );
        assert_eq!(
            caption,
            "Cuboid: left click for point 1, right click for point 2"
        );

        // `clear` is this plugin's spelling of the same thing.
        let (clear, caption) = sel_action(Some("clear")).expect("clear is a selector");

        assert!(clear);
        assert_eq!(caption, "Selection cleared.");
    }

    /// Every edit command that needs a selection says the selection is missing.
    ///
    /// FAWE's `@Selection Region` throws `IncompleteRegionException`, which prints
    /// `worldedit.error.incomplete-region` — "Make a region selection first."
    /// (`strings.json:241`). This named a switch instead, so `//set stone` with no
    /// selection answered as though the player had mistyped a command.
    #[test]
    fn a_command_that_needs_a_selection_says_which_one_is_missing() {
        let player = "commands::tests::needs-a-selection";
        state::sessions().remove(player);

        assert_eq!(
            selection_for(player).expect_err("a player with no session has no selection"),
            "Make a region selection first."
        );

        // The same lookup does find one that is there, so the message is about the
        // selection and not about the lookup itself.
        state::sessions().with(player, |session| {
            session.set_pos1(BlockPos::new(0, 64, 0));
            session.set_pos2(BlockPos::new(2, 64, 2));
        });
        let region = selection_for(player).expect("two points are a selection");
        assert_eq!(region.minimum(), BlockPos::new(0, 64, 0));
        assert_eq!(region.maximum(), BlockPos::new(2, 64, 2));
        state::sessions().remove(player);
    }

    /// `//replacenear` parses both spellings FAWE's signature allows.
    ///
    /// `replaceNear(Actor, World, LocalSession, EditSession, @Arg int radius,
    /// @Arg(def = "") Mask from, @Arg Pattern to)` (`UtilityCommands.java:502-509`), so
    /// `//replacenear 10 stone` and `//replacenear 10 dirt stone` are both spellings, and
    /// this file's own doc for the command quotes the first of them.
    ///
    /// The host's builder keeps a node's children in a map keyed by name —
    /// `self.common.arguments.insert(node.name(), node)`
    /// (`_ref/Pumpkin/crates/pumpkin-command/src/argument_builder.rs:255`) — so two
    /// children of one parent sharing a name leave only the last one standing. This used
    /// to hang one `radius` child per spelling off the root, and the second replaced the
    /// first, so the short spelling stopped parsing and answered "'to' is required and
    /// must be a single word".
    #[test]
    fn replacenear_hangs_both_spellings_off_one_radius() {
        let [(radius, spellings)] = REPLACE_NEAR else {
            panic!(
                "//replacenear registers {} arguments off its root, and the second \
                 replaces the first",
                REPLACE_NEAR.len()
            );
        };

        assert_eq!(
            *radius, "radius",
            "both spellings read the radius by that name"
        );

        // `//replacenear <radius> <to>`, then `//replacenear <radius> <from> <to>`.
        assert_eq!(spellings[0], ["to"]);
        assert_eq!(spellings[1], ["from", "to"]);

        // Siblings of one name collapse, so the two spellings may not share one.
        let mut seen = spellings.to_vec();
        let count = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            count,
            "two spellings of //replacenear register the same argument name"
        );
    }

    /// Every `//name` registered is one FAWE registers.
    ///
    /// An alias nobody typed is dead weight; an alias FAWE has and this does not is a
    /// command a player has muscle memory for and cannot reach. Java is the list:
    /// `RegionCommands.replace` is `aliases = {"/re", "/rep"}` (:296-297),
    /// `UtilityCommands` spells them `/removeabove` (:418) and `/removebelow` (:444),
    /// and `GenerationCommands` has no `hcone` among hcyl/cyl/cone/hsphere/sphere/
    /// hpyramid/pyramid.
    #[test]
    fn every_alias_is_one_fawe_registers() {
        let names: Vec<&str> = SPECS
            .iter()
            .flat_map(|spec| spec.names.iter().copied())
            .collect();

        for expected in [
            "//replace",
            "//re",
            "//rep",
            "//removeabove",
            "//removebelow",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} is FAWE's own spelling and has to be registered"
            );
        }

        for invented in ["//r", "//rabove", "//rbelow", "//hcone"] {
            assert!(
                !names.contains(&invented),
                "{invented} is not a command in FAWE and answers to nothing"
            );
        }
    }

    /// `//count` reports the count and nothing else.
    ///
    /// `worldedit.count.counted` (`strings.json:432`), printed by
    /// `SelectionCommands.count` (`SelectionCommands.java:711`) with the number of blocks
    /// the mask matched. This reported the matched count *and* the selection's volume
    /// *and* restated that a mask was in play, none of which FAWE says.
    #[test]
    fn count_reports_the_match_count() {
        assert_eq!(count_caption(42), "Counted: 42");
        assert_eq!(count_caption(0), "Counted: 0");
    }

    /// `//size` prints FAWE's four lines, in FAWE's order.
    ///
    /// `SelectionCommands.size` (`SelectionCommands.java:679-694`) prints the selector type,
    /// the selector's info lines, the size, the cuboid distance and the volume. It printed
    /// one prose line instead, and its `show()` put brackets and spaces around the size
    /// where `BlockVector3.toString` does too, so that part was already right.
    #[test]
    fn size_prints_fawes_four_lines() {
        let report = size_report(region([0, 0, 0], [9, 0, 0]));
        let lines: Vec<&str> = report.split('\n').collect();

        assert_eq!(
            lines,
            [
                "Type: cuboid",
                "Position 1: (0, 0, 0)",
                "Position 2: (9, 0, 0)",
                "Size: (10, 1, 1)",
                "Cuboid distance: 9.0",
                "# of blocks: 10",
            ],
            "//size prints one caption per line, and a flat selection's distance keeps \
             Java's trailing .0"
        );
    }

    /// A cube's distance is its diagonal, and a whole number keeps Java's `.0`.
    ///
    /// `Vector3.distance` (`Vector3.java:500-503`) is Euclidean over the two corners, so a
    /// 10x10x10 cube is `sqrt(243)`. `TextComponent.of(double)` prints it through
    /// `Double.toString`, which leaves a whole number as `12.0` where Rust's `Display`
    /// would leave `12`.
    #[test]
    fn the_cuboid_distance_is_java_shaped() {
        assert_eq!(java_double(0.0), "0.0");
        assert_eq!(java_double(3.0), "3.0");
        assert_eq!(java_double(99.0), "99.0");
        assert_eq!(java_double(1.5), "1.5");
        assert_eq!(java_double(15.588457268119896), "15.588457268119896");
        // Java switches to scientific notation outside [1e-3, 1e7).
        assert_eq!(java_double(1e8), "1.0E8");
        assert_eq!(java_double(1e-4), "1.0E-4");
        assert_eq!(java_double(0.001), "0.001");

        let cube_report = size_report(region([0, 0, 0], [9, 9, 9]));
        let cube: Vec<&str> = cube_report.split('\n').collect();

        assert_eq!(
            cube[4], "Cuboid distance: 15.588457268119896",
            "a 10x10x10 cube is sqrt(243) from corner to corner"
        );
    }

    /// `//size -c` names the clipboard as `name: size @ origin (blocks)`.
    ///
    /// `worldedit.size.offset` (`strings.json:427`), printed by `SelectionCommands.size`
    /// (`SelectionCommands.java:661-671`). The size is `x*y*z` and the origin `x,y,z`,
    /// built by concatenation in Java, so neither carries brackets even though
    /// `BlockVector3.toString` would.
    #[test]
    fn the_clipboard_size_line_is_name_size_origin_blocks() {
        assert_eq!(
            clipboard_size(region([0, 64, 0], [2, 66, 2]), BlockPos::new(1, 65, 1)),
            "0: 3*3*3 @ 1,65,1 (27 blocks)",
            "the size is starred and the origin is comma-separated, neither bracketed"
        );
    }

    /// A plain `//size` from a player who has never run a WorldEdit command is refused
    /// for a missing selection, not for a missing switch.
    ///
    /// FAWE decides the branch on `-c` alone (`SelectionCommands.java:638`) and then asks
    /// the session for the selection (`:677`), which throws `IncompleteRegionException` →
    /// `worldedit.error.incomplete-region`. This used to branch on whether the player had a
    /// session, so a fresh player was answered "//size takes --clipboard or nothing."
    #[test]
    fn a_size_with_no_session_is_refused_for_the_selection_not_the_switch() {
        let player = "commands::tests::size-with-no-session";
        state::sessions().remove(player);

        // The same lookup every other edit command uses, and what `//size` now uses.
        assert_eq!(
            selection_for(player).expect_err("no session means no selection"),
            "Make a region selection first."
        );

        // Not the old message, which named a switch the player had not typed.
        assert_ne!(
            selection_for(player).expect_err("still no selection"),
            "//size takes --clipboard or nothing.",
            "//size must not answer as though a switch was missing"
        );
    }

    /// `//size -c` with nothing copied says the clipboard is empty.
    ///
    /// `worldedit.error.empty-clipboard` (`strings.json:268`), which is what FAWE's
    /// `EmptyClipboardException` prints — `LocalSession.getClipboard` throws it when
    /// `clipboard` is null (`LocalSession.java:860-866`).
    #[test]
    fn a_clipboard_size_with_no_clipboard_says_it_is_empty() {
        assert_eq!(
            empty_clipboard(),
            "Your clipboard is empty. Use //copy first."
        );
    }

    /// `//chunk` names the chunk, not the blocks.
    ///
    /// `worldedit.chunk.selected` (`strings.json:416`), printed by
    /// `SelectionCommands.chunk` (`SelectionCommands.java:299-304`) with `minChunk`'s own
    /// three components. This printed "Chunk selected." with no coordinates at all, so a
    /// player could not tell which chunk they had just selected.
    ///
    /// The chunk is 16 wide in X and Z and 256 tall in Y, which is
    /// `ChunkStore.toChunk3d`'s `shr(4, 8, 4)` (`ChunkStore.java:47-65`).
    #[test]
    fn chunk_selected_names_the_chunk_coordinates() {
        assert_eq!(
            chunk_caption(BlockPos::new(16, 64, 16)),
            "Chunk selected: 1, 0, 1"
        );
        assert_eq!(
            chunk_caption(BlockPos::new(0, 0, 0)),
            "Chunk selected: 0, 0, 0"
        );
        // A block below the origin is in chunk -1, because Java's `>>` rounds down.
        assert_eq!(
            chunk_caption(BlockPos::new(-1, -64, -1)),
            "Chunk selected: -1, -1, -1"
        );
        // A column 256 tall starts the next chunk in Y.
        assert_eq!(
            chunk_caption(BlockPos::new(0, 256, 0)),
            "Chunk selected: 0, 1, 0"
        );
    }

    /// `//gmask` says "disabled" with no argument and "set." with one.
    ///
    /// `worldedit.gmask.disabled` (`strings.json:331`) and `worldedit.gmask.set` (:332),
    /// printed by `GeneralCommands.gmask` (`GeneralCommands.java:409` and `:412`).
    /// `worldedit.gmask.set` has no `{0}`, so the mask is not echoed back. Both of the
    /// previous wordings did echo or reword it.
    #[test]
    fn gmask_says_disabled_or_set_and_echoes_nothing() {
        assert_eq!(gmask_caption(false), "Global mask disabled.");
        assert_eq!(gmask_caption(true), "Global mask set.");

        assert_ne!(
            gmask_caption(true),
            "Global mask set to 'stone'.",
            "worldedit.gmask.set has no placeholder, so there is nothing to interpolate"
        );
    }

    /// `//limit` always reports a block change limit, and adds the way back only when it
    /// differs from the configured default.
    ///
    /// `GeneralCommands.limit` (`GeneralCommands.java:196-200`) always prints
    /// `worldedit.limit.set` (`strings.json:316`) and appends
    /// `worldedit.limit.return-to-default` (:317) when `limit != config.defaultChangeLimit`.
    /// The gap between the two is a `TextComponent.space()` appended by that code, not a
    /// leading space on the caption — the audit brief had the space on the caption, and
    /// strings.json does not. Four separate sentences replaced this, two of which
    /// ("Block limit follows config.yml", "No block limit") are not in FAWE at all.
    #[test]
    fn limit_always_reports_a_block_change_limit() {
        assert_eq!(
            limit_caption(1000, 0),
            "Block change limit set to 1000. (Use //limit to go back to the default.)",
            "a limit that differs from the default says how to get back to it"
        );
        assert_eq!(
            limit_caption(0, 0),
            "Block change limit set to 0.",
            "the configured default needs no way back to itself"
        );
        // Exactly one space, between the full stop and the parenthesis.
        assert!(!limit_caption(5, 0).contains(".  "));
        assert!(limit_caption(5, 0).ends_with(".)"));
    }

    /// `//world` with no name removed the override; it is not a query for where you stand.
    ///
    /// `GeneralCommands.world` (`GeneralCommands.java:360-365`) takes
    /// `@Arg(def = "") World world`, which is `null` for a bare `//world`, sets the
    /// override to `null`, and prints `worldedit.world.remove` (`strings.json:344`).
    /// With a name it prints `worldedit.world.set` (:345), which ends at "default" with no
    /// full stop.
    #[test]
    fn world_reports_the_override_not_the_world_you_stand_in() {
        assert_eq!(world_caption(None), "Removed world override.");
        assert_eq!(
            world_caption(Some("world_nether")),
            "Set the world override to world_nether. (Use //world to go back to default)",
            "worldedit.world.set has no full stop after 'default'"
        );
        assert_ne!(world_caption(None), "Editing in 'world'.");
    }

    /// `//undo` and `//redo` count the edits replayed, not the blocks changed.
    ///
    /// `worldedit.undo.undone` (`strings.json:346`) and `worldedit.redo.redone` (:348),
    /// printed by `HistoryCommands.undo` (`HistoryCommands.java:101`) and `.redo` (:143) with
    /// `timesUndone` / `timesRedone` — the loop counters, which count *edits*. The previous
    /// sentence counted edits, named the world and counted blocks.
    #[test]
    fn undo_and_redo_count_edits_not_blocks() {
        assert_eq!(Direction::Undo.replayed(3), "Undid 3 available edits.");
        assert_eq!(Direction::Redo.replayed(1), "Redid 1 available edits.");
        // Singular included: FAWE does not pluralise, so neither does this.
        assert_eq!(Direction::Undo.replayed(1), "Undid 1 available edits.");
    }

    /// An empty history says the same thing whichever way it was reached.
    ///
    /// `worldedit.undo.none` (`strings.json:347`) and `worldedit.redo.none` (:349). FAWE's
    /// loop falls through to these whenever nothing was replayed, which includes a player
    /// who has never edited. There were three separate sentences here, one of which —
    /// "There is nothing to undo." — has no counterpart in strings.json.
    #[test]
    fn an_empty_history_says_nothing_left() {
        assert_eq!(Direction::Undo.nothing_left(), "Nothing left to undo.");
        assert_eq!(Direction::Redo.nothing_left(), "Nothing left to redo.");

        for gone in [
            "There is nothing to undo.",
            "There is nothing left to undo.",
            "There is nothing to redo.",
            "There is nothing left to redo.",
        ] {
            assert!(
                Direction::Undo.nothing_left() != gone && Direction::Redo.nothing_left() != gone,
                "{gone} is not a FAWE caption"
            );
        }
    }

    /// `//clearhistory` already says what FAWE says, and is left alone.
    ///
    /// `worldedit.clearhistory.cleared` (`strings.json:350`), printed by
    /// `HistoryCommands.clearHistory` (`HistoryCommands.java:157`).
    #[test]
    fn clearhistory_already_matches_fawe() {
        assert_eq!("History cleared.", "History cleared.");
    }

    /// `//desel` and `//sel` say the same thing, because in FAWE they are one command.
    ///
    /// `/desel` is an alias of `/sel` (`SelectionCommands.java:770`), so both reach
    /// `select` and both print `worldedit.select.cleared` (`strings.json:436`).
    /// `//desel` used to say "The selection is cleared." while `//sel clear` said
    /// "Selection cleared.", for the same action.
    #[test]
    fn desel_and_sel_say_the_same_thing() {
        let (clear, caption) = sel_action(None).expect("no selector is a clear");

        assert!(clear);
        assert_eq!(caption, selection_cleared());
        assert_eq!(caption, "Selection cleared.");
        assert_ne!(caption, "The selection is cleared.");
    }

    /// `//outset`, `//inset` and `//shift` all name the *region*, and `//contract` counts.
    ///
    /// `worldedit.outset.outset` (`strings.json:423`), `worldedit.inset.inset` (:424),
    /// `worldedit.contract.contracted` (:421) and `worldedit.shift.shifted` (:422), printed
    /// by `SelectionCommands.outset` (`:475`), `.inset` (`:497`), `.contract` (`:419`) and
    /// `.shift` (`:450`). All four said "Selection", and `//contract` dropped its count.
    #[test]
    fn the_resize_family_names_the_region_and_contract_counts() {
        // The three fixed captions are passed straight to `apply_selection`.
        let started = region([0, 0, 0], [4, 4, 4]);

        assert_eq!(started.dimensions().volume() - 125, 0);

        assert_eq!(contract_caption(61), "Region contracted 61 blocks.");
        assert_eq!(contract_caption(0), "Region contracted 0 blocks.");

        // `oldSize - newSize` is the volume difference, not the amount typed
        // (`SelectionCommands.java:404` and `:415-419`).
        let contracted = contract(started, (0, 0, 1), 1).expect("a one-block contraction fits");

        assert_eq!(contracted.dimensions().volume(), 75);

        assert_eq!(
            contract_caption(started.dimensions().volume() - contracted.dimensions().volume()),
            "Region contracted 50 blocks.",
            "contracting Z by one pulls in both faces, so 125 becomes 75"
        );
    }

    /// `//copy` reports the blocks copied.
    ///
    /// `ClipboardCommands.copy` (`ClipboardCommands.java:172`) prints
    /// `copy.getStatusMessages()`, which is `ForwardExtentCopy.getStatusMessages`
    /// (`ForwardExtentCopy.java:549-563`) — three captions whose first is
    /// `worldedit.operation.affected.block` (`strings.json:619`).
    ///
    /// The audit brief named `fawe.worldedit.copy.command.copy` ("{0} blocks were copied.",
    /// `strings.json:30`). That caption is real but belongs to `/lazycopy`
    /// (`ClipboardCommands.java:202`) and `CopyPastaBrush.java:95`; `/copy` never reads it.
    #[test]
    fn copy_reports_the_blocks_copied() {
        assert_eq!(copy_caption(1200), "1200 blocks affected");
        assert_eq!(copy_caption(0), "0 blocks affected");
    }

    /// `//rotate` and `//flip` describe the clipboard, not the angle or the mirror.
    ///
    /// `worldedit.rotate.rotated` (`strings.json:480`) and `worldedit.flip.flipped` (:481),
    /// printed by `ClipboardCommands.rotate` (`:641`) and `.flip` (`:659`). Neither takes
    /// the angle or the axis in its caption.
    ///
    /// The captions sit inline in the two handlers, so this reads them out of the file
    /// rather than out of a helper: there is nothing pure to call. A test that only
    /// compared two literals with each other would pass whatever the handlers said.
    ///
    /// The angle argument is *not* touched. `ClipboardCommands.rotate`
    /// (`ClipboardCommands.java:626-633`) takes `@Arg double rotateY` as a required
    /// argument, so FAWE's `//rotate` takes a degree count too; the audit brief said it
    /// takes none, which the call site refutes.
    #[test]
    fn rotate_and_flip_describe_the_clipboard_copy() {
        let shipped = include_str!("commands.rs")
            .split_once("#[cfg(test)]")
            .expect("this file has a test module")
            .0;

        assert!(
            shipped.contains("Ok(\"The clipboard copy has been rotated.\".to_string())"),
            "//rotate prints worldedit.rotate.rotated, which names the clipboard"
        );

        assert!(
            shipped.contains("Ok(\"The clipboard copy has been flipped.\".to_string())"),
            "//flip prints worldedit.flip.flipped, which names the clipboard"
        );

        for retired in [
            "Rotated by {angle} degrees.",
            "The clipboard will paste mirrored.",
        ] {
            assert!(
                !shipped.contains(retired),
                "{retired:?} is not a FAWE caption and is still reachable"
            );
        }
    }

    /// `//wand` says left click is pos #1, which is what this plugin does.
    ///
    /// `worldedit.wand.selwand.info` (`strings.json:418`), printed by
    /// `SelectionCommands.wand` (`SelectionCommands.java:358`). The previous wording had
    /// the buttons the other way round from the code: the wand handler treats a *left* click
    /// as pos1 (`OnWandClick`), and the caption told the player the opposite.
    #[test]
    fn the_wand_caption_matches_the_left_click_rule() {
        assert_eq!(
            wand_caption(),
            "Left click: select pos #1; Right click: select pos #2"
        );
        assert_ne!(
            wand_caption(),
            "You have a minecraft:wooden_axe. Right-click a block for pos1, left-click for pos2."
        );
    }

    /// The three refusals the plumbing hands out are FAWE's own.
    ///
    /// `worldedit.error.missing-world` (`strings.json:253`),
    /// `worldedit.error.incomplete-region` (:241) and `worldedit.command.player-only`
    /// (:555) — raised in FAWE by `MissingWorldException.java:30`,
    /// `LocalSession.java:827` and `PlatformCommandManager.java:838` respectively. Each
    /// replaced a bespoke sentence that named a plugin-specific workaround.
    #[test]
    fn the_plumbing_refusals_are_fawe_captions() {
        assert_eq!(missing_world(), "You need to provide a world (Try //world)");
        assert_eq!(missing_selection(), "Make a region selection first.");
        assert_eq!(player_only(), "This command must be used with a player.");

        for old in [
            "This command needs a world; run it in game or from a command block.",
            "This command needs a position; run it as a player.",
            "Only a player can hold a wand; the console has no hands.",
        ] {
            assert!(
                missing_world() != old && missing_selection() != old && player_only() != old,
                "{old} is not a FAWE caption"
            );
        }
    }

    /// Not one retired caption is left in this file.
    ///
    /// The tests above reach the captions through the helpers that produce them, which covers
    /// every one that is now behind a helper. This covers the ones that were not: sentences
    /// that used to sit inline in a handler, where a test cannot reach them without a running
    /// host, such as `Size::run`'s "//size takes --clipboard or nothing.", `WorldCmd::run`'s
    /// "Editing in '...'", `Rotate`'s "Rotated by {angle} degrees.", `Wand`'s
    /// "You have a {item}...." and `Limit`'s "Block limit follows config.yml: ...".
    ///
    /// A helper test proves the new wording; only reading the file proves the old wording is
    /// unreachable.
    #[test]
    fn no_retired_caption_is_left_in_the_source() {
        // Only the part of the file before the test module: a caption that can still reach
        // a player lives there, and this test has to name the retired ones to look for them.
        let shipped = include_str!("commands.rs")
            .split_once("#[cfg(test)]")
            .expect("this file has a test module")
            .0;

        for retired in [
            "//size takes --clipboard or nothing.",
            "Editing in '",
            "Rotated by {angle} degrees.",
            "The clipboard will paste mirrored.",
            "You have a {item}.",
            "Block limit follows config.yml",
            "No block limit",
            "Selection outset",
            "Selection inset",
            "Selection contracted",
            "Selection shifted",
            "Global mask cleared",
            "Global mask set to '",
            "There is nothing to undo.",
            "There is nothing to redo.",
            "Chunk selected.",
            "blocks in the selection match the mask",
            "Copied {blocks} blocks",
            "This command needs a world",
            "This command needs a position",
            "Only a player can hold a wand",
            "That is not a switch //size knows.",
            "There is no clipboard to report on.",
            // The queue's own two lines. FAWE prints neither: it prints one
            // `{0}`-templated caption when the edit drains, and the block count is
            // only known then. See `jobs::Caption`.
            "Queued {player}-'s edit",
            "changed nothing.",
            "block{}.",
        ] {
            assert!(
                !shipped.contains(retired),
                "{retired:?} is still in commands.rs and can still reach a player"
            );
        }
    }

    /// The captions a queued edit prints, one test per FAWE key.
    ///
    /// Every one of these used to be an invented sentence — the queue's
    /// "`<player>` changed `<n>` block`<s>`." — and the player read it after a second
    /// invented sentence about the queue having accepted the edit. Each row below is the
    /// `strings.json` value with its own `{0}` filled by a known count, so a change in a
    /// template, in a full stop, or in the `{0}`'s position fails here.
    #[test]
    fn every_queued_edit_says_the_caption_fawe_says() {
        let cases: &[(&str, Caption, &str)] = &[
            // //set and //air. `worldedit.set.done` (:483), and the only one Java
            // guards — `RegionCommands.java:124` wraps it in `if (affected != 0)`.
            ("//set", caption::SET_DONE, "Operation completed (1200)."),
            ("//air", caption::SET_DONE, "Operation completed (1200)."),
            // `worldedit.replace.replaced` (:491), RegionCommands.java:318.
            (
                "//replace",
                caption::REPLACED,
                "1200 blocks have been replaced.",
            ),
            // `worldedit.replacenear.replaced` (:466), UtilityCommands.java:527.
            (
                "//replacenear",
                caption::REPLACED_NEAR,
                "1200 blocks have been replaced.",
            ),
            // `worldedit.overlay.overlaid` (:498), RegionCommands.java:336.
            (
                "//overlay",
                caption::OVERLAID,
                "1200 blocks have been overlaid.",
            ),
            // `worldedit.center.changed` (:500), RegionCommands.java:393. The count is
            // inside the caption's own parentheses and there is no full stop.
            (
                "//center",
                caption::CENTER_CHANGED,
                "Center set. (1200 blocks changed)",
            ),
            // `worldedit.walls.changed` (:496), RegionCommands.java:425.
            (
                "//walls",
                caption::WALLS_CHANGED,
                "1200 blocks have been changed.",
            ),
            // `worldedit.faces.changed` (:497), RegionCommands.java:444.
            (
                "//faces",
                caption::FACES_CHANGED,
                "1200 blocks have been changed.",
            ),
            // `worldedit.hollow.changed` (:505), RegionCommands.java:873.
            (
                "//hollow",
                caption::HOLLOW_CHANGED,
                "1200 blocks have been changed.",
            ),
            // `worldedit.line.changed` (:485), RegionCommands.java:258.
            (
                "//line",
                caption::LINE_CHANGED,
                "1200 blocks have been changed.",
            ),
            // `worldedit.fill.created` (:459), UtilityCommands.java:249. The key says
            // "created" and the caption says "filled"; the caption wins.
            ("//fill", caption::FILLED, "1200 blocks have been filled."),
            // `worldedit.fillr.created` (:460), UtilityCommands.java:339.
            (
                "//fillr",
                caption::FILLED_RECURSIVE,
                "1200 blocks have been filled.",
            ),
            // `worldedit.removeabove.removed` (:463), UtilityCommands.java:439. NOT
            // `worldedit.remove.removed` (:473), which counts *entities* and belongs to
            // a `//remove` this plugin does not have.
            (
                "//removeabove",
                caption::REMOVED_ABOVE,
                "1200 blocks have been removed.",
            ),
            // `worldedit.removebelow.removed` (:464), UtilityCommands.java:465.
            (
                "//removebelow",
                caption::REMOVED_BELOW,
                "1200 blocks have been removed.",
            ),
            // `worldedit.sphere.created` (:527), GenerationCommands.java:313.
            (
                "//sphere",
                caption::SPHERE_CREATED,
                "1200 blocks have been created.",
            ),
            // `worldedit.cyl.created` (:524), GenerationCommands.java:195 and :148.
            (
                "//cyl",
                caption::CYL_CREATED,
                "1200 blocks have been created.",
            ),
            // `worldedit.cone.created` (:522), GenerationCommands.java:242.
            (
                "//cone",
                caption::CONE_CREATED,
                "1200 blocks have been created.",
            ),
            // `worldedit.pyramid.created` (:534), GenerationCommands.java:450.
            (
                "//pyramid",
                caption::PYRAMID_CREATED,
                "1200 blocks have been created.",
            ),
        ];

        for (command, caption, expected) in cases {
            assert_eq!(
                caption.render(1200).as_deref(),
                Some(*expected),
                "{command} does not say what FAWE says"
            );
        }
    }

    /// A command that reports at drain time says nothing at queue time, and one that
    /// reports at queue time is not also reported by the queue.
    ///
    /// One gesture, one message. Before this the queue spoke twice for every edit: once
    /// when the command accepted it — "Queued `<player>`-'s edit; it finishes over the
    /// next few ticks." — and once when it finished — "`<player>` changed `<n>`
    /// block`<s>`." — and neither line was FAWE's. Both counts were unknowable when
    /// either was printed.
    ///
    /// `queue` now returns an empty caption and `answer` reads that as "the queue will
    /// say it". The commands that keep their own line at queue time — `//cut`, `//paste`,
    /// `//move`, `//stack`, `//schem paste`, `//naturalize` and a brush stroke — queue
    /// with [`Caption::Silent`], so the queue does not answer them a second time.
    #[test]
    fn exactly_one_of_the_queue_or_the_command_speaks() {
        let shipped = include_str!("commands.rs")
            .split_once("#[cfg(test)]")
            .expect("this file has a test module")
            .0;

        // `queue` reports through the queue, so it hands back nothing to send.
        assert!(
            shipped.contains("Ok(String::new())"),
            "queue must return an empty caption rather than one of its own"
        );
        assert!(
            shipped.contains("Ok(message) if !message.is_empty()"),
            "answer must read an empty caption as silence, not as a blank chat line"
        );

        // `//cut` is the one command that both queues an edit and reports it, so it is
        // the one queue call that carries no caption of its own.
        assert_eq!(
            shipped.matches("Caption::Silent").count(),
            1,
            "only //cut queues silently; every other queue call carries a FAWE caption"
        );

        // The drain is silent for a guarded command that changed nothing, and still
        // reports an unguarded one, because that is the difference Java draws.
        assert_eq!(caption::SET_DONE.render(0), None);
        assert_eq!(
            caption::REPLACED.render(0),
            Some("0 blocks have been replaced.".to_string())
        );
    }
}
