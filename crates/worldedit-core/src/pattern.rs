//! Block patterns: what `//set`, `//replace` and the shape commands fill with.
//!
//! ## Grammar
//!
//! The input is split into a list on `,`, `&` and space at bracket depth zero,
//! which is `FaweParser.parse`. For patterns all three separators mean the same
//! thing — the entries become a weighted random choice rather than a boolean
//! combination — because `RichPatternParser` reads the `and` flag the lexer
//! records and then ignores it. That is why `stone,dirt` picks one of the two per
//! block.
//!
//! An empty segment between two separators is an error rather than a skipped
//! entry, so `stone, dirt` is rejected: a list with a space in it is a typo, not
//! two entries.
//!
//! ```text
//! list  := entry (',' entry)*
//! entry := [number '%'] body
//! body  := block | '[' body (',' body)* ']'
//! block := name ['[' property (',' property)* ']']
//! ```
//!
//! A weight is only read when the whole entry matches `[0-9]+(\.[0-9]*)?%.*`,
//! which is `percentPatternRegex`. Weights are relative rather than
//! probabilities: `RandomPattern.add` normalises by the sum, so `50%stone,50%dirt`
//! is half each and `2%stone,1%oak_log` is two thirds stone. Repeating an entry
//! adds its weights together, which is why `50%stone,50%stone,dirt` is very nearly
//! all stone.

use crate::{
    block::{BlockResolver, BlockValue},
    math::BlockPos,
};

/// What to place at each position.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// One block, always this one.
    Single(BlockValue),
    /// A weighted choice, resolved once per position.
    ///
    /// The choice is keyed on the position rather than drawn from a running
    /// generator, so re-running an edit over the same blocks gives the same
    /// scatter. That is `RandomCollection.next(x, y, z)`.
    Random(Vec<(Pattern, f64)>),
}

impl Pattern {
    /// The block this pattern places at `position`.
    #[must_use]
    pub fn next(&self, position: BlockPos) -> BlockValue {
        match self {
            Self::Single(block) => block.clone(),
            Self::Random(entries) => {
                let total: f64 = entries.iter().map(|(_, chance)| *chance).sum();
                if total <= 0.0 {
                    // Every entry had zero chance. `parse` refuses to build this,
                    // so reaching here means the engine was handed one by hand.
                    return BlockValue::new(0);
                }
                let mut roll = position_roll(position) * total;
                for (pattern, chance) in entries {
                    roll -= *chance;
                    if roll <= 0.0 {
                        return pattern.next(position);
                    }
                }
                entries
                    .last()
                    .map_or_else(|| BlockValue::new(0), |(pattern, _)| pattern.next(position))
            }
        }
    }
}

/// A uniform value in `[0, 1)` derived from a position.
///
/// The scatter differs from Java's `TrueRandom.nextInt(x, y, z, bound)`. What is
/// preserved, and what a player can actually observe, is that it depends only on
/// the position.
fn position_roll(position: BlockPos) -> f64 {
    // SplitMix64 over the three coordinates. Each axis gets its own odd
    // multiplier before the words are folded together, so every axis changes the
    // result. Combining two of them with a shift instead would leave the third
    // out entirely — which is what made the scatter identical down every Z
    // column, so `//set 50%air,50%stone` carved corridors extruded along Z
    // rather than scattering blocks. Cheap, and unlike a product of the
    // coordinates it does not collapse nearby positions onto the same value.
    let mut hash = u64::from(position.x as u32).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ u64::from(position.y as u32).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ u64::from(position.z as u32).wrapping_mul(0x1656_67B1_9E37_79F9);
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94D0_49BB_1331_11EB);
    hash ^= hash >> 31;
    // 53 bits is the most an f64 carries exactly.
    (hash >> 11) as f64 / (1u64 << 53) as f64
}

/// The bracket properties of a block id, as name and value pairs.
///
/// Named because it appears in several signatures and spelling it out each time made
/// them unreadable.
pub type Properties = Vec<(String, String)>;

