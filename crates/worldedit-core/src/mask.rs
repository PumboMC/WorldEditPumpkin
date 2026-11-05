//! Masks: which blocks an edit is allowed to touch.
//!
//! ## Grammar
//!
//! The tokenizer is `FaweParser.parse`, shared with [`crate::pattern`]. It splits
//! on `,` at bracket depth zero as a union and on `&` or space as an
//! intersection, so `,` binds tighter than `&` and `stone,dirt&grass` means
//! `(stone or dirt) and grass`.
//!
//! There is no difference operator, and `!` is not one either: it is greedy, so
//! `a - b` is not expressible as `!a&b`. That reads as `not (a and b)`.
//!
//! Prefixes split into two kinds, and the difference is not cosmetic:
//!
//! * **Greedy** — `!`, `>`, `<` take the whole rest of the input and return
//!   immediately, discarding every group on both sides. So `!a&b` is
//!   `not (a and b)`, `>stone,dirt` is `below (stone or dirt)`, and `stone,!dirt`
//!   is just `not dirt`. That is the `case '%','$','<','>', '!'` branch in
//!   `RichMaskParser`: it searches the whole input for the sigil and ends in
//!   `return mask`.
//! * **Rich** — `~`, `|`, `{`, `^` apply to their own token only and fall through,
//!   so `~stone,dirt` is `(adjacent to stone) or dirt`.
//!
//! Collapsing the two into one rule breaks both of the first pair.
//!
//! A leading `#` selects one of the named masks below. `##tag` is a block tag and
//! is not supported here: the Pumpkin plugin interface exposes no tag registry. A
//! single `#` followed by anything unrecognised is an error rather than being read
//! as a block name, which is what makes a typo visible instead of matching nothing.

use std::sync::{Arc, Mutex};

use crate::{
    block::{BlockResolver, BlockValue},
    extent::Extent,
    math::BlockPos,
    region::Region,
};

/// A block an edit will accept, matched by name or by exact state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockRef {
    /// Any state of the named block type.
    Type(String),
    /// Exactly this block state.
    State(u16),
}

/// The axis masks latch onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Every tested block shares one X.
    X,
    /// Every tested block shares one Y.
    Y,
    /// Every tested block shares one Z.
    Z,
}

/// A filter over positions.
///
/// `Debug` is written out rather than derived because [`Mask::Region`] holds a
/// type-erased region, and the shapes behind it have no single meaningful
/// rendering.
#[derive(Clone)]
pub enum Mask {
    /// Accept everything.
    AlwaysTrue,
    /// Accept nothing.
    AlwaysFalse,
    /// Accept the listed blocks, in any state or exactly.
    Blocks(Vec<BlockRef>),
    /// Accept air.
    Air,
    /// Accept anything that is not air.
    Existing,
    /// Accept solid blocks.
    Solid,
    /// Accept liquids.
    Liquid,
    /// Accept non-air blocks with air beside them.
    Surface,
    /// Accept the complement.
    Not(Box<Mask>),
    /// Accept positions in every child.
    And(Vec<Mask>),
    /// Accept positions in any child.
    Or(Vec<Mask>),
    /// Accept a position when the block at an offset from it matches.
    Offset {
        /// The mask applied at the offset position.
        mask: Box<Mask>,
        /// Offset X.
        dx: i32,
        /// Offset Y.
        dy: i32,
        /// Offset Z.
        dz: i32,
        /// Positions below this Y reject without consulting `mask`.
        min_y: i32,
        /// Positions above this Y reject without consulting `mask`.
        max_y: i32,
    },
    /// Accept a position when between `min` and `max` of its six neighbours match.
    Adjacent {
        /// The mask each neighbour is tested with.
        mask: Box<Mask>,
        /// Fewest matching neighbours.
        min: u32,
        /// Most matching neighbours.
        max: u32,
    },
    /// The same, over the four horizontal neighbours only.
    Wall {
        /// The mask each neighbour is tested with.
        mask: Box<Mask>,
        /// Fewest matching neighbours.
        min: u32,
        /// Most matching neighbours.
        max: u32,
    },
    /// Accept positions within a radius of the first position tested.
    Radius {
        /// Inner radius.
        min: f64,
        /// Outer radius.
        max: f64,
        /// The position every distance is measured from.
        origin: Latch,
    },
    /// Accept positions inside a selection.
    Region(Arc<dyn Region>),
    /// Accept positions sharing a coordinate with the first one tested.
    Axis {
        /// The coordinate every tested position has to share.
        axis: Axis,
        /// The position that coordinate is read from.
        origin: Latch,
    },
}

/// The first position a mask that measures against one was asked about.
///
/// `RadiusMask.pos` and `XAxisMask.layer` hold this in the mask itself in Java, and
/// that placement is the whole point: an edit asks one mask about thousands of
/// positions, and every answer has to be relative to the same origin. A latch
/// anywhere the caller controls would be reset by every caller that happens to build
/// a fresh one — [`EditJob`](crate::edit::EditJob) builds a [`MaskContext`] per
/// position, because the write it does afterwards needs the extent mutably — so a
/// radius would measure every position against itself and accept nothing.
///
/// Java clears it through `ResettableMask.reset()` once per operation, which is what
/// a fresh parse already gives here: a mask is parsed per command, and cloning one
/// starts a new operation rather than inheriting an origin measured against someone
/// else's positions.
#[derive(Debug, Default)]
pub struct Latch(Mutex<Option<BlockPos>>);

impl Latch {
    /// A latch that has not recorded an origin yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The latched origin, recorded on the first call and reused after that.
    #[must_use]
    pub fn origin(&self, position: BlockPos) -> BlockPos {
        // The lock is taken and released inside this one expression, so it is never
        // held across anything that could panic and poison it.
        let mut latched = self
            .0
            .lock()
            .expect("the mask latch is never held across a panic");
        *latched.get_or_insert(position)
    }
}

