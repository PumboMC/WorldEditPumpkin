//! Reading WorldEdit's arguments out of the command tree.
//!
//! Pumpkin's `ConsumedArgs::get_value` answers a key the command was not given
//! with `Arg::Simple("")`, so an empty string is how absence is spelled and asking
//! for an argument that was not supplied is safe. That is why almost everything
//! here is "empty means absent" rather than an `Option`.

use pumpkin_plugin_api::{
    Server,
    command::{
        Arg, ArgumentType, CommandNode, CommandSender, CommandSuggestions, ConsumedArgs,
        StringType, SuggestionRequest,
    },
    commands::CommandSuggestionHandler,
    wit::pumpkin::plugin::command::{CommandSuggestion, NotInBounds, Number},
};

use crate::{commands::java_double, world::Registry};

/// A required single-word argument.
///
/// ## Why the refusal is not `fawe.error.command.syntax`
///
/// The obvious caption is `fawe.error.command.syntax`, "Usage: {0}" (`strings.json:85`),
/// and it is deliberately not used here. Its `{0}` is always a syntax string the class
/// already knows — every one of its twenty-five call sites is a pattern, mask or
/// transform parser passing its own `getSyntax()`, plus `Scroll` passing
/// `"clipboard [file]"` (`Scroll.java:59`). No command handler passes one, because a
/// command whose required argument is missing never reaches its handler: the argument is
/// resolved before the method is invoked.
///
/// So there is no usage line to interpolate, and the name alone would leave the player
/// with nothing they can act on. This wording stays until the tree can carry one.
pub fn word(args: &ConsumedArgs, name: &str) -> Result<String, String> {
    match args.get_value(name) {
        Arg::Simple(value) if !value.is_empty() => Ok(value),
        _ => Err(format!("'{name}' is required and must be a single word")),
    }
}

/// An optional single-word argument.
///
/// The `_` arm fires when the node was declared as something that is not a string at
/// all, which is a tree that disagrees with its handler rather than a mistyped command,
/// and no key in `strings.json` describes it. See [`word`] for why
/// `fawe.error.command.syntax` is not used.
pub fn optional_word(args: &ConsumedArgs, name: &str) -> Result<Option<String>, String> {
    match args.get_value(name) {
        Arg::Simple(value) if value.is_empty() => Ok(None),
        Arg::Simple(value) => Ok(Some(value)),
        _ => Err(format!("'{name}' must be a single word")),
    }
}

/// A required number, from a `double` or `integer` argument.
pub fn number(args: &ConsumedArgs, name: &str) -> Result<f64, String> {
    match args.get_value(name) {
        // A `double`, an `integer`, a `float` and a `long` all arrive as `Arg::Num`,
        // so every one of them is unwrapped here rather than in each caller.
        Arg::Num(Ok(number)) => Ok(match number {
            Number::Float64(value) => value,
            Number::Float32(value) => f64::from(value),
            Number::Int32(value) => f64::from(value),
            Number::Int64(value) => value as f64,
        }),
        Arg::Num(Err(bounds)) => Err(outside_range(name, bounds)),
        // An absent argument. See [`word`] for why this is not
        // `fawe.error.command.syntax`.
        Arg::Simple(value) if value.is_empty() => Err(format!("'{name}' is required")),
        Arg::Simple(value) => Err(invalid_number(&value)),
        _ => Err(invalid_number_given()),
    }
}

/// A number the player typed that is not one, naming what they typed.
///
/// `worldedit.error.invalid-number.matches` (`strings.json:260`), printed by
/// `WorldEditExceptionConverter.convert(NumberFormatException)`
/// (`WorldEditExceptionConverter.java:80-91`) when the exception's message matches
/// `^For input string: "(.*)"$`. The quoted text is the string the parser choked on,
/// which is exactly the string this arm was handed.
pub fn invalid_number(typed: &str) -> String {
    format!("Number expected; string \"{typed}\" given.")
}

/// [`invalid_number`] without the quoted text.
///
/// `worldedit.error.invalid-number` (`strings.json:261`), which the same converter
/// reaches at `WorldEditExceptionConverter.java:89` for a `NumberFormatException`
/// whose message does not name the offending input. Used where the input is not a
/// string this plugin still has — a node declared with a different argument type, or a
/// `block-pos` whose components were never a number.
pub fn invalid_number_given() -> String {
    "Number expected; string given.".to_string()
}