/// Why a pattern could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternError {
    /// An entry had no text at all, such as from `stone,,dirt`.
    Dangling(char),
    /// A `[` was never closed.
    UnclosedBracket,
    /// The block name is not one the server has.
    UnknownBlock {
        /// What the player typed, properties included.
        name: String,
    },
    /// A property was not `key=value`.
    BadProperty {
        /// The block being read.
        block: String,
        /// The offending text.
        property: String,
    },
    /// The server has the block but not this state of it.
    UnknownState {
        /// The block being read.
        block: String,
        /// The properties as typed.
        properties: String,
    },
    /// Every entry had a weight of zero, so nothing could ever be chosen.
    ZeroWeight,
    /// The pattern uses a feature this plugin does not implement.
    Unsupported(&'static str),
}

impl std::fmt::Display for PatternError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dangling(character) => {
                write!(
                    formatter,
                    "Invalid dangling character '{character}' in the pattern."
                )
            }
            Self::UnclosedBracket => write!(formatter, "Missing a ']' in the pattern."),
            Self::UnknownBlock { name } => write!(formatter, "Unknown block: '{name}'"),
            Self::BadProperty { block, property } => write!(
                formatter,
                "Bad state format in {block}: '{property}' is not key=value."
            ),
            Self::UnknownState { block, properties } => {
                write!(formatter, "{block} has no state {properties}.")
            }
            Self::ZeroWeight => write!(
                formatter,
                "Every pattern in the list has a zero weight, so none of them could be chosen."
            ),
            Self::Unsupported(what) => write!(formatter, "{what} is not supported."),
        }
    }
}

impl std::error::Error for PatternError {}

/// Reads a pattern against a server's block registry.
///
/// The resolver is taken at parse time rather than per block, so an unknown block
/// is reported to the player before any of the world has been touched.
pub fn parse(input: &str, resolver: &dyn BlockResolver) -> Result<Pattern, PatternError> {
    let mut entries: Vec<(Pattern, f64)> = Vec::new();
    for entry in split(input)? {
        entries.push(parse_entry(entry, resolver)?);
    }
    if entries.is_empty() {
        return Err(PatternError::UnknownBlock {
            name: input.to_string(),
        });
    }

    // `RandomPattern.add` merges a repeated pattern's weights instead of adding a
    // second arm, and drops a zero weight outright. Both are visible: repeating a
    // block is how a player expresses "mostly this one".
    let mut merged: Vec<(Pattern, f64)> = Vec::new();
    for (pattern, chance) in &entries {
        if *chance <= 0.0 {
            continue;
        }
        match merged
            .iter_mut()
            .find(|(existing, _)| *existing == *pattern)
        {
            Some((_, total)) => *total += *chance,
            None => merged.push((pattern.clone(), *chance)),
        }
    }
    if merged.is_empty() {
        // Only an all-zero LIST is an error. A lone zero-weight entry is not:
        // `RichPatternParser` returns `patterns.get(0)` as soon as there is one
        // pattern, so the chance is discarded before `RandomPattern` is ever built
        // and the zero-weight drop never runs. So `//set 0%stone` sets stone.
        if entries.len() == 1 {
            return Ok(entries
                .into_iter()
                .next()
                .map(|(pattern, _)| pattern)
                .expect("there is one entry"));
        }
        return Err(PatternError::ZeroWeight);
    }

    Ok(match merged.len() {
        1 => merged
            .pop()
            .map(|(pattern, _)| pattern)
            .expect("one entry is in merged"),
        _ => Pattern::Random(merged),
    })
}