impl Clone for Latch {
    /// A copy with nothing latched, which is what `RadiusMask.copy()` and
    /// `XAxisMask.copy()` hand back: the copy is a new operation, not a continuation
    /// of the one that recorded the origin.
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Mask {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The shape behind a region has no useful rendering of its own, so it is
        // named by its bounding box instead.
        let name = match self {
            Self::AlwaysTrue => "AlwaysTrue".to_string(),
            Self::AlwaysFalse => "AlwaysFalse".to_string(),
            Self::Blocks(blocks) => format!("Blocks({blocks:?})"),
            Self::Air => "Air".to_string(),
            Self::Existing => "Existing".to_string(),
            Self::Solid => "Solid".to_string(),
            Self::Liquid => "Liquid".to_string(),
            Self::Surface => "Surface".to_string(),
            Self::Not(mask) => format!("Not({mask:?})"),
            Self::And(masks) => format!("And({masks:?})"),
            Self::Or(masks) => format!("Or({masks:?})"),
            Self::Offset {
                mask,
                dx,
                dy,
                dz,
                min_y,
                max_y,
            } => format!("Offset({mask:?}, {dx}, {dy}, {dz}, {min_y}..={max_y})"),
            Self::Adjacent { mask, min, max } => format!("Adjacent({mask:?}, {min}..={max})"),
            Self::Wall { mask, min, max } => format!("Wall({mask:?}, {min}..={max})"),
            // The latch is not printed: it is a record of what one operation has
            // already been asked, not part of what the mask accepts.
            Self::Radius { min, max, .. } => format!("Radius({min}..={max})"),
            Self::Region(region) => format!(
                "Region({:?} - {:?})",
                region.minimum_point(),
                region.maximum_point()
            ),
            Self::Axis { axis, .. } => format!("Axis({axis:?})"),
        };
        formatter.write_str(&name)
    }
}

/// The six neighbour offsets, in `AdjacentMask`'s order.
const NEIGHBOURS: [(i32, i32, i32); 6] = [
    (1, 0, 0),
    (-1, 0, 0),
    (0, 1, 0),
    (0, -1, 0),
    (0, 0, 1),
    (0, 0, -1),
];

/// The four horizontal neighbour offsets, in `WallMask`'s order.
const HORIZONTAL: [(i32, i32, i32); 4] = [(1, 0, 0), (-1, 0, 0), (0, 0, 1), (0, 0, -1)];

/// Everything a mask needs to answer a question about a position.
///
/// This is the world being edited and the registry that names its blocks, and
/// nothing else. It holds no origin of its own: `RadiusMask` and the axis masks
/// compare every position against one latched position, and that latch lives in the
/// mask ([`Latch`]) rather than here, because one edit asks about thousands of
/// positions and every answer has to be relative to the same origin.
pub struct MaskContext<'a> {
    /// The world being edited.
    pub extent: &'a dyn Extent,
    /// The server's block registry.
    pub resolver: &'a dyn BlockResolver,
}

impl<'a> MaskContext<'a> {
    /// A context bound to one world and registry.
    #[must_use]
    pub fn new(extent: &'a dyn Extent, resolver: &'a dyn BlockResolver) -> Self {
        Self { extent, resolver }
    }
}

impl Mask {
    /// Whether the block at `position` passes this mask.
    #[must_use]
    pub fn test(&self, context: &mut MaskContext<'_>, position: BlockPos) -> bool {
        let current = context.extent.get_block(position);
        match self {
            Self::AlwaysTrue => true,
            Self::AlwaysFalse => false,
            Self::Blocks(blocks) => blocks
                .iter()
                .any(|block| matches_block(block, context.resolver, &current)),
            Self::Air => context.resolver.is_air(current.state),
            Self::Existing => !context.resolver.is_air(current.state),
            Self::Solid => context.resolver.is_solid(current.state),
            Self::Liquid => context.resolver.is_liquid(current.state),
            // `SurfaceMask` extends `AdjacentAnyMask`, whose child is every air
            // type, so a block counts only when an adjacent block fails the child.
            Self::Surface => {
                !context.resolver.is_air(current.state)
                    && NEIGHBOURS.iter().any(|(dx, dy, dz)| {
                        let neighbour = context.extent.get_block(position.offset(*dx, *dy, *dz));
                        context.resolver.is_air(neighbour.state)
                    })
            }
            Self::Not(mask) => !mask.test(context, position),
            Self::And(masks) => masks.iter().all(|mask| mask.test(context, position)),
            Self::Or(masks) => masks.iter().any(|mask| mask.test(context, position)),
            Self::Offset {
                mask,
                dx,
                dy,
                dz,
                min_y,
                max_y,
            } => {
                // Out of range rejects without consulting the child, which is what
                // stops `>stone` reading below the world.
                let target_y = position.y + dy;
                if target_y < *min_y || target_y > *max_y {
                    return false;
                }
                mask.test(context, position.offset(*dx, *dy, *dz))
            }
            Self::Adjacent { mask, min, max } => {
                let matched = NEIGHBOURS
                    .iter()
                    .filter(|(dx, dy, dz)| mask.test(context, position.offset(*dx, *dy, *dz)))
                    .count() as u32;
                matched >= *min && matched <= *max
            }
            Self::Wall { mask, min, max } => {
                let matched = HORIZONTAL
                    .iter()
                    .filter(|(dx, _, dz)| mask.test(context, position.offset(*dx, 0, *dz)))
                    .count() as u32;
                matched >= *min && matched <= *max
            }
            Self::Radius { min, max, origin } => {
                let origin = origin.origin(position);
                let dx = f64::from(position.x - origin.x);
                let dy = f64::from(position.y - origin.y);
                let dz = f64::from(position.z - origin.z);
                let distance = dx * dx + dy * dy + dz * dz;
                distance >= min * min && distance <= max * max
            }
            Self::Region(region) => region.contains(position),
            Self::Axis { axis, origin } => {
                let origin = origin.origin(position);
                match axis {
                    Axis::X => position.x == origin.x,
                    Axis::Y => position.y == origin.y,
                    Axis::Z => position.z == origin.z,
                }
            }
        }
    }
}

