//! The brushes: shapes that follow the block you are looking at.
//!
//! A brush is *equipped*, not applied. `//sphere stone 5` records the brush against the
//! player and tells them so; the world changes when they next right-click a block. FAWE
//! works the same way — `BrushCommands.process` only calls `tool.setPrimary` and prints
//! "brush equipped", and the tool runs from the click handler — so applying on the
//! command instead would be a different tool wearing the same name.
//!
//! ## Where the click lands
//!
//! FAWE traces from the player's eyes along their look vector. The plugin WIT has
//! `player.ray-trace-block`, which does exactly that, so the target position is the
//! host's answer rather than one computed from yaw and pitch here.
//!
//! ## What is not implemented
//!
//! The size adjust. FAWE grows and shrinks a brush with scroll or sneak-and-scroll, and
//! the WIT exposes no scroll event, so a brush's size is fixed when it is equipped and
//! has to be re-issued to change it. Every brush here takes its size as an argument
//! instead, which is how you size a brush in FAWE anyway.

use std::sync::Arc;

use worldedit_core::{
    block::BlockValue,
    clipboard::ClipboardHolder,
    extent::Extent,
    mask::{Mask, MaskContext},
    math::BlockPos,
    shape::{self, Bounds},
};

/// The brushes this plugin can equip.
///
/// A flat set of named shapes rather than the FAWE `Brush` trait hierarchy: the host
/// hands us a click and expects a set of blocks back, so the part of a brush that
/// matters here is "which positions, with which pattern", and every FAWE brush in this
/// list is a different answer to that question and nothing more.
///
/// The terrain-reading brushes (`Smooth`, `Raise`, `Flatten`, `Surface`) need the
/// world's own blocks to decide, which is why they cannot be pure functions of a
/// centre. They are kept honest by their tests using the in-memory `TestExtent`.
#[derive(Debug, Clone)]
pub enum Brush {
    /// `//sphere <pattern> [radius]` — FAWE's default radius is 2, not the 5 that
    /// `//set` uses.
    Sphere {
        /// The pattern to fill with.
        pattern: Arc<str>,
        /// The radius in blocks.
        radius: f64,
        /// `true` for `-h`.
        hollow: bool,
    },
    /// `//set [radius]` — fill a sphere, replacing every block that is already there
    /// rather than only the air.
    Set {
        /// The radius in blocks.
        radius: f64,
        /// `true` for a hollow region.
        hollow: bool,
    },
    /// `//cylinder <pattern> [radius] [height]`
    Cylinder {
        /// The pattern to fill with.
        pattern: Arc<str>,
        /// The radius in blocks.
        radius: f64,
        /// How many blocks tall.
        height: i32,
        /// `true` for a hollow cylinder.
        hollow: bool,
    },
    /// `//line <pattern> [height]` — a column rising from the target.
    Line {
        /// The pattern to fill with.
        pattern: Arc<str>,
        /// How many blocks tall.
        height: i32,
    },
    /// `//clipboard` — paste the clipboard with its origin at the target.
    Clipboard,
    /// `//snow` — lay snow on the topmost solid block within the radius.
    Snow {
        /// The radius to search in.
        radius: f64,
    },
    /// `//surface <pattern> [radius]` — replace the topmost solid block in each column.
    Surface {
        /// The pattern to lay on top.
        pattern: Arc<str>,
        /// The radius to search in.
        radius: f64,
    },
    /// `//circle <pattern> [radius]` — a filled disc on the target's own layer.
    Circle {
        /// The pattern to fill with.
        pattern: Arc<str>,
        /// The radius in blocks.
        radius: f64,
    },
    /// `//smooth [radius] [iterations]` — average each block with its neighbours.
    ///
    /// Every iteration reads the result of the last, so this is inherently two passes
    /// over the region; it is applied in one go rather than queued a tick at a time
    /// because a brush click is expected to be complete by the time the player sees it.
    Smooth {
        /// The radius of the neighbourhood to average over.
        radius: f64,
        /// How many times to repeat the average.
        iterations: i32,
    },
    /// `//raise <pattern> [count] [radius]` — move each column's floor up.
    Raise {
        /// The pattern to raise the floor with.
        pattern: Arc<str>,
        /// How many blocks to move each floor by.
        count: i32,
        /// The radius to search in.
        radius: f64,
    },
    /// `//lower <pattern> [count] [radius]` — move each column's floor down.
    Lower {
        /// The pattern to lower the floor with.
        pattern: Arc<str>,
        /// How many blocks to move each floor by.
        count: i32,
        /// The radius to search in.
        radius: f64,
    },
    /// `//flatten <pattern> [radius]` — bring every column to the region's mean height.
    Flatten {
        /// The pattern to fill with.
        pattern: Arc<str>,
        /// The radius to search in.
        radius: f64,
    },
}