/// Splits on the separators, keeping bracketed groups whole.
///
/// Returns an error for an empty segment, which is `FaweParser.parse` throwing
/// `fawe.error.parse.invalid-dangling-character`.
///
/// A `[` with no `]` swallows the rest of the input and is treated as if the
/// bracket were there, which is the same auto-close `FaweParser` performs.
fn split(input: &str) -> Result<Vec<&str>, PatternError> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut in_expression = false;
    for (index, character) in input.char_indices() {
        match character {
            '[' => {
                depth += 1;
                // A bracketed group ends the expression, so the separator after it
                // splits again. Same rule as the mask tokenizer's.
                in_expression = false;
            }
            ']' => depth = depth.saturating_sub(1),
            '=' if depth == 0 => in_expression = true,
            ',' | '&' | ' ' if depth == 0 && !in_expression => {
                let part = input[start..index].trim();
                if part.is_empty() {
                    return Err(PatternError::Dangling(character));
                }
                parts.push(part);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    let last = input[start..].trim();
    if !last.is_empty() {
        parts.push(last);
    }
    Ok(parts)
}

/// Reads one entry, peeling off at most one weight.
///
/// A weight whose body carries its own weight nests, and the outer weight wins
/// because the inner one becomes a lone pattern whose chance is discarded. That is
/// `RichPatternParser` recursing and then keeping only `chances.get(0)`.
fn parse_entry(entry: &str, resolver: &dyn BlockResolver) -> Result<(Pattern, f64), PatternError> {
    match split_weight(entry) {
        None => Ok((parse_body(entry, resolver)?, 1.0)),
        Some((chance, body)) => {
            let (pattern, _) = parse_entry(body, resolver)?;
            Ok((pattern, chance))
        }
    }
}

/// Splits a leading `number%` off an entry.
///
/// Returns `None` when the entry carries no weight, which is `percentPatternRegex`
/// failing to match.
fn split_weight(entry: &str) -> Option<(f64, &str)> {
    let percent = entry.find('%')?;
    let digits = &entry[..percent];
    let well_formed = !digits.is_empty()
        && digits.starts_with(|c: char| c.is_ascii_digit())
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.matches('.').count() <= 1;
    if !well_formed {
        return None;
    }
    let chance = digits.parse::<f64>().ok()?;
    (chance >= 0.0).then_some((chance, &entry[percent + 1..]))
}

/// Reads a single body: either a bracketed list or one block.
fn parse_body(body: &str, resolver: &dyn BlockResolver) -> Result<Pattern, PatternError> {
    if let Some(inner) = strip_outer_brackets(body) {
        return parse(inner, resolver);
    }
    if body.trim_start().starts_with('=') {
        // The splitter already kept the separators inside this entry, so everything
        // after `=` is the expression.
        return Err(PatternError::Unsupported("an `=` expression pattern"));
    }
    parse_block(body, resolver)
}

/// Strips one balanced `[...]` from around the whole text, if it is one.
///
/// `[stone][dirt]` is not one group, so it falls through and is read as a block
/// named `[stone][dirt]`, which fails. That matches the tokenizer: only the first
/// `[` opens a group and the rest is trailing arguments.
fn strip_outer_brackets(body: &str) -> Option<&str> {
    let inner = body.strip_prefix('[')?;
    let end = inner.rfind(']')?;
    if end + 1 != inner.len() {
        return None;
    }
    Some(&inner[..end])
}

/// Reads one block, with or without bracket properties.
/// Splits a block id into its name and its bracket properties.
///
/// `minecraft:oak_log[axis=x,waterlogged=false]` becomes
/// `("minecraft:oak_log", [("axis", "x"), ("waterlogged", "false")])`.
///
/// Shared with the schematic reader, which meets exactly this shape in a schematic's
/// block palette and must resolve it the same way a pattern does — a palette entry read
/// by a different rule than `//set oak_log[...]` would place a different block.
pub fn split_block_id(text: &str) -> Result<(&str, Properties), PatternError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(PatternError::UnknownBlock {
            name: String::new(),
        });
    }
    let (name, properties_text) = match text.find('[') {
        None => (text, ""),
        Some(index) => {
            let rest = &text[index + 1..];
            match rest.rfind(']') {
                Some(end) => (text[..index].trim(), &rest[..end]),
                // Auto-close an unterminated `[`, the way the tokenizer does.
                None => (text[..index].trim(), rest),
            }
        }
    };
    if name.is_empty() {
        return Err(PatternError::UnknownBlock {
            name: text.to_string(),
        });
    }
    Ok((name, parse_properties(properties_text)))
}