/// A number that fell outside the bounds the argument node declared for it.
///
/// `fawe.error.outside-range-lower` (`strings.json:126`) and
/// `fawe.error.outside-range-upper` (:127), in the shape `GenerationCommands.ore`
/// uses them (`GenerationCommands.java:770-775`):
/// `Caption.of("fawe.error.outside-range-lower", "miny", editSession.getMinY())`.
///
/// There is also a `fawe.error.outside-range` (`:125`, "Argument {0} outside of range
/// {1}-{2}."), but it needs both ends of the range and only one is ever known here.
///
/// Unreachable with this plugin's own trees — [`double`] and [`integer_type`] both
/// declare `(None, None)`, which the host does not clamp against — so it is here for a
/// node that does declare a range.
pub fn outside_range(name: &str, bounds: NotInBounds) -> String {
    match bounds {
        NotInBounds::LowerBound((_, bound)) => {
            format!("Argument {name} may not be less than {}", bound_text(bound))
        }
        NotInBounds::UpperBound((_, bound)) => {
            format!(
                "Argument {name} may not be greater than {}",
                bound_text(bound)
            )
        }
    }
}

/// A bound as Java writes it, for the `outside-range` captions.
///
/// `Caption.of` renders a non-`Component` argument through
/// `TextComponent.of(Objects.toString(arg))` (`Caption.java:132-137`), so an integer
/// bound is written the way Java writes an integer and a floating-point one the way
/// Java writes a `double`.
fn bound_text(number: Number) -> String {
    match number {
        Number::Int32(value) => value.to_string(),
        Number::Int64(value) => value.to_string(),
        Number::Float32(value) => java_double(f64::from(value)),
        Number::Float64(value) => java_double(value),
    }
}

/// An optional number, falling back to `default` when the argument is absent.
pub fn optional_number(args: &ConsumedArgs, name: &str, default: f64) -> Result<f64, String> {
    match args.get_value(name) {
        Arg::Simple(value) if value.is_empty() => Ok(default),
        _ => number(args, name),
    }
}

/// A whole number, from a `double` or `integer` argument.
///
/// A number too large for an `int` is `worldedit.error.invalid-number`
/// (`strings.json:261`): in FAWE the argument is an `int`, Piston's converter throws
/// `NumberFormatException` on the oversized text, and
/// `WorldEditExceptionConverter.convert(NumberFormatException)`
/// (`WorldEditExceptionConverter.java:80-91`) is what turns that into a caption.
///
/// Its sibling `worldedit.error.invalid-number.matches` (:260) would be closer — it
/// quotes the offending input — but by the time the value has been parsed to an `f64`
/// this plugin no longer has that input, and re-reading it out of `args` to quote a
/// number the player plainly typed would be quoting the wrong thing.
pub fn integer(args: &ConsumedArgs, name: &str) -> Result<i32, String> {
    let value = number(args, name)?;
    if !value.is_finite() || value < i32::MIN as f64 || value > i32::MAX as f64 {
        return Err(invalid_number_given());
    }
    Ok(value.round() as i32)
}

/// An optional whole number, falling back to `default` when the argument is absent.
pub fn optional_integer(args: &ConsumedArgs, name: &str, default: i32) -> Result<i32, String> {
    match args.get_value(name) {
        Arg::Simple(value) if value.is_empty() => Ok(default),
        _ => integer(args, name),
    }
}

/// A required greedy string, which is how a pattern or mask arrives.
///
/// Greedy rather than single-word because a pattern legitimately contains spaces
/// inside its brackets, as in `//set stone[foo=bar]`.
///
/// An absent argument is worded as in [`word`], not as `fawe.error.command.syntax`.
pub fn text(args: &ConsumedArgs, name: &str) -> Result<String, String> {
    match args.get_value(name) {
        Arg::Simple(value) if !value.is_empty() => Ok(value),
        _ => Err(format!("'{name}' is required")),
    }
}