/// Whether a block reference matches the block at `position`.
///
/// A bare block name is compared without its namespace on both sides. The host reports
/// namespaced ids (`minecraft:air`) while a mask stores whatever the player typed with
/// the namespace stripped off, so comparing the two verbatim makes every bare-name mask
/// silently match nothing.
fn same_block_name(found: &str, wanted: &str) -> bool {
    fn bare(name: &str) -> &str {
        name.strip_prefix("minecraft:").unwrap_or(name)
    }
    bare(found).eq_ignore_ascii_case(bare(wanted))
}

/// Whether a block reference matches the block at `position`.
fn matches_block(reference: &BlockRef, resolver: &dyn BlockResolver, block: &BlockValue) -> bool {
    match reference {
        BlockRef::State(state) => block.state == *state,
        BlockRef::Type(name) => resolver
            .block_name(block.state)
            .is_some_and(|found| same_block_name(found, name)),
    }
}

/// Why a mask could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaskError {
    /// An entry had no text at all.
    Dangling(char),
    /// A `#` mask that is not one this plugin knows.
    UnknownMask(String),
    /// A block name the server does not have.
    UnknownBlock(String),
    /// A prefix was written with the wrong number of arguments.
    BadArity {
        /// The prefix that was written.
        name: String,
        /// How many arguments it takes.
        expected: usize,
        /// How many were given.
        found: usize,
    },
    /// An argument was not a number.
    BadNumber {
        /// The prefix that was written.
        name: String,
        /// The offending text.
        value: String,
    },
    /// The mask uses a feature this plugin does not implement.
    Unsupported(&'static str),
}

impl std::fmt::Display for MaskError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dangling(character) => {
                write!(
                    formatter,
                    "Invalid dangling character '{character}' in the mask."
                )
            }
            Self::UnknownMask(name) => write!(formatter, "Unknown mask: '{name}'"),
            Self::UnknownBlock(name) => write!(formatter, "No block found for '{name}'."),
            Self::BadArity {
                name,
                expected,
                found,
            } => write!(
                formatter,
                "'{name}' takes {expected} argument(s) but {found} were given."
            ),
            Self::BadNumber { name, value } => {
                write!(formatter, "'{value}' is not a number '{name}' can use.")
            }
            Self::Unsupported(what) => write!(formatter, "{what} is not supported."),
        }
    }
}

impl std::error::Error for MaskError {}

/// Reads a mask against a server's block registry.
///
/// `bounds` is the world's inclusive Y range, which is what the `>` and `<`
/// prefixes are clamped to so they cannot reach past the floor or ceiling.
pub fn parse(
    input: &str,
    resolver: &dyn BlockResolver,
    bounds: (i32, i32),
) -> Result<Mask, MaskError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Mask::AlwaysTrue);
    }

    // The greedy prefixes are handled before anything else, because they consume
    // the whole remainder. That ordering is what `RichMaskParser`'s `return` gives.
    if let Some(rest) = trimmed.strip_prefix('!') {
        return Ok(Mask::Not(Box::new(parse(rest, resolver, bounds)?)));
    }
    if let Some(rest) = trimmed.strip_prefix('>') {
        return Ok(offset(parse(rest, resolver, bounds)?, 0, -1, 0, bounds));
    }
    if let Some(rest) = trimmed.strip_prefix('<') {
        return Ok(offset(parse(rest, resolver, bounds)?, 0, 1, 0, bounds));
    }

    parse_groups(trimmed, resolver, bounds)
}

/// Wraps a mask so it is tested at an offset, clamped to the world.
fn offset(mask: Mask, dx: i32, dy: i32, dz: i32, bounds: (i32, i32)) -> Mask {
    Mask::Offset {
        mask: Box::new(mask),
        dx,
        dy,
        dz,
        min_y: bounds.0,
        max_y: bounds.1,
    }
}

/// One token from the mask tokenizer.
/// The greedy prefix a token carries, if any.
///
/// These three differ from `~`, `|`, `{` and `^` by covering the rest of the
/// input rather than their own token.
fn greedy_sigil(text: &str) -> Option<char> {
    match text.chars().next()? {
        sigil @ ('!' | '>' | '<') => Some(sigil),
        _ => None,
    }
}

/// One token from the mask tokenizer.
struct Token {
    /// The token with its trailing argument groups removed, leaving the name and
    /// any sigil.
    text: String,
    /// The token as written, arguments included.
    ///
    /// `~[stone][1][2]` has the text `~` and the full `~[stone][1][2]`: the name
    /// selects the mask and the bracketed groups are its arguments. Keeping only
    /// the peeled text would throw the arguments away.
    full: String,
    /// Whether the separator before this token was `&` or a space.
    is_and: bool,
    /// Where this token starts in the string that was handed to the tokenizer, so a
    /// greedy prefix can reach the text after it rather than just this token.
    ///
    /// `None` for the tokens `child_token` builds, which have no position in the
    /// original input.
    start: Option<usize>,
}