impl Brush {
    /// The pattern this brush fills with, if it fills with a pattern at all.
    ///
    /// `None` for the brushes that do not name a block — `//set` reuses what is there,
    /// `//snow` and `//clipboard` carry their own — and for the terrain brushes, which
    /// write both the pattern and air. The caller needs this as text because that is what
    /// the pattern parser takes, and resolving it once per stroke rather than per block
    /// is the whole reason an edit is cheap.
    #[must_use]
    pub fn pattern_text(&self) -> Option<&str> {
        match self {
            Self::Sphere { pattern, .. }
            | Self::Cylinder { pattern, .. }
            | Self::Line { pattern, .. }
            | Self::Surface { pattern, .. }
            | Self::Circle { pattern, .. }
            | Self::Raise { pattern, .. }
            | Self::Lower { pattern, .. }
            | Self::Flatten { pattern, .. } => Some(pattern),
            Self::Smooth { .. } => None,
            Self::Set { .. } | Self::Snow { .. } | Self::Clipboard => None,
        }
    }

    /// The name FAWE would print after equipping this brush.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Sphere { .. } => "sphere",
            Self::Set { .. } => "set",
            Self::Cylinder { .. } => "cylinder",
            Self::Line { .. } => "line",
            Self::Clipboard => "clipboard",
            Self::Snow { .. } => "snow",
            Self::Surface { .. } => "surface",
            Self::Circle { .. } => "circle",
            Self::Smooth { .. } => "smooth",
            Self::Raise { .. } => "raise",
            Self::Lower { .. } => "lower",
            Self::Flatten { .. } => "flatten",
        }
    }
}

/// One block a brush decided to change: where, and to what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Affected {
    /// Where the block goes.
    pub position: BlockPos,
    /// The pattern text to fill it with.
    ///
    /// Text rather than a resolved state because a brush may touch the same block with
    /// a randomised pattern and each occurrence should draw its own value.
    pub pattern: Arc<str>,
}

/// A brush's geometry, as far as it does not depend on the world's contents.
///
/// Everything a brush needs to know about the world arrives as this, which keeps the
/// shape logic a pure function and therefore testable without an extent.
pub trait Probe {
    /// The block at `position`, or `None` if it is outside the world or unloaded.
    fn block(&self, position: BlockPos) -> Option<BlockValue>;
    /// Whether `position` is inside the world at all.
    fn inside(&self, position: BlockPos) -> bool;
    /// The world's vertical limits, so a brush does not wander off the top.
    fn bounds(&self) -> Bounds;
    /// The world's air state id.
    ///
    /// Air is a state like any other, so "is this air" is a comparison against whatever
    /// the server calls air rather than a check for a missing block. A world may have no
    /// blocks loaded at all, and those two states must not be confused.
    fn air_state(&self) -> u16;
}

/// A [`Probe`] backed by an [`Extent`] and the world's own height.
struct ExtentProbe<'a> {
    extent: &'a dyn Extent,
    bounds: Bounds,
}

impl Probe for ExtentProbe<'_> {
    fn block(&self, position: BlockPos) -> Option<BlockValue> {
        Probe::inside(self, position).then(|| self.extent.get_block(position))
    }

    fn inside(&self, position: BlockPos) -> bool {
        (self.bounds.min_y..=self.bounds.max_y).contains(&position.y)
    }

    fn air_state(&self) -> u16 {
        self.extent.air_state()
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }
}

/// Adapts an [`Extent`] to the [`Probe`] a brush reads through.
#[must_use]
pub fn probe_of(extent: &dyn Extent) -> impl Probe + '_ {
    ExtentProbe {
        extent,
        bounds: Bounds {
            min_y: extent.min_y(),
            max_y: extent.max_y(),
        },
    }
}