/// A required block position, from a `block-pos` argument.
///
/// The `_` arm fires when the node was declared with some other argument type, so
/// there is no position to read. The closest caption in `strings.json` is
/// `worldedit.error.invalid-number` (`:261`, "Number expected; string given."): FAWE
/// reads a `@Arg BlockVector3` through `VectorConverter.BLOCK_VECTOR_3_CONVERTER`
/// (`VectorConverter.java:41-45`), which parses three comma-separated components as
/// `int`s, and a component that is not a number surfaces as a `NumberFormatException`
/// that the converter captions (`WorldEditExceptionConverter.java:80-91`). There is no
/// key for "that is not a block position", and nothing here invents one.
pub fn block_position(args: &ConsumedArgs, name: &str) -> Result<[i32; 3], String> {
    match args.get_value(name) {
        Arg::BlockPos(position) => Ok([position.x, position.y, position.z]),
        // See [`word`] for why this is not `fawe.error.command.syntax`.
        Arg::Simple(value) if value.is_empty() => Err(format!("'{name}' is required")),
        _ => Err(invalid_number_given()),
    }
}

/// A boolean flag's value, read from the `bool` argument Piston puts it under.
///
/// `-a` and friends are declared as arguments rather than as a flag type, because
/// `//paste -a` and `//paste` are the same node with and without the value.
pub fn flag(args: &ConsumedArgs, name: &str) -> bool {
    matches!(args.get_value(name), Arg::Bool(true))
}

/// Who sent the command, and where they were.
///
/// A console sender has no position and no world, which is what the selection
/// commands report back rather than guessing at a location.
pub struct Sender {
    /// The sender's name, which is the player's name or `CONSOLE`.
    pub name: String,
    /// The sender's block position, when there is one.
    pub position: Option<[i32; 3]>,
    /// The sender's world, when there is one.
    pub world: Option<pumpkin_plugin_api::World>,
    /// Whether the sender is a player rather than the console or RCON.
    pub is_player: bool,
}

impl Sender {
    /// Reads the sender's identity out of a command sender.
    #[must_use]
    pub fn of(sender: &CommandSender) -> Self {
        match sender.as_player() {
            Some(handle) => {
                let (x, y, z) = handle.get_position();
                Self {
                    name: sender.get_name(),
                    position: Some([x.floor() as i32, y.floor() as i32, z.floor() as i32]),
                    world: Some(handle.get_world()),
                    is_player: true,
                }
            }
            // The console and RCON have no block of their own. A command block does
            // have a world, which is worth keeping even though it has no player.
            None => Self {
                name: sender.get_name(),
                position: None,
                world: sender.world(),
                is_player: false,
            },
        }
    }
}

/// The `single-word` argument type, built once per command tree.
#[must_use]
pub fn single_word() -> &'static ArgumentType {
    static TYPE: std::sync::OnceLock<ArgumentType> = std::sync::OnceLock::new();
    TYPE.get_or_init(|| ArgumentType::String(StringType::SingleWord))
}

/// The `quotable` argument type, for a pattern or mask.
#[must_use]
pub fn quotable() -> &'static ArgumentType {
    static TYPE: std::sync::OnceLock<ArgumentType> = std::sync::OnceLock::new();
    TYPE.get_or_init(|| ArgumentType::String(StringType::Quotable))
}

/// The `block-pos` argument type.
#[must_use]
pub fn block_pos() -> &'static ArgumentType {
    static TYPE: std::sync::OnceLock<ArgumentType> = std::sync::OnceLock::new();
    TYPE.get_or_init(|| ArgumentType::BlockPos)
}

/// The `double` argument type.
#[must_use]
pub fn double() -> &'static ArgumentType {
    static TYPE: std::sync::OnceLock<ArgumentType> = std::sync::OnceLock::new();
    TYPE.get_or_init(|| ArgumentType::Double((None, None)))
}

/// The `integer` argument type.
#[must_use]
pub fn integer_type() -> &'static ArgumentType {
    static TYPE: std::sync::OnceLock<ArgumentType> = std::sync::OnceLock::new();
    TYPE.get_or_init(|| ArgumentType::Integer((None, None)))
}

/// The `bool` argument type, which is how `-a` and friends are declared.
#[must_use]
pub fn boolean() -> &'static ArgumentType {
    static TYPE: std::sync::OnceLock<ArgumentType> = std::sync::OnceLock::new();
    TYPE.get_or_init(|| ArgumentType::Bool)
}

/// Refuses the subcommand unless the sender holds `node`.
///
/// The console and RCON always pass on the host, so this is invisible to a
/// console-driven test run and visible only in game.
///
/// The wording is `fawe.error.no-perm` verbatim (`strings.json:86`), which names the node
/// the player is short of rather than restating that they are short of something: an
/// operator who cannot `//set` needs to be told which node to grant.
pub fn require(sender: &CommandSender, server: &Server, node: &str) -> Result<(), String> {
    if sender.has_permission(server, node) {
        Ok(())
    } else {
        Err(no_permission(node))
    }
}