/// Reads the top level: intersections of unions of entries.
fn parse_groups(
    input: &str,
    resolver: &dyn BlockResolver,
    bounds: (i32, i32),
) -> Result<Mask, MaskError> {
    let tokens = tokenize(input)?;
    let mut groups: Vec<Vec<Mask>> = vec![Vec::new()];
    for token in tokens {
        // A greedy prefix partway through the input is not a term of its own.
        // `RichMaskParser` searches the *whole* input for the sigil and returns
        // the mask it builds straight away, so whatever came before is dropped and
        // the prefix covers everything after it. `stone,!dirt` is `not dirt`, not
        // `stone or not dirt`. Only `parse` above gets here for a leading sigil,
        // so anything still carrying one is mid-string.
        if let Some(start) = token.start {
            if let Some(sigil) = greedy_sigil(&token.text) {
                let after = &input[start + 1..];
                return match sigil {
                    '!' => Ok(Mask::Not(Box::new(parse(after, resolver, bounds)?))),
                    '>' => Ok(offset(parse(after, resolver, bounds)?, 0, -1, 0, bounds)),
                    _ => Ok(offset(parse(after, resolver, bounds)?, 0, 1, 0, bounds)),
                };
            }
        }
        // A leading `&` or space starts a new intersection group; a leading `,`
        // stays inside the current one.
        if token.is_and && groups.last().is_some_and(|group| !group.is_empty()) {
            groups.push(Vec::new());
        }
        groups
            .last_mut()
            .expect("there is always at least one group")
            .push(parse_entry(&token, resolver, bounds)?);
    }

    let unions: Vec<Mask> = groups
        .into_iter()
        .map(|group| match group.len() {
            1 => group.into_iter().next().expect("the group has one entry"),
            0 => Mask::AlwaysFalse,
            _ => Mask::Or(group),
        })
        .collect();

    Ok(match unions.len() {
        1 => unions.into_iter().next().expect("there is one union"),
        0 => Mask::AlwaysFalse,
        _ => Mask::And(unions),
    })
}