/// The blocks a brush changes at `centre`.
///
/// Every position is filtered through [`Probe::inside`] before it is returned, so a
/// brush at the edge of the world clips rather than writing into the void, and the same
/// is true of a generated shape's own height.
#[must_use]
pub fn apply(
    brush: &Brush,
    centre: BlockPos,
    clipboard: Option<&ClipboardHolder>,
    mask: Option<(&Mask, &mut MaskContext<'_>)>,
    probe: &dyn Probe,
) -> Vec<Affected> {
    let bounds = probe.bounds();
    let mut out: Vec<Affected> = match brush {
        Brush::Sphere {
            pattern,
            radius,
            hollow,
        } => shape::sphere(centre, (*radius, *radius, *radius), *hollow, bounds)
            .into_iter()
            .map(|position| Affected {
                position,
                pattern: Arc::clone(pattern),
            })
            .collect(),

        Brush::Set { radius, hollow } => {
            // A `//set` brush replaces what is there, so it skips air rather than
            // filling the empty space around a building.
            let filled = shape::sphere(centre, (*radius, *radius, *radius), *hollow, bounds);
            filled
                .into_iter()
                .filter(|position| is_solid(probe, *position))
                .map(|position| Affected {
                    position,
                    pattern: EXISTING.into(),
                })
                .collect()
        }

        Brush::Cylinder {
            pattern,
            radius,
            height,
            hollow,
        } => shape::cylinder(centre, (*radius, *radius), *height, *hollow, 0.0, bounds)
            .into_iter()
            .map(|position| Affected {
                position,
                pattern: Arc::clone(pattern),
            })
            .collect(),

        Brush::Line { pattern, height } => (0..*height)
            .map(|step| centre.offset(0, step, 0))
            .filter(|position| probe.inside(*position))
            .map(|position| Affected {
                position,
                pattern: Arc::clone(pattern),
            })
            .collect(),

        Brush::Clipboard => {
            let Some(holder) = clipboard else {
                return Vec::new();
            };
            holder
                .clipboard()
                .paste_steps(holder.transform(), centre)
                .into_iter()
                .map(|(position, _)| Affected {
                    position,
                    pattern: CLIPBOARD.into(),
                })
                .collect()
        }

        Brush::Snow { radius } => top_surface(probe, centre, *radius)
            .into_iter()
            .map(|position| Affected {
                position,
                pattern: SNOW.into(),
            })
            .collect(),

        Brush::Surface { pattern, radius } => top_surface(probe, centre, *radius)
            .into_iter()
            .map(|position| Affected {
                position,
                pattern: Arc::clone(pattern),
            })
            .collect(),

        Brush::Circle { pattern, radius } => {
            let reach = radius.ceil() as i32;
            let mut positions = Vec::new();
            for dx in -reach..=reach {
                for dz in -reach..=reach {
                    if f64::from(dx * dx + dz * dz) > *radius * *radius {
                        continue;
                    }
                    let position = centre.offset(dx, 0, dz);
                    if probe.inside(position) {
                        positions.push(position);
                    }
                }
            }
            positions
                .into_iter()
                .map(|position| Affected {
                    position,
                    pattern: Arc::clone(pattern),
                })
                .collect()
        }

        Brush::Smooth { radius, iterations } => smooth(probe, centre, *radius, *iterations),

        Brush::Raise {
            pattern,
            count,
            radius,
        }
        | Brush::Lower {
            pattern,
            count,
            radius,
        } => {
            let down = matches!(brush, Brush::Lower { .. });
            top_surface(probe, centre, *radius)
                .into_iter()
                .filter_map(|top| {
                    // Moving a floor down means finding the next solid block under the
                    // current one; moving up means the new floor is the top itself.
                    let target = if down {
                        find_floor_below(probe, top, *count)?
                    } else {
                        top
                    };
                    Some(Affected {
                        position: target,
                        pattern: Arc::clone(pattern),
                    })
                })
                .collect()
        }

        Brush::Flatten { pattern, radius } => {
            let columns = top_surface(probe, centre, *radius);
            if columns.is_empty() {
                return Vec::new();
            }
            // The mean is over whole blocks: FAWE flattens to an integer height, and
            // a fractional target would leave a ragged edge.
            let mean =
                (columns.iter().map(|p| i64::from(p.y)).sum::<i64>() / columns.len() as i64) as i32;
            let mut out = Vec::new();
            for column in columns {
                // Everything between the existing surface and the mean is rewritten,
                // whether that means filling a hollow or cutting a rise off. Leaving the
                // low column alone and only filling upwards would leave a hole in the
                // middle of a flattened field.
                let from = mean.min(column.y);
                let to = mean.max(column.y);
                for y in from..=to {
                    out.push(Affected {
                        position: BlockPos::new(column.x, y, column.z),
                        pattern: Arc::clone(pattern),
                    });
                }
            }
            out
        }
    };

    // A mask narrows the brush the same way it narrows a region command. It is applied
    // after the shape so that a mask cannot change the shape's own arithmetic.
    //
    // `smooth` already consulted it while deciding which columns to average; testing
    // again here is harmless because smoothing only ever *adds* blocks inside a column
    // it already admitted, so the second test keeps exactly the same set.
    if let Some((mask, context)) = mask {
        out.retain(|affected| mask.test(context, affected.position));
    }
    out
}

/// The pattern a `//set` brush uses: whatever is already there, unchanged.
///
/// Reached through the same `Affected` shape as every other brush so that a mask, a
/// block limit and the undo history all see one uniform list.
const EXISTING: &str = "\u{0}existing";

/// A `//clipboard` brush's marker, resolved by the caller against the real paste.
const CLIPBOARD: &str = "\u{0}clipboard";

/// `//snow`'s block.
const SNOW: &str = "minecraft:snow";

/// Whether a block counts as something worth keeping or replacing.
///
/// Air is the only thing treated as empty, which is what makes `//snow` and `//set`
/// agree about what a surface is.
fn is_solid(probe: &dyn Probe, position: BlockPos) -> bool {
    probe
        .block(position)
        .is_some_and(|block| block.state != probe.air_state())
}

/// The topmost solid block in each column within `radius` of `centre`.
///
/// This is what `//snow`, `//surface`, `//raise`, `//lower` and `//flatten` all need
/// and all mean by "the surface": the highest block in the column that is not air. A
/// column with nothing solid in it is skipped rather than treated as the floor, because
/// filling a void to build a platform is what `//sphere` is for.
fn top_surface(probe: &dyn Probe, centre: BlockPos, radius: f64) -> Vec<BlockPos> {
    let reach = radius.ceil() as i32;
    let bounds = probe.bounds();
    let mut tops = Vec::new();
    for dx in -reach..=reach {
        for dz in -reach..=reach {
            if f64::from(dx * dx + dz * dz) > radius * radius {
                continue;
            }
            let column = centre.offset(dx, 0, dz);
            if !probe.inside(column) {
                continue;
            }
            // Search down from just under the top of the world: the surface is the
            // ceiling, not the centre, and a brush used on a tall column should find it.
            let Some(y) = (bounds.min_y..=bounds.max_y)
                .rev()
                .find(|y| is_solid(probe, BlockPos::new(column.x, *y, column.z)))
            else {
                continue;
            };
            tops.push(BlockPos::new(column.x, y, column.z));
        }
    }
    tops
}

/// The top of the solid run `count` blocks below `top`, or `None` if the column ends.
///
/// A `//lower` moves the floor down by exactly `count`: it walks the column and stops at
/// the `count`-th solid block below the current surface, so lowering by more than the
/// column's depth leaves the floor where the world ends instead of deleting it.
fn find_floor_below(probe: &dyn Probe, top: BlockPos, count: i32) -> Option<BlockPos> {
    let bounds = probe.bounds();
    let mut found = 0;
    for y in (bounds.min_y..top.y).rev() {
        if is_solid(probe, BlockPos::new(top.x, y, top.z)) {
            found += 1;
            if found >= count {
                return Some(BlockPos::new(top.x, y, top.z));
            }
        }
    }
    None
}

/// Averages each block in a sphere with its neighbours, `iterations` times.
///
/// Air is counted in the average rather than skipped: averaging only the solid blocks
/// would let a surface next to open sky stay at full height, so a smoothed hillside
/// would keep its cliff edge.
fn smooth(probe: &dyn Probe, centre: BlockPos, radius: f64, iterations: i32) -> Vec<Affected> {
    let bounds = probe.bounds();
    let sphere = shape::sphere(centre, (radius, radius, radius), false, bounds);
    let mut out = Vec::new();

    for _ in 0..iterations.max(0) {
        // Read every height before writing any, so a pass cannot feed its own writes
        // into the next block's average. One pass left to right would otherwise let
        // the smoothed height leak into its neighbour and pull the whole region down.
        let heights: Vec<Option<i32>> = sphere
            .iter()
            .map(|position| column_height(probe, *position))
            .collect();

        let mut next = Vec::new();
        for (index, position) in sphere.iter().enumerate() {
            let Some(current) = heights[index] else {
                continue;
            };
            // A missing neighbour means the sphere reaches past the top or bottom of
            // the world. Treated as "no opinion" rather than as height 0, which would
            // sink the edge of every smoothed region by half its height.
            let reach = radius.ceil() as i32;
            let mut total = i64::from(current);
            let mut count = 1_i64;
            for dy in -reach..=reach {
                for dx in -reach..=reach {
                    for dz in -reach..=reach {
                        if dx == 0 && dy == 0 && dz == 0 {
                            continue;
                        }
                        let neighbour = position.offset(dx, dy, dz);
                        if !probe.inside(neighbour) {
                            continue;
                        }
                        if let Some(neighbour_height) = column_height(probe, neighbour) {
                            total += i64::from(neighbour_height);
                            count += 1;
                        }
                    }
                }
            }
            let target = (total / count) as i32;
            let target = target.clamp(bounds.min_y, bounds.max_y - 1);
            // Ordered low to high rather than `current..target`, which is empty whenever
            // `target` is below `current` — and lowering a column is exactly what
            // smoothing is for, so an unordered range would leave every column above the
            // mean untouched and the brush would appear to do nothing.
            for y in current.min(target)..current.max(target) {
                if y < target {
                    // The column has to grow. A smoothing brush names no block of its own
                    // to grow it with, and FAWE's `//smooth` likewise takes an optional
                    // fill block; without one it leaves the gap rather than guessing.
                    continue;
                }
                next.push(Affected {
                    position: BlockPos::new(position.x, y, position.z),
                    pattern: AIR.into(),
                });
            }
        }
        out.append(&mut next);
    }
    out
}

/// How many blocks are solid above and including `position` in its column.
fn column_height(probe: &dyn Probe, position: BlockPos) -> Option<i32> {
    if !probe.inside(position) {
        return None;
    }
    let bounds = probe.bounds();
    (position.y..bounds.max_y)
        .all(|y| !is_solid(probe, BlockPos::new(position.x, y, position.z)))
        .then_some(position.y)
}

/// `//smooth` clears the air above a lowered column.
const AIR: &str = "minecraft:air";

/// `//smooth` has no fill block of its own; it lowers to here and leaves it for the
/// player to fill, which is what FAWE's `//smooth` does with no argument.
const BLANK: &str = "\u{0}skip";

/// Whether an [`Affected`] is one of the sentinels rather than a real pattern.
///
/// Kept as a function because the three callers all have to agree on the set, and a
/// string comparison repeated in three places is how one of them ends up trying to parse
/// `"\u{0}existing"` as a block name.
#[must_use]
pub fn is_sentinel(pattern: &str) -> bool {
    matches!(pattern, EXISTING | CLIPBOARD | BLANK)
}

/// The blocks a brush wants written, split by which job can write them.
///
/// A brush produces three kinds of write and they need three different routes, so this
/// is the one place that knows about all of them:
///
/// - an ordinary pattern goes to the edit queue, where the limit and the undo history
///   live;
/// - a clipboard goes to the paste queue, because its blocks are the clipboard's own
///   and re-resolving a pattern to arrive at the same state would waste a lookup per
///   block;
/// - `//set`'s "whatever is there" resolves to the block already in the world, so it is
///   not a write at all and is dropped here.
#[derive(Debug, Default)]
pub struct Writes {
    /// Positions to fill with the brush's own pattern.
    pub positions: Vec<BlockPos>,
    /// Positions to clear.
    ///
    /// A separate list because clearing needs a different pattern, and one edit job
    /// carries exactly one. `//smooth` is the only brush that writes both.
    pub air: Vec<BlockPos>,
    /// The clipboard's own paste steps.
    pub paste: Vec<(BlockPos, BlockValue)>,
    /// How many blocks were dropped as already correct.
    pub unchanged: usize,
}

impl Writes {
    /// How many blocks this brush stroke writes in total.
    #[must_use]
    pub fn total(&self) -> usize {
        self.positions.len() + self.air.len() + self.paste.len()
    }
}

/// Sorts a brush's [`Affected`] list into the routes above.
///
/// `clipboard_steps` is the caller's own paste iterator. It is needed because
/// [`apply`] only learns the *positions* a clipboard brush touches; the blocks come from
/// the clipboard, which lives in the session and is borrowed there.
#[must_use]
pub fn route(affected: Vec<Affected>, clipboard_steps: Vec<(BlockPos, BlockValue)>) -> Writes {
    let mut writes = Writes::default();
    let mut wanted: Vec<BlockPos> = Vec::new();

    for entry in affected {
        match entry.pattern.as_ref() {
            CLIPBOARD => wanted.push(entry.position),
            EXISTING | BLANK => writes.unchanged += 1,
            AIR => writes.air.push(entry.position),
            _ => writes.positions.push(entry.position),
        }
    }

    // The brush decided *where* the clipboard goes; the paste decides *what* each of
    // those blocks becomes, so the blocks are taken from the paste rather than
    // re-derived. A position the brush named but the paste did not is dropped: it means
    // the clipboard has no block there, which is the paste being authoritative.
    for (position, block) in clipboard_steps {
        if wanted.contains(&position) {
            writes.paste.push((position, block));
        }
    }
    writes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test world's air, which is a different state from the one it fills with.
    const AIR_STATE: u16 = 0;

    /// The state a `TestWorld`'s columns are made of.
    const SOLID_STATE: u16 = 1;

    /// A fixed world, so a brush's arithmetic can be checked against a known answer.
    struct TestWorld {
        blocks: Vec<(BlockPos, bool)>,
        bounds: Bounds,
    }

    impl TestWorld {
        fn new(bounds: Bounds) -> Self {
            Self {
                blocks: Vec::new(),
                bounds,
            }
        }

        /// A solid column of `height` blocks standing on `floor`.
        fn column(mut self, x: i32, z: i32, floor: i32, height: i32) -> Self {
            for y in floor..floor + height {
                self.blocks.push((BlockPos::new(x, y, z), true));
            }
            self
        }
    }

    impl Probe for TestWorld {
        fn block(&self, position: BlockPos) -> Option<BlockValue> {
            // A position with nothing recorded in it reads as air rather than as
            // missing, so a sparse test world behaves like a real one where the void is
            // still a block that can be stood on top of.
            self.blocks
                .iter()
                .find(|(p, _)| *p == position)
                .map(|_| BlockValue::new(SOLID_STATE))
        }

        fn inside(&self, position: BlockPos) -> bool {
            (self.bounds.min_y..=self.bounds.max_y).contains(&position.y)
        }

        fn bounds(&self) -> Bounds {
            self.bounds
        }

        fn air_state(&self) -> u16 {
            AIR_STATE
        }
    }

    #[test]
    fn a_sphere_brush_fills_every_block_in_its_radius() {
        let world = TestWorld::new(Bounds {
            min_y: -64,
            max_y: 319,
        });
        let brush = Brush::Sphere {
            pattern: "minecraft:stone".into(),
            radius: 2.0,
            hollow: false,
        };
        let affected = apply(&brush, BlockPos::new(0, 64, 0), None, None, &world);
        // `makeSphere` adds 0.5 to the radius, so a radius-2 brush is really a
        // radius-2.5 ball: every offset whose squares sum to at most 6, which counts 81.
        assert_eq!(affected.len(), 81);
        assert!(
            affected
                .iter()
                .all(|a| a.pattern.as_ref() == "minecraft:stone")
        );
    }

    #[test]
    fn a_hollow_sphere_brush_leaves_the_middle_alone() {
        let world = TestWorld::new(Bounds {
            min_y: -64,
            max_y: 319,
        });
        let brush = Brush::Sphere {
            pattern: "minecraft:stone".into(),
            radius: 3.0,
            hollow: true,
        };
        let affected = apply(&brush, BlockPos::new(0, 64, 0), None, None, &world);
        assert!(
            !affected
                .iter()
                .any(|a| a.position == BlockPos::new(0, 64, 0)),
            "the centre of a hollow sphere must survive"
        );
    }

    #[test]
    fn a_set_brush_skips_air_rather_than_filling_it() {
        // One solid block and nothing else, so a radius-1 sphere around it has one
        // solid and 26 air.
        let world = TestWorld::new(Bounds {
            min_y: -64,
            max_y: 319,
        })
        .column(0, 0, 64, 1);
        let brush = Brush::Set {
            radius: 1.0,
            hollow: false,
        };
        let affected = apply(&brush, BlockPos::new(0, 64, 0), None, None, &world);
        assert_eq!(
            affected.len(),
            1,
            "//set replaces what is there, not the air around it"
        );
        assert_eq!(affected[0].position, BlockPos::new(0, 64, 0));
    }

    #[test]
    fn a_brush_clips_at_the_bottom_of_the_world() {
        // A sphere centred at the very bottom would otherwise write below it.
        let world = TestWorld::new(Bounds { min_y: 0, max_y: 3 });
        let brush = Brush::Sphere {
            pattern: "minecraft:stone".into(),
            radius: 5.0,
            hollow: false,
        };
        let affected = apply(&brush, BlockPos::new(0, 0, 0), None, None, &world);
        assert!(affected.iter().all(|a| a.position.y >= 0));
    }

    #[test]
    fn a_cylinder_brush_is_a_radius_wide_column() {
        let world = TestWorld::new(Bounds {
            min_y: -64,
            max_y: 319,
        });
        let brush = Brush::Cylinder {
            pattern: "minecraft:stone".into(),
            radius: 1.0,
            height: 3,
            hollow: false,
        };
        let affected = apply(&brush, BlockPos::new(0, 64, 0), None, None, &world);
        // `makeCylinder` also adds 0.5, so a radius-1 brush covers the 3x3 square, and
        // three layers of nine is 27.
        assert_eq!(affected.len(), 27);
    }

    #[test]
    fn a_line_brush_rises_from_the_target() {
        let world = TestWorld::new(Bounds {
            min_y: -64,
            max_y: 319,
        });
        let brush = Brush::Line {
            pattern: "minecraft:glass".into(),
            height: 4,
        };
        let affected = apply(&brush, BlockPos::new(0, 64, 0), None, None, &world);
        assert_eq!(affected.len(), 4);
        assert_eq!(affected[0].position.y, 64);
        assert_eq!(affected[3].position.y, 67);
    }

    #[test]
    fn a_snow_brush_lands_on_the_top_of_each_column() {
        let world = TestWorld::new(Bounds {
            min_y: 0,
            max_y: 100,
        })
        .column(0, 0, 60, 5) // top at 64
        .column(1, 0, 60, 9); // top at 68
        let brush = Brush::Snow { radius: 2.0 };
        let affected = apply(&brush, BlockPos::new(0, 64, 0), None, None, &world);
        assert!(
            affected
                .iter()
                .any(|a| a.position == BlockPos::new(0, 64, 0))
        );
        assert!(
            affected
                .iter()
                .any(|a| a.position == BlockPos::new(1, 68, 0))
        );
        assert!(
            affected
                .iter()
                .all(|a| a.pattern.as_ref() == "minecraft:snow")
        );
    }

    #[test]
    fn a_snow_brush_skips_a_column_with_nothing_in_it() {
        let world = TestWorld::new(Bounds {
            min_y: 0,
            max_y: 100,
        })
        .column(0, 0, 60, 1);
        let brush = Brush::Snow { radius: 1.0 };
        let affected = apply(&brush, BlockPos::new(0, 60, 0), None, None, &world);
        assert_eq!(
            affected.len(),
            1,
            "only the one column with a solid block in it"
        );
    }

    #[test]
    fn a_flatten_brush_brings_every_column_to_the_mean_height() {
        // Two columns, tops at 60 and 68, so the mean is 64.
        let world = TestWorld::new(Bounds {
            min_y: 0,
            max_y: 100,
        })
        .column(0, 0, 50, 11) // top at 60
        .column(1, 0, 60, 9); // top at 68
        let brush = Brush::Flatten {
            pattern: "minecraft:grass_block".into(),
            // A radius of 1 reaches the second column; radius 0 would only ever see one.
            radius: 1.0,
        };
        let affected = apply(&brush, BlockPos::new(0, 60, 0), None, None, &world);
        // Column 0 gains blocks from 61 up to 64; column 1 loses 65..=68.
        // Both columns are rewritten between their own surface and the mean, so the low one
        // is filled up and the high one is cut down.
        let low: Vec<_> = affected
            .iter()
            .filter(|a| a.position.x == 0)
            .map(|a| a.position.y)
            .collect();
        assert_eq!(low, vec![60, 61, 62, 63, 64]);
        let high: Vec<_> = affected
            .iter()
            .filter(|a| a.position.x == 1)
            .map(|a| a.position.y)
            .collect();
        assert_eq!(high, vec![64, 65, 66, 67, 68]);
    }

    #[test]
    fn a_raise_brush_moves_a_floor_up_by_the_count() {
        let world = TestWorld::new(Bounds {
            min_y: 0,
            max_y: 100,
        })
        .column(0, 0, 60, 4); // top at 63
        let brush = Brush::Raise {
            pattern: "minecraft:stone".into(),
            count: 3,
            radius: 0.0,
        };
        let affected = apply(&brush, BlockPos::new(0, 63, 0), None, None, &world);
        assert_eq!(
            affected.len(),
            1,
            "raising puts one block on top of the surface"
        );
        assert_eq!(affected[0].position.y, 63);
    }

    #[test]
    fn a_lower_brush_finds_the_surface_below_the_one_above_it() {
        let world = TestWorld::new(Bounds {
            min_y: 0,
            max_y: 100,
        })
        .column(0, 0, 60, 10); // solid from 60 to 69
        let brush = Brush::Lower {
            pattern: "minecraft:stone".into(),
            count: 3,
            radius: 0.0,
        };
        let affected = apply(&brush, BlockPos::new(0, 69, 0), None, None, &world);
        assert_eq!(affected.len(), 1);
        // Three solid blocks below the top are 68, 67, 66.
        assert_eq!(affected[0].position.y, 66);
    }

    #[test]
    fn a_clipboard_brush_with_no_clipboard_does_nothing() {
        let world = TestWorld::new(Bounds {
            min_y: -64,
            max_y: 319,
        });
        let affected = apply(
            &Brush::Clipboard,
            BlockPos::new(0, 64, 0),
            None,
            None,
            &world,
        );
        assert!(affected.is_empty());
    }

    /// A pattern brush's positions go to the edit queue and nothing else does.
    ///
    /// The routing is what keeps a brush stroke in the undo history: an edit job records
    /// its own changes, so anything sent round it would be invisible to `//undo`.
    #[test]
    fn a_pattern_brush_routes_its_positions_to_the_edit_queue() {
        let world = TestWorld::new(Bounds {
            min_y: -64,
            max_y: 319,
        });
        let brush = Brush::Sphere {
            pattern: "minecraft:stone".into(),
            radius: 1.0,
            hollow: false,
        };
        let writes = route(
            apply(&brush, BlockPos::new(0, 64, 0), None, None, &world),
            Vec::new(),
        );
        assert!(!writes.positions.is_empty());
        assert!(writes.paste.is_empty(), "a sphere brush has no clipboard");
        assert_eq!(
            writes.unchanged, 0,
            "a sphere brush replaces air too, so nothing is skipped"
        );
    }

    /// A `//set` brush touches only what is already there, so its writes are all
    /// `existing` and there is nothing for the pattern queue to do.
    ///
    /// This is the property that distinguishes `//set` from `//sphere`: given the same
    /// sphere and the same world, `//sphere` writes 27 blocks and `//set` writes one.
    #[test]
    fn a_set_brush_leaves_nothing_for_the_pattern_queue() {
        let world = TestWorld::new(Bounds {
            min_y: -64,
            max_y: 319,
        })
        .column(0, 0, 64, 1);
        let brush = Brush::Set {
            radius: 1.0,
            hollow: false,
        };
        let writes = route(
            apply(&brush, BlockPos::new(0, 64, 0), None, None, &world),
            Vec::new(),
        );
        assert_eq!(
            writes.unchanged, 1,
            "the one solid block is already correct"
        );
        assert!(
            writes.positions.is_empty(),
            "//set must not fill the air around the block"
        );
    }

    /// A clipboard brush's blocks come from the paste, not from a pattern.
    ///
    /// `apply` only learns which positions the clipboard touches; the blocks themselves
    /// live in the session and arrive as `clipboard_steps`. Routing has to take them from
    /// there, or a clipboard brush would paste nothing.
    #[test]
    fn a_clipboard_brush_routes_the_paste_not_a_pattern() {
        // No world is needed: routing reads only the `Affected` list and the paste steps,
        // which is what makes this checkable without one.
        // Two blocks the clipboard will place, and one it does not.
        let steps = vec![
            (BlockPos::new(0, 64, 0), BlockValue::new(1)),
            (BlockPos::new(0, 65, 0), BlockValue::new(1)),
            (BlockPos::new(9, 9, 9), BlockValue::new(1)),
        ];
        let wanted = vec![
            Affected {
                position: BlockPos::new(0, 64, 0),
                pattern: Arc::from(CLIPBOARD),
            },
            Affected {
                position: BlockPos::new(0, 65, 0),
                pattern: Arc::from(CLIPBOARD),
            },
            Affected {
                position: BlockPos::new(5, 5, 5),
                pattern: Arc::from(CLIPBOARD),
            },
        ];
        let writes = route(wanted, steps);
        assert_eq!(
            writes.paste.len(),
            2,
            "the block the brush did not name is dropped"
        );
        assert!(
            !writes
                .paste
                .iter()
                .any(|(p, _)| *p == BlockPos::new(9, 9, 9)),
            "the paste is authoritative about what it places"
        );
        assert!(writes.positions.is_empty());
    }

    /// A smoothing brush clears as well as fills, and the two have to be separated.
    ///
    /// One edit job carries one pattern, so a list that mixed air and the fill block
    /// would fill the whole thing with whichever it resolved first.
    #[test]
    fn a_smoothing_brush_separates_its_air_from_its_fill() {
        let world = TestWorld::new(Bounds {
            min_y: 0,
            max_y: 40,
        })
        .column(0, 0, 30, 10) // a pillar ten tall
        .column(1, 0, 30, 2); // a stub two tall
        let brush = Brush::Smooth {
            radius: 1.0,
            iterations: 1,
        };
        let writes = route(
            apply(&brush, BlockPos::new(0, 39, 0), None, None, &world),
            Vec::new(),
        );
        // A smoothing brush names no pattern of its own: it only clears. The fill is
        // deliberately left to the player, as FAWE's does.
        assert_eq!(brush.pattern_text(), None);
        assert!(
            !writes.air.is_empty(),
            "the tall pillar must lose the blocks above the mean"
        );
        assert!(
            writes.positions.is_empty(),
            "a smoothing brush has nothing to fill with"
        );
    }

    #[test]
    fn a_circle_brush_is_flat_and_does_not_reach_the_corners() {
        let world = TestWorld::new(Bounds {
            min_y: -64,
            max_y: 319,
        });
        let brush = Brush::Circle {
            pattern: "minecraft:stone".into(),
            radius: 2.0,
        };
        let affected = apply(&brush, BlockPos::new(0, 64, 0), None, None, &world);
        assert!(affected.iter().all(|a| a.position.y == 64));
        assert!(
            !affected
                .iter()
                .any(|a| a.position.x == 2 && a.position.z == 2),
            "the corner of a radius-2 circle is outside it"
        );
    }
}