/// [`require`]'s refusal, as a value so the wording can be checked without a sender.
///
/// `fawe.error.no-perm` (`strings.json:86`). No trailing full stop: the caption does not
/// have one, and this fires on every denied subcommand, so the difference is visible.
pub fn no_permission(node: &str) -> String {
    format!("You are lacking the permission node: {node}")
}

/// Tells the sender what went wrong, and yields a non-zero exit code.
///
/// Every user-facing failure goes through here rather than through
/// `CommandError::CommandFailed`, because the host renders that variant as
/// `Syntax error: Unexpected "<the whole message>"`, which reads as a parser error
/// rather than as an explanation.
pub fn fail(sender: &CommandSender, message: impl Into<String>) -> i32 {
    sender.send_error(pumpkin_plugin_api::text::TextComponent::text(
        &message.into(),
    ));
    1
}

/// Every block name, offered as a completion for a pattern or a mask.
///
/// The host enumerates blocks through get-all-blocks, so this asks for them and keeps
/// the answer on a per-invocation cache: a suggestion handler runs on every keystroke, and
/// re-enumerating a thousand blocks per tab-press is the kind of cost that shows up only
/// as lag.
struct BlockSuggestions;

impl CommandSuggestionHandler for BlockSuggestions {
    fn suggest(
        &self,
        _sender: CommandSender,
        _server: Server,
        request: SuggestionRequest,
    ) -> CommandSuggestions {
        // No world is asked for: a block name needs none, so completion works from the
        // console as well as in game.
        let names = Registry::new().all_block_names();
        let (offset, values) = complete(&names, &request.remaining);
        CommandSuggestions {
            start: request.start + offset as u32,
            length: (request.remaining.len() - offset) as u32,
            values: values
                .into_iter()
                .map(|value| CommandSuggestion {
                    value,
                    tooltip: None,
                })
                .collect(),
        }
    }
}

/// The blocks that complete `remaining`, and where the completion starts inside it.
///
/// Only the segment after the last comma is completed, so `//set stone,di` completes
/// `dirt` and leaves `stone,` alone — which is what a weighted pattern needs, and what
/// the plugin API's own note on `SuggestionRequest` calls out. The offset is returned
/// rather than assumed to be zero because a completion that replaced the whole argument
/// would throw away everything the player already typed before the comma.
///
/// A segment starting with `#` or `!` completes nothing: those begin a tag or a
/// negation, and neither is a block name, so offering blocks there would suggest
/// `!minecraft:stone` as the negation of a block.
fn complete(names: &[String], remaining: &str) -> (usize, Vec<String>) {
    let segment = remaining.rsplit(',').next().unwrap_or("");
    let offset = remaining.len() - segment.len();
    if segment.starts_with('#') || segment.starts_with('!') {
        return (offset, Vec::new());
    }
    let matches = names
        .iter()
        // A name matches either way round, because `//set dirt` is how a player writes
        // it while the registry says `minecraft:dirt`. Matching only the namespaced
        // form would make completion silently useless for every unprefixed block.
        .filter(|name| {
            name.starts_with(segment)
                || name
                    .strip_prefix("minecraft:")
                    .is_some_and(|bare| bare.starts_with(segment))
        })
        .take(MAX_BLOCK_SUGGESTIONS)
        .cloned()
        .collect();
    (offset, matches)
}

/// How many blocks to offer at once.
///
/// A tab-press over ten thousand blocks would produce a completion list taller than the
/// screen and slower than the keystroke. The prefix already narrows this to a handful for
/// any real prefix; the cap is for the ones that do not.
const MAX_BLOCK_SUGGESTIONS: usize = 64;

/// Attaches block completion to a pattern or mask argument.
///
/// Every pattern-taking command shares this, which is why it is here rather than in the
/// command module: one handler, one place to fix it.
pub fn suggest_blocks(node: CommandNode) -> CommandNode {
    node.suggest(BlockSuggestions)
}

/// A small fixed vocabulary, offered as a completion for an argument that takes a closed
/// set of words rather than a block name.
///
/// `StringArgumentType` implements no `list_suggestions` of its own and advertises no
/// client-side parser, so an argument node with no provider offers nothing at all when the
/// player presses tab. That is the whole reason `//schem <tab>` came back empty: the
/// handler knows four actions, and nothing was telling the client so.
pub fn suggest_words(node: CommandNode, words: &'static [&'static str]) -> CommandNode {
    node.suggest(WordSuggestions { words })
}