/// Splits a mask into tokens, peeling trailing `[...]` arguments off each.
///
/// `=` puts the tokenizer into expression mode, where separators stop mattering,
/// because an expression contains commas. That is what `=` is for; the expression
/// engine itself is not implemented and is reported as unsupported.
fn tokenize(input: &str) -> Result<Vec<Token>, MaskError> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut is_and = false;
    let mut in_expression = false;

    for (index, character) in input.char_indices() {
        match character {
            '[' => {
                depth += 1;
                // A bracketed group ends the expression, so the separator after it
                // splits again. This is the `expression = false` at the bottom of
                // `FaweParser.parse`'s `[` branch, and it is what lets
                // `=a[b],c` be one term and a comma again.
                in_expression = false;
            }
            ']' => depth = depth.saturating_sub(1),
            '=' if depth == 0 => in_expression = true,
            ',' | '&' | ' ' if depth == 0 && !in_expression => {
                let raw = &input[start..index];
                let part = raw.trim();
                if part.is_empty() {
                    return Err(MaskError::Dangling(character));
                }
                let leading = raw.len() - raw.trim_start().len();
                tokens.push(peel(part, is_and, start + leading));
                is_and = character != ',';
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    let raw = &input[start..];
    let last = raw.trim();
    if !last.is_empty() {
        let leading = raw.len() - raw.trim_start().len();
        tokens.push(peel(last, is_and, start + leading));
    }
    Ok(tokens)
}

/// Splits trailing `[...]` groups off a token, keeping both forms.
fn peel(token: &str, is_and: bool, start: usize) -> Token {
    let full = token.trim().to_string();
    let mut text = full.clone();
    while let Some(close) = text.rfind(']') {
        // A bracket with nothing in front of it is the whole token, which is a
        // block list rather than an argument group.
        let Some(open) = text[..close].rfind('[').filter(|open| *open > 0) else {
            break;
        };
        text.truncate(open);
    }
    Token {
        text: text.trim().to_string(),
        full,
        is_and,
        start: Some(start),
    }
}

/// Reads one token.
fn parse_entry(
    token: &Token,
    resolver: &dyn BlockResolver,
    bounds: (i32, i32),
) -> Result<Mask, MaskError> {
    let name = token.text.as_str();

    // The greedy prefixes apply to this token and everything after it, so a token
    // reached through the top-level split has to be handed back to `parse` rather
    // than read on its own. Otherwise `>stone&dirt` would offset only `stone`.
    if name.starts_with('!') {
        return Ok(Mask::Not(Box::new(parse(
            &token.full[1..],
            resolver,
            bounds,
        )?)));
    }
    if name.starts_with('>') {
        return Ok(offset(
            parse(&token.full[1..], resolver, bounds)?,
            0,
            -1,
            0,
            bounds,
        ));
    }
    if name.starts_with('<') {
        return Ok(offset(
            parse(&token.full[1..], resolver, bounds)?,
            0,
            1,
            0,
            bounds,
        ));
    }

    // The rich prefixes apply to this token only and then fall through, leaving the
    // rest of the input as its own groups.
    if name.starts_with('~') {
        let arguments = token_arguments(token, '~')?;
        let (min, max) = adjacency_range(&arguments, "~")?;
        return Ok(Mask::Adjacent {
            mask: Box::new(parse_entry(
                &child_token(token, &arguments),
                resolver,
                bounds,
            )?),
            min,
            max: max.unwrap_or(min),
        });
    }
    if name.starts_with('|') {
        let arguments = token_arguments(token, '|')?;
        let (min, max) = adjacency_range(&arguments, "|")?;
        return Ok(Mask::Wall {
            mask: Box::new(parse_entry(
                &child_token(token, &arguments),
                resolver,
                bounds,
            )?),
            min,
            max: max.unwrap_or(min),
        });
    }
    if name.starts_with('{') {
        let arguments = token_arguments(token, '{')?;
        if arguments.len() != 2 {
            return Err(MaskError::BadArity {
                name: "{".to_string(),
                expected: 2,
                found: arguments.len(),
            });
        }
        return Ok(Mask::Radius {
            min: number("{", &arguments[0])?,
            max: number("{", &arguments[1])?,
            origin: Latch::new(),
        });
    }
    // `^` selects the same block lists as the bare form; it exists so that a state
    // list is explicit, and because FAWE's strict form used it.
    if name.starts_with('^') {
        return parse_body(&token.full[1..], resolver);
    }
    parse_body(&token.full, resolver)
}

/// The bracketed arguments of a token, taken from after its sigil.
///
/// `~[stone][1][2]` has three arguments, which is what makes it an adjacency mask
/// with a range rather than three separate tokens.
fn token_arguments(token: &Token, sigil: char) -> Result<Vec<String>, MaskError> {
    let rest = token
        .full
        .strip_prefix(sigil)
        .map_or_else(String::new, ToString::to_string);
    arguments_of(&rest, &sigil.to_string())
}

/// The token the sigil applies to: its own text, or its first argument when the
/// whole body was bracketed.
fn child_token(token: &Token, arguments: &[String]) -> Token {
    let peeled = token.text[1..].trim().to_string();
    if !peeled.is_empty() {
        return Token {
            text: peeled.clone(),
            full: peeled,
            is_and: token.is_and,
            start: None,
        };
    }
    // `[stone]` after a sigil: the argument is the body.
    let body = arguments.first().cloned().unwrap_or_default();
    Token {
        text: body.clone(),
        full: body,
        is_and: token.is_and,
        start: None,
    }
}

/// Reads the `[min][max]` range of the adjacency prefixes.
fn adjacency_range(arguments: &[String], name: &str) -> Result<(u32, Option<u32>), MaskError> {
    // An adjacency mask's first argument is the child, so the range starts at the
    // second. With no arguments at all the range is the default `1..=1`.
    let min = match arguments.get(1) {
        Some(value) => number(name, value)? as u32,
        None => 1,
    };
    let max = match arguments.get(2) {
        Some(value) => Some(number(name, value)? as u32),
        None => None,
    };
    Ok((min, max))
}

/// The bracketed arguments attached to a token, or none when there are none.
fn arguments_of(token: &str, name: &str) -> Result<Vec<String>, MaskError> {
    let Some(open) = token.find('[') else {
        return Ok(Vec::new());
    };
    let mut arguments = Vec::new();
    let mut rest = &token[open..];
    while !rest.is_empty() {
        let Some(after_open) = rest.strip_prefix('[') else {
            break;
        };
        match after_open.find(']') {
            None => {
                return Err(MaskError::BadArity {
                    name: name.to_string(),
                    expected: 1,
                    found: arguments.len(),
                });
            }
            Some(close) => {
                arguments.push(after_open[..close].to_string());
                rest = &after_open[close + 1..];
            }
        }
    }
    Ok(arguments)
}

fn number(name: &str, value: &str) -> Result<f64, MaskError> {
    value
        .trim()
        .parse::<f64>()
        .map_err(|_| MaskError::BadNumber {
            name: name.to_string(),
            value: value.to_string(),
        })
}

/// Reads a token with no prefix left on it: a named mask or a block list.
fn parse_body(token: &str, resolver: &dyn BlockResolver) -> Result<Mask, MaskError> {
    let token = token.trim();
    if token.is_empty() {
        return Ok(Mask::AlwaysTrue);
    }
    if token.starts_with('=') {
        // The tokenizer already kept the separators inside this token, so
        // everything after `=` is the expression.
        return Err(MaskError::Unsupported("an `=` expression mask"));
    }
    if let Some(name) = token.strip_prefix('#') {
        return named_mask(name);
    }
    block_mask(token, resolver)
}

/// The masks a leading `#` selects.
fn named_mask(name: &str) -> Result<Mask, MaskError> {
    match name {
        "air" => Ok(Mask::Air),
        "existing" => Ok(Mask::Existing),
        "solid" => Ok(Mask::Solid),
        "liquid" => Ok(Mask::Liquid),
        "surface" => Ok(Mask::Surface),
        "true" => Ok(Mask::AlwaysTrue),
        "false" => Ok(Mask::AlwaysFalse),
        "xaxis" => Ok(Mask::Axis {
            axis: Axis::X,
            origin: Latch::new(),
        }),
        "yaxis" => Ok(Mask::Axis {
            axis: Axis::Y,
            origin: Latch::new(),
        }),
        "zaxis" => Ok(Mask::Axis {
            axis: Axis::Z,
            origin: Latch::new(),
        }),
        "region" | "selection" | "sel" | "dregion" | "dselection" | "dsel" => {
            Err(MaskError::Unsupported(
                "a `#region` selection mask, which needs the session's selection",
            ))
        }
        other if other.starts_with('#') => Err(MaskError::Unsupported("a `##` block tag mask")),
        other => Err(MaskError::UnknownMask(other.to_string())),
    }
}

/// Reads a list of block names or states.
fn block_mask(token: &str, resolver: &dyn BlockResolver) -> Result<Mask, MaskError> {
    let text = strip_brackets(token);
    if text.trim().is_empty() {
        return Ok(Mask::AlwaysTrue);
    }
    let blocks: Vec<BlockRef> = text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| parse_block_ref(part, resolver))
        .collect::<Result<_, _>>()?;
    Ok(match blocks.len() {
        0 => Mask::AlwaysTrue,
        1 => Mask::Blocks(blocks),
        _ => Mask::Or(
            blocks
                .into_iter()
                .map(|block| Mask::Blocks(vec![block]))
                .collect(),
        ),
    })
}

/// Parses one `stone` or `oak_log[axis=x]`.
fn parse_block_ref(part: &str, resolver: &dyn BlockResolver) -> Result<BlockRef, MaskError> {
    let (name, properties) = match part.find('[') {
        None => (part, ""),
        Some(index) => {
            let rest = &part[index + 1..];
            match rest.rfind(']') {
                // Auto-close an unterminated `[`, the way the tokenizer does.
                None => (&part[..index], rest),
                Some(end) => (&part[..index], &rest[..end]),
            }
        }
    };
    let name = name.trim();
    if name.is_empty() {
        return Err(MaskError::UnknownBlock(part.to_string()));
    }

    let parsed = split_properties(properties);
    match resolver.resolve(name, &parsed) {
        // Any state of the type: matched by name, because a bare block name is
        // about the type rather than one state of it.
        Some(_) if parsed.is_empty() => Ok(BlockRef::Type(
            name.strip_prefix("minecraft:").unwrap_or(name).to_string(),
        )),
        Some(state) => Ok(BlockRef::State(state)),
        None => Err(MaskError::UnknownBlock(part.to_string())),
    }
}

/// Turns `axis=x,facing=north` into pairs.
fn split_properties(properties: &str) -> Vec<(String, String)> {
    let properties = properties.trim();
    if properties.is_empty() {
        return Vec::new();
    }
    properties
        .split(',')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (key.trim().to_string(), value.trim().to_string()),
            None => (pair.to_string(), String::new()),
        })
        .collect()
}