/// Reads one block, with or without bracket properties.
fn parse_block(text: &str, resolver: &dyn BlockResolver) -> Result<Pattern, PatternError> {
    let text = text.trim();
    let (name, properties) = split_block_id(text)?;
    // The bracket text on its own, so the error message can quote it the way the player
    // wrote it rather than the block name again.
    let properties_text = match text.find('[') {
        None => "",
        Some(index) => &text[index + 1..text.len().saturating_sub(1).max(index + 1)],
    };

    if let Some(offender) = properties
        .iter()
        .find(|(_, value)| value.is_empty())
        .map(|(key, _)| key.clone())
    {
        return Err(PatternError::BadProperty {
            block: name.to_string(),
            property: offender,
        });
    }

    let Some(state) = resolver.resolve(name, &properties) else {
        return Err(if properties_text.trim().is_empty() {
            PatternError::UnknownBlock {
                name: name.to_string(),
            }
        } else {
            PatternError::UnknownState {
                block: name.to_string(),
                properties: format!("[{properties_text}]"),
            }
        });
    };

    Ok(Pattern::Single(BlockValue::new(state)))
}

/// Turns `key=value,key2=value2` into pairs.
///
/// An empty property list yields no pairs, so `stone` reaches the resolver with no
/// state at all rather than with one blank property.
fn parse_properties(text: &str) -> Vec<(String, String)> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    text.split(',')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (key.trim().to_string(), value.trim().to_string()),
            None => (pair.to_string(), String::new()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockStateInfo;

    /// A registry with the handful of blocks the tests name, so parsing can be
    /// exercised without a server.
    struct TestBlocks;

    impl BlockResolver for TestBlocks {
        fn resolve(&self, name: &str, properties: &[(String, String)]) -> Option<u16> {
            let base = match name {
                "stone" | "minecraft:stone" => 1,
                "dirt" | "minecraft:dirt" => 2,
                "air" | "minecraft:air" => 0,
                "oak_log" => 3,
                _ => return None,
            };
            match name {
                "oak_log" => {
                    // Exactly one property, and it has to be the real one. Anything
                    // else is a state the server does not have.
                    if properties.len() != 1 {
                        return None;
                    }
                    let (key, value) = &properties[0];
                    if key != "axis" {
                        return None;
                    }
                    match value.as_str() {
                        "x" => Some(4),
                        "y" => Some(5),
                        _ => None,
                    }
                }
                // Only the log takes properties, so `stone[foo=bar]` has to fail
                // rather than quietly resolve to the plain block.
                _ if properties.is_empty() => Some(base),
                _ => None,
            }
        }

        fn info(&self, state: u16) -> Option<BlockStateInfo> {
            Some(BlockStateInfo {
                name: format!("minecraft:test_{state}"),
                properties: Vec::new(),
            })
        }

        fn is_air(&self, state: u16) -> bool {
            state == 0
        }

        fn is_solid(&self, _state: u16) -> bool {
            true
        }

        fn is_liquid(&self, _state: u16) -> bool {
            false
        }

        fn block_name(&self, _state: u16) -> Option<&str> {
            None
        }
    }

    fn ok(input: &str) -> Pattern {
        parse(input, &TestBlocks).unwrap_or_else(|error| panic!("{input:?}: {error}"))
    }

    fn entries(input: &str) -> Vec<(Pattern, f64)> {
        match ok(input) {
            Pattern::Random(entries) => entries,
            single => panic!("{input:?} did not become a list, got {single:?}"),
        }
    }

    #[test]
    fn a_bare_block_name_is_a_single_pattern() {
        assert_eq!(ok("stone"), Pattern::Single(BlockValue::new(1)));
    }

    #[test]
    fn the_minecraft_namespace_is_optional() {
        assert_eq!(ok("minecraft:dirt"), Pattern::Single(BlockValue::new(2)));
    }

    /// `stone,dirt` has to become a two-way random choice rather than a syntax
    /// error, because that is what makes the common scatter work.
    #[test]
    fn a_comma_list_becomes_an_equal_weight_choice() {
        let list = entries("stone,dirt");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].1, 1.0);
        assert_eq!(list[1].1, 1.0);
    }

    /// The lexer records which separator was used and the pattern parser ignores
    /// it, so all three have to mean the same thing.
    #[test]
    fn every_separator_builds_the_same_list() {
        for separator in [",", "&", " "] {
            assert_eq!(
                entries(&format!("stone{separator}dirt")).len(),
                2,
                "separator {separator:?} must build a list"
            );
        }
    }

    #[test]
    fn weights_are_read_from_a_leading_percentage() {
        let list = entries("50%stone,25%dirt,25%air");
        assert_eq!(
            list.iter().map(|(_, chance)| *chance).collect::<Vec<_>>(),
            vec![50.0, 25.0, 25.0]
        );
    }

    /// A single weighted entry has nothing to be random against, so it comes back
    /// as that entry rather than as a one-armed random pattern.
    #[test]
    fn a_lone_weighted_entry_is_just_that_entry() {
        assert_eq!(ok("100%stone"), Pattern::Single(BlockValue::new(1)));
    }

    /// Repeating an entry adds its weights together, which is how a player writes
    /// "mostly this one".
    #[test]
    fn a_repeated_entry_merges_its_weights() {
        let list = entries("50%stone,50%stone,dirt");
        assert_eq!(list.len(), 2, "the repeat must not add a third arm");
        assert_eq!(list[0].1, 100.0);
        assert_eq!(list[1].1, 1.0);
    }

    /// A zero-weight entry never fires. FAWE drops it from the collection rather
    /// than failing, so `0%stone,10%dirt` is all dirt.
    #[test]
    fn a_zero_weight_entry_is_dropped() {
        assert_eq!(
            ok("0%stone,10%dirt"),
            Pattern::Single(BlockValue::new(2)),
            "dropping the only zero-weight arm leaves a single pattern"
        );
    }

    #[test]
    fn an_all_zero_list_is_refused_rather_than_silently_becoming_air() {
        assert_eq!(
            parse("0%stone,0%dirt", &TestBlocks),
            Err(PatternError::ZeroWeight)
        );
    }

    /// The weight regex allows one decimal point. A malformed number must leave
    /// the whole entry as a block name so the player sees "unknown block" rather
    /// than a baffling "unknown state".
    #[test]
    fn a_malformed_weight_leaves_the_entry_as_a_block_name() {
        assert_eq!(
            parse("1.2.3%stone", &TestBlocks),
            Err(PatternError::UnknownBlock {
                name: "1.2.3%stone".to_string()
            })
        );
    }

    /// Bracket properties must not be split on their own commas, or
    /// `oak_log[axis=x]` would arrive as two entries.
    #[test]
    fn a_state_is_read_from_bracket_properties() {
        assert_eq!(ok("oak_log[axis=x]"), Pattern::Single(BlockValue::new(4)));
        assert_eq!(ok("oak_log[axis=y]"), Pattern::Single(BlockValue::new(5)));
    }

    #[test]
    fn several_properties_are_all_passed_to_the_resolver() {
        // The log only accepts `axis`, so a second property must be reported as an
        // unknown state rather than silently dropped.
        assert_eq!(
            parse("oak_log[axis=x,facing=north]", &TestBlocks),
            Err(PatternError::UnknownState {
                block: "oak_log".to_string(),
                properties: "[axis=x,facing=north]".to_string(),
            })
        );
    }

    /// A property that is not `key=value` is its own error, distinct from a state
    /// the server does not have.
    #[test]
    fn a_property_without_a_value_is_reported_as_such() {
        assert_eq!(
            parse("oak_log[axis]", &TestBlocks),
            Err(PatternError::BadProperty {
                block: "oak_log".to_string(),
                property: "axis".to_string(),
            })
        );
    }

    /// The tokenizer closes an unterminated `[` rather than failing on it, so
    /// `oak_log[axis=x` is closed before it reaches the resolver, so it resolves
    /// exactly as `oak_log[axis=x]` does rather than being rejected as malformed.
    #[test]
    fn an_unclosed_bracket_is_auto_closed() {
        assert_eq!(ok("oak_log[axis=x"), ok("oak_log[axis=x]"));
        assert_eq!(ok("oak_log[axis=x"), Pattern::Single(BlockValue::new(4)));
        // Closing it does not rescue a state that does not exist.
        assert!(matches!(
            parse("oak_log[axis=q", &TestBlocks),
            Err(PatternError::UnknownState { .. })
        ));
    }

    #[test]
    fn an_unknown_block_names_itself() {
        assert_eq!(
            parse("flurb", &TestBlocks),
            Err(PatternError::UnknownBlock {
                name: "flurb".to_string()
            })
        );
    }

    /// A list with a space in it is a typo, not two entries, and is rejected.
    #[test]
    fn an_empty_segment_between_separators_is_refused() {
        assert_eq!(
            parse("stone, dirt", &TestBlocks),
            Err(PatternError::Dangling(' '))
        );
        assert_eq!(
            parse("stone,,dirt", &TestBlocks),
            Err(PatternError::Dangling(','))
        );
    }

    /// A trailing separator is not an error, because the last segment is simply
    /// empty and the lexer never sees it as a dangling pair.
    #[test]
    fn a_trailing_separator_is_tolerated() {
        assert_eq!(ok("stone,"), Pattern::Single(BlockValue::new(1)));
        assert_eq!(entries("stone,dirt,").len(), 2);
    }

    #[test]
    fn a_bracketed_body_is_read_as_a_list() {
        assert_eq!(entries("[stone,dirt]").len(), 2);
    }

    /// The choice must depend only on the position. If it did not, re-running
    /// `//set 50%stone,50%dirt` would reshuffle the same blocks every time and
    /// `//undo` then `//redo` would not land back where it started.
    #[test]
    fn the_random_choice_is_stable_for_a_position() {
        let pattern = ok("stone,dirt");
        let position = BlockPos::new(37, 64, -12);
        let first = pattern.next(position);
        for _ in 0..64 {
            assert_eq!(pattern.next(position), first);
        }
    }

    /// Every axis has to affect the roll. When Z was left out of the hash, all
    /// the blocks in a Z column got the same arm, so `//set 50%air,50%stone`
    /// carved the selection into corridors extruded along Z instead of
    /// scattering it.
    #[test]
    fn the_random_choice_varies_along_z_as_well() {
        let pattern = ok("stone,dirt");
        let mut saw_stone = false;
        let mut saw_dirt = false;
        for z in 0..64 {
            match pattern.next(BlockPos::new(0, 64, z)).state {
                1 => saw_stone = true,
                2 => saw_dirt = true,
                _ => {}
            }
        }
        assert!(
            saw_stone && saw_dirt,
            "a single Z column only ever got one arm, so the roll ignores Z"
        );
    }

    #[test]
    fn both_arms_of_a_random_pattern_get_chosen_somewhere() {
        let pattern = ok("stone,dirt");
        let mut saw_stone = false;
        let mut saw_dirt = false;
        for x in 0..200 {
            match pattern.next(BlockPos::new(x, 64, 0)).state {
                1 => saw_stone = true,
                2 => saw_dirt = true,
                _ => {}
            }
        }
        assert!(saw_stone, "stone never came up");
        assert!(saw_dirt, "dirt never came up");
    }

    /// Two thirds stone, one third dirt: the weights are relative, so they are
    /// normalised by the sum rather than treated as percentages.
    #[test]
    fn weights_are_relative_to_each_other() {
        let pattern = ok("2%stone,1%dirt");
        let mut stone = 0;
        let mut dirt = 0;
        for x in 0..600 {
            match pattern.next(BlockPos::new(x, 0, 0)).state {
                1 => stone += 1,
                2 => dirt += 1,
                _ => {}
            }
        }
        assert!(
            stone > dirt * 3 / 2,
            "2:1 should favour stone, got {stone}:{dirt}"
        );
        assert!(
            stone < dirt * 5 / 2,
            "2:1 should not be overwhelming, got {stone}:{dirt}"
        );
    }

    #[test]
    fn an_empty_pattern_is_refused() {
        assert!(parse("", &TestBlocks).is_err());
        assert!(parse("   ", &TestBlocks).is_err());
    }

    /// `=` keeps the separators inside the entry, because an expression is full of
    /// commas. Without that, `=x>1,dirt` would read as an expression and a block.
    #[test]
    fn an_expression_pattern_is_reported_rather_than_split_on_its_commas() {
        assert!(matches!(
            parse("=x>1,dirt", &TestBlocks),
            Err(PatternError::Unsupported("an `=` expression pattern"))
        ));
        assert!(matches!(
            parse("stone,=x>1", &TestBlocks),
            Err(PatternError::Unsupported("an `=` expression pattern"))
        ));
    }
}