struct WordSuggestions {
    words: &'static [&'static str],
}

impl CommandSuggestionHandler for WordSuggestions {
    fn suggest(
        &self,
        _sender: CommandSender,
        _server: Server,
        request: SuggestionRequest,
    ) -> CommandSuggestions {
        let names: Vec<String> = self.words.iter().map(|word| (*word).to_string()).collect();

        let (offset, values) = complete(&names, &request.remaining);

        CommandSuggestions {
            start: request.start + offset as u32,
            length: (request.remaining.len() - offset) as u32,
            values: values
                .into_iter()
                .map(|value| CommandSuggestion {
                    value,
                    tooltip: None,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the server's block list, sorted the way `all_block_names` sorts it.
    fn names() -> Vec<String> {
        [
            "minecraft:air",
            "minecraft:dirt",
            "minecraft:dirt_path",
            "minecraft:stone",
        ]
        .iter()
        .map(|n| (*n).to_string())
        .collect()
    }

    /// A bare prefix completes from the start of the argument.
    #[test]
    fn a_bare_prefix_completes_from_the_start() {
        let (offset, matches) = complete(&names(), "dirt");
        assert_eq!(offset, 0);
        assert_eq!(
            matches,
            vec!["minecraft:dirt", "minecraft:dirt_path"],
            "an unprefixed block still completes"
        );
    }

    /// Only the segment after the last comma is completed, so a weighted pattern keeps
    /// what was already typed before it.
    ///
    /// This is the case that makes the offset worth returning rather than assuming zero:
    /// a completion anchored at the start would replace `stone,` as well as `di`.
    #[test]
    fn a_weighted_pattern_completes_only_its_last_segment() {
        let (offset, matches) = complete(&names(), "minecraft:stone,di");
        assert_eq!(offset, 16, "`minecraft:stone,` is sixteen characters");
        assert_eq!(matches, vec!["minecraft:dirt", "minecraft:dirt_path"]);
    }

    /// A pattern that matches nothing completes nothing rather than everything.
    #[test]
    fn a_prefix_that_matches_nothing_completes_nothing() {
        let (_, matches) = complete(&names(), "zzz");
        assert!(matches.is_empty());
    }

    /// A tag or a negation completes nothing, because no block name starts with them.
    #[test]
    fn a_tag_or_negation_completes_nothing() {
        for typed in ["#exi", "!minecraft:"] {
            let (offset, matches) = complete(&names(), typed);
            assert_eq!(offset, 0);
            assert!(
                matches.is_empty(),
                "{typed} should not offer blocks, but offered {}",
                matches.len()
            );
        }
    }

    /// The list is capped, so an empty prefix cannot ask the client to render everything.
    #[test]
    fn an_empty_prefix_is_capped_rather_than_listing_every_block() {
        let many: Vec<String> = (0..10_000).map(|i| format!("minecraft:block{i}")).collect();
        let (_, matches) = complete(&many, "");
        assert_eq!(matches.len(), MAX_BLOCK_SUGGESTIONS);
    }

    /// An empty argument offers everything, which is what a tab on a fresh argument
    /// should do.
    #[test]
    fn an_empty_argument_offers_every_block() {
        let (offset, matches) = complete(&names(), "");
        assert_eq!(offset, 0);
        assert_eq!(matches.len(), 4);
    }

    /// A refused subcommand names the permission node the player is short of.
    ///
    /// `fawe.error.no-perm` (`strings.json:86`), which every denied `//` subcommand
    /// reaches the player with. The previous wording restated that they cannot do
    /// something and parenthesised the node, which is the opposite of what an operator
    /// reading it needs: the node is the only actionable part.
    ///
    /// This fires on every denial, so it is the most-read string in the plugin after the
    /// command captions themselves.
    #[test]
    fn a_refused_subcommand_names_the_missing_permission_node() {
        assert_eq!(
            no_permission("worldedit.edit.set"),
            "You are lacking the permission node: worldedit.edit.set"
        );
        // No trailing full stop: `fawe.error.no-perm` does not have one.
        assert!(!no_permission("worldedit.edit.set").ends_with('.'));
        assert_ne!(
            no_permission("worldedit.edit.set"),
            "You do not have permission to do that (worldedit.edit.set)."
        );
    }
}