/// Removes a wrapping `[...]` from a token.
fn strip_brackets(token: &str) -> &str {
    match token.strip_prefix('[') {
        None => token,
        Some(inner) => inner.strip_suffix(']').unwrap_or(inner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{block::BlockStateInfo, region::CuboidRegion};

    /// State `1` is stone and solid, `5` is air, `6` is water.
    const AIR: u16 = 5;

    struct World {
        region: CuboidRegion,
        states: Vec<u16>,
    }

    impl World {
        fn new(region: CuboidRegion, states: Vec<u16>) -> Self {
            Self { region, states }
        }

        fn filled(region: CuboidRegion, state: u16) -> Self {
            Self {
                region,
                states: vec![state; region.dimensions().volume()],
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
            if !self.region.contains(position) {
                return BlockValue::new(AIR);
            }
            let index = self.index(position);
            BlockValue::new(self.states.get(index).copied().unwrap_or(AIR))
        }

        fn set_block(&mut self, position: BlockPos, block: &BlockValue) {
            let index = self.index(position);
            if let Some(slot) = self.states.get_mut(index) {
                *slot = block.state;
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

    struct TestBlocks;

    impl BlockResolver for TestBlocks {
        fn resolve(&self, name: &str, properties: &[(String, String)]) -> Option<u16> {
            let base = match name {
                "stone" | "minecraft:stone" => 1,
                "dirt" => 2,
                "oak_log" => 3,
                "air" | "minecraft:air" => AIR,
                "water" => 6,
                _ => return None,
            };
            properties.is_empty().then_some(base)
        }

        fn info(&self, state: u16) -> Option<BlockStateInfo> {
            Some(BlockStateInfo {
                name: format!("minecraft:b{state}"),
                properties: Vec::new(),
            })
        }

        fn is_air(&self, state: u16) -> bool {
            state == AIR
        }

        fn is_solid(&self, state: u16) -> bool {
            (1..=4).contains(&state)
        }

        fn is_liquid(&self, state: u16) -> bool {
            state == 6
        }

        fn block_name(&self, state: u16) -> Option<&str> {
            Some(match state {
                1 => "stone",
                2 => "dirt",
                3 => "oak_log",
                AIR => "air",
                6 => "water",
                _ => "other",
            })
        }
    }

    /// The world bounds every mask in these tests is read against.
    const BOUNDS: (i32, i32) = (-64, 319);

    fn ok(input: &str) -> Mask {
        parse(input, &TestBlocks, BOUNDS).unwrap_or_else(|error| panic!("{input:?}: {error}"))
    }

    fn accepts(mask: &Mask, world: &World, position: BlockPos) -> bool {
        let mut context = MaskContext::new(world, &TestBlocks);
        mask.test(&mut context, position)
    }

    fn one_block(state: u16) -> World {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
        World::filled(region, state)
    }

    #[test]
    fn an_empty_mask_accepts_everything() {
        assert!(matches!(ok(""), Mask::AlwaysTrue));
    }

    #[test]
    fn a_block_name_matches_every_state_of_that_type() {
        let world = one_block(1);
        assert!(accepts(&ok("stone"), &world, BlockPos::new(0, 0, 0)));
        assert!(!accepts(&ok("dirt"), &world, BlockPos::new(0, 0, 0)));
    }

    /// The registry reports `minecraft:stone`, so a mask written either way has to
    /// end up matching the same blocks.
    #[test]
    fn a_namespaced_name_matches_the_same_block_as_the_bare_one() {
        let stone = one_block(1);
        let dirt = one_block(2);
        for mask in ["minecraft:stone", "stone"] {
            assert!(accepts(&ok(mask), &stone, BlockPos::new(0, 0, 0)), "{mask}");
            assert!(!accepts(&ok(mask), &dirt, BlockPos::new(0, 0, 0)), "{mask}");
        }
    }

    /// A resolver that reports namespaced block ids, the way the Pumpkin host does.
    ///
    /// [`TestBlocks`] hands back bare names, which is what let the namespace mismatch
    /// survive: a bare mask name and a bare reported name compare equal by accident, so
    /// the unit tests agreed with each other while the game silently matched nothing.
    struct NamespacedBlocks;

    impl BlockResolver for NamespacedBlocks {
        fn resolve(&self, name: &str, properties: &[(String, String)]) -> Option<u16> {
            TestBlocks.resolve(name, properties)
        }

        fn info(&self, state: u16) -> Option<BlockStateInfo> {
            Some(BlockStateInfo {
                name: format!("minecraft:b{state}"),
                properties: Vec::new(),
            })
        }

        fn is_air(&self, state: u16) -> bool {
            state == AIR
        }

        fn is_solid(&self, state: u16) -> bool {
            (1..=4).contains(&state)
        }

        fn is_liquid(&self, state: u16) -> bool {
            state == 6
        }

        fn block_name(&self, state: u16) -> Option<&str> {
            Some(match state {
                1 => "minecraft:stone",
                2 => "minecraft:dirt",
                3 => "minecraft:oak_log",
                AIR => "minecraft:air",
                6 => "minecraft:water",
                _ => "minecraft:other",
            })
        }
    }

    /// `//replacenear 10 air glass` used to change nothing, and report nothing wrong: the
    /// mask kept the bare `air` the player typed while the host answered with
    /// `minecraft:air`, and the two were compared verbatim.
    #[test]
    fn a_bare_name_matches_a_host_that_reports_namespaced_ids() {
        let air = one_block(AIR);
        let stone = one_block(1);
        for name in ["air", "minecraft:air"] {
            let mask = parse(name, &NamespacedBlocks, BOUNDS)
                .unwrap_or_else(|error| panic!("{name:?}: {error}"));
            let mut context = MaskContext::new(&air, &NamespacedBlocks);
            assert!(mask.test(&mut context, BlockPos::new(0, 0, 0)), "{name}");
        }
        for name in ["stone", "minecraft:stone"] {
            let mask = parse(name, &NamespacedBlocks, BOUNDS)
                .unwrap_or_else(|error| panic!("{name:?}: {error}"));
            let mut context = MaskContext::new(&stone, &NamespacedBlocks);
            assert!(mask.test(&mut context, BlockPos::new(0, 0, 0)), "{name}");
        }
    }

    /// `,` has to bind tighter than `&`, or `stone,dirt&oak_log` becomes the wrong
    /// shape entirely.
    #[test]
    fn a_comma_binds_tighter_than_an_ampersand() {
        let mask = ok("stone,dirt&oak_log");
        let Mask::And(parts) = &mask else {
            panic!("expected an intersection, got {mask:?}");
        };
        assert_eq!(parts.len(), 2, "expected two intersection groups");
        assert!(
            matches!(&parts[0], Mask::Or(arms) if arms.len() == 2),
            "the comma group should be a union"
        );
    }

    #[test]
    fn a_dangling_separator_is_refused() {
        assert!(matches!(
            parse("stone,,dirt", &TestBlocks, BOUNDS),
            Err(MaskError::Dangling(','))
        ));
        assert!(matches!(
            parse("stone, dirt", &TestBlocks, BOUNDS),
            Err(MaskError::Dangling(' '))
        ));
    }

    /// The greedy prefixes take the rest of the input, so `!stone,dirt` has to be
    /// the negation of the union rather than a union of a negation.
    #[test]
    fn a_greedy_prefix_covers_everything_after_it() {
        let stone = one_block(1);
        let air = one_block(AIR);
        // Stone is in `stone,dirt`, so the union accepts it and its negation does not.
        assert!(accepts(&ok("stone,dirt"), &stone, BlockPos::new(0, 0, 0)));
        assert!(!accepts(&ok("!stone,dirt"), &stone, BlockPos::new(0, 0, 0)));
        assert!(accepts(&ok("!stone,dirt"), &air, BlockPos::new(0, 0, 0)));
    }

    /// `>stone&dirt` has to offset the whole intersection, not just the first token.
    #[test]
    fn a_greedy_prefix_inside_a_group_still_takes_the_rest() {
        // A three-high column of dirt with air above it.
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 2, 0));
        let world = World::new(region, vec![2, AIR, AIR]);

        // `>` is greedy, so this is `below(dirt and dirt)`, which is just
        // `below dirt`. Had it bound only to its own token it would also have
        // required the block itself to be dirt, and air would reject.
        assert!(accepts(&ok(">dirt&dirt"), &world, BlockPos::new(0, 1, 0)));
        assert!(!accepts(&ok(">dirt&dirt"), &world, BlockPos::new(0, 2, 0)));
    }

    /// A greedy prefix partway through the input still swallows the rest of it and
    /// drops what came before, because `RichMaskParser` searches the whole input
    /// for the sigil and returns straight away.
    #[test]
    fn a_greedy_prefix_partway_through_drops_what_came_before() {
        let dirt = one_block(2);
        // `stone,!dirt` is `not dirt`: stone never gets a say, because the mask is
        // built from the sigil onwards and returned before `stone` is combined.
        let mask = ok("stone,!dirt");
        assert!(
            matches!(mask, Mask::Not(_)),
            "expected a negation, got {mask:?}"
        );
        assert!(!accepts(&mask, &dirt, BlockPos::new(0, 0, 0)));
        assert!(accepts(&mask, &one_block(1), BlockPos::new(0, 0, 0)));
    }

    /// The same rule for an offset prefix: `stone,>dirt` reads the neighbour below,
    /// so a stone block with dirt beneath it is accepted even though the block
    /// itself is not dirt.
    #[test]
    fn an_offset_prefix_partway_through_drops_what_came_before() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 1, 0));
        let world = World::new(region, vec![2, 1]);
        let mask = ok("stone,>dirt");
        assert!(
            matches!(&mask, Mask::Offset { dy: -1, .. }),
            "expected an offset mask, got {mask:?}"
        );
        assert!(accepts(&mask, &world, BlockPos::new(0, 1, 0)));
    }

    /// A greedy prefix inside its own brackets has no position in the original
    /// input, so it stays a term of its own group. `~[!dirt]` is adjacency of
    /// something that is not dirt, not the negation of adjacency.
    #[test]
    fn a_bracketed_greedy_prefix_stays_inside_its_argument() {
        let mask = ok("~[!dirt]");
        match &mask {
            Mask::Adjacent { mask, .. } => assert!(
                matches!(&**mask, Mask::Not(_)),
                "expected the child to be negated, got {mask:?}"
            ),
            other => panic!("expected an adjacency mask, got {other:?}"),
        }
    }

    /// The rich prefixes apply to one token only, so `~stone,dirt` keeps `dirt` as
    /// a separate union arm.
    #[test]
    fn a_rich_prefix_applies_to_its_own_token_only() {
        let mask = ok("~stone,dirt");
        match &mask {
            Mask::Or(parts) => assert_eq!(parts.len(), 2, "expected a union of two"),
            other => panic!("expected a union, got {other:?}"),
        }
    }

    #[test]
    fn the_named_masks_answer_what_they_say() {
        let solid = one_block(1);
        let air = one_block(AIR);
        let water = one_block(6);

        assert!(accepts(&ok("#solid"), &solid, BlockPos::new(0, 0, 0)));
        assert!(!accepts(&ok("#air"), &solid, BlockPos::new(0, 0, 0)));
        assert!(accepts(&ok("#air"), &air, BlockPos::new(0, 0, 0)));
        assert!(!accepts(&ok("#existing"), &air, BlockPos::new(0, 0, 0)));
        assert!(accepts(&ok("#existing"), &solid, BlockPos::new(0, 0, 0)));
        assert!(accepts(&ok("#liquid"), &water, BlockPos::new(0, 0, 0)));
        assert!(!accepts(&ok("#liquid"), &solid, BlockPos::new(0, 0, 0)));
        assert!(accepts(&ok("#true"), &air, BlockPos::new(0, 0, 0)));
        assert!(!accepts(&ok("#false"), &air, BlockPos::new(0, 0, 0)));
    }

    /// `>stone` looks one block down, so air above a stone floor passes.
    #[test]
    fn the_offset_prefix_reads_the_neighbour_below() {
        // Two blocks stacked: index 0 is the lower one, index 1 the upper.
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 1, 0));
        let mut world = World::new(region, vec![1, AIR]);

        // Air above a stone floor passes `>stone`, because the block below it is
        // stone. The floor itself does not, because below it is outside the world.
        assert!(accepts(&ok(">stone"), &world, BlockPos::new(0, 1, 0)));
        assert!(!accepts(&ok(">stone"), &world, BlockPos::new(0, 0, 0)));
        // `<stone` reads the other way, so nothing here has stone above it.
        assert!(!accepts(&ok("<stone"), &world, BlockPos::new(0, 0, 0)));
        assert!(!accepts(&ok("<stone"), &world, BlockPos::new(0, 1, 0)));

        // With stone on top instead, the answers invert.
        world.states = vec![AIR, 1];
        assert!(!accepts(&ok(">stone"), &world, BlockPos::new(0, 1, 0)));
        assert!(accepts(&ok("<stone"), &world, BlockPos::new(0, 0, 0)));
    }

    /// The offset mask is clamped to the world, so reading below the floor is a
    /// rejection rather than a read out of bounds.
    #[test]
    fn the_offset_prefix_will_not_read_outside_the_world() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(0, 0, 0));
        let world = World::new(region, vec![1]);
        let mask = offset(ok("stone"), 0, -1, 0, BOUNDS);
        assert!(!accepts(&mask, &world, BlockPos::new(0, 0, 0)));
    }

    #[test]
    fn an_unknown_named_mask_is_reported_rather_than_matching_nothing() {
        assert!(matches!(
            parse("#flurb", &TestBlocks, BOUNDS),
            Err(MaskError::UnknownMask(name)) if name == "flurb"
        ));
        assert!(matches!(
            parse("##logs", &TestBlocks, BOUNDS),
            Err(MaskError::Unsupported("a `##` block tag mask"))
        ));
    }

    #[test]
    fn an_unknown_block_is_reported() {
        assert!(matches!(
            parse("flurb", &TestBlocks, BOUNDS),
            Err(MaskError::UnknownBlock(name)) if name == "flurb"
        ));
    }

    #[test]
    fn an_expression_mask_is_reported_as_unsupported_rather_than_misread() {
        assert!(matches!(
            parse("=x>10", &TestBlocks, BOUNDS),
            Err(MaskError::Unsupported("an `=` expression mask"))
        ));
    }

    /// The adjacency mask counts the six neighbours, so the air beside a lone
    /// stone has to satisfy `~stone[1]`.
    #[test]
    fn the_adjacency_prefix_counts_the_six_neighbours() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 2, 2));
        let mut world = World::new(region, vec![AIR; 27]);
        world.set_block(BlockPos::new(1, 1, 1), &BlockValue::new(1));

        let mask = ok("~stone[1]");
        assert!(accepts(&mask, &world, BlockPos::new(0, 1, 1)));
        assert!(!accepts(&mask, &world, BlockPos::new(1, 1, 1)));
    }

    /// A radius is measured from the first position tested, so the mask latches
    /// that position rather than using the world origin.
    #[test]
    fn the_radius_prefix_measures_from_the_first_position_tested() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(4, 0, 0));
        let world = World::filled(region, 1);
        // A shell one to two blocks from the origin: the origin itself is excluded
        // by the inner radius.
        let mask = ok("{[1][2]}");
        let mut context = MaskContext::new(&world, &TestBlocks);
        assert!(!mask.test(&mut context, BlockPos::new(0, 0, 0)));
        assert!(mask.test(&mut context, BlockPos::new(1, 0, 0)));
        assert!(mask.test(&mut context, BlockPos::new(2, 0, 0)));
        assert!(!mask.test(&mut context, BlockPos::new(3, 0, 0)));
    }

    #[test]
    fn a_radius_needs_exactly_two_numbers() {
        assert!(matches!(
            parse("{[2][4][6]}", &TestBlocks, BOUNDS),
            Err(MaskError::BadArity {
                expected: 2,
                found: 3,
                ..
            })
        ));
    }

    /// A surface is a non-air block with air beside it, so a buried block does not
    /// match but a block at the edge does.
    #[test]
    fn the_surface_mask_wants_air_beside_a_solid_block() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 2, 2));
        let mut world = World::new(region, vec![AIR; 27]);
        world.set_block(BlockPos::new(1, 1, 1), &BlockValue::new(1));

        let mask = ok("#surface");
        assert!(!accepts(&mask, &world, BlockPos::new(0, 1, 1)));
        world.set_block(BlockPos::new(1, 0, 1), &BlockValue::new(1));
        assert!(accepts(&mask, &world, BlockPos::new(1, 0, 1)));
    }

    #[test]
    fn a_bracketed_block_list_is_read_as_a_union() {
        let stone = one_block(1);
        assert!(accepts(&ok("[stone,dirt]"), &stone, BlockPos::new(0, 0, 0)));
    }
}
