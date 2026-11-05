//! Clipboard paste transforms.
//!
//! `//rotate` and `//flip` never touch the copied blocks. They set a transform on
//! the holder, and the paste applies it to each destination coordinate, which is
//! what `PositionTransformExtent` does in FAWE:
//!
//! ```text
//! destination = pivot + floor(round_half_up(transform(source - pivot)))
//! ```
//!
//! The pivot is the clipboard origin, so a rotation turns the copy about its own
//! anchor rather than about the world origin.
//!
//! A turn moves the blocks as well as their positions, though: a stair copied
//! facing north comes out of `//rotate 90` facing east. FAWE does that by wrapping
//! the clipboard in a `BlockTransformExtent` before the copy runs
//! (`PasteBuilder.java:146-151`), and reading a block through that extent rewrites
//! its directional state (`BlockTransformExtent.getFullBlock`,
//! `BlockTransformExtent.java:532-539`) along with a sign's `Rot` byte
//! (`transformBaseBlockNBT`, `BlockTransformExtent.java:403-430`).
//! [`Transform::transform_block`] is that rewrite, and
//! [`crate::clipboard::Clipboard::paste_steps_with`] is where a paste applies it.

use crate::{
    block::{BlockResolver, BlockValue},
    math::{BlockPos, Vec3},
};

/// A rotation and mirror to apply while pasting.
#[derive(Debug, Clone, PartialEq)]
pub enum Transform {
    /// Leave every coordinate alone.
    Identity,
    /// A rotation about the given axis, in degrees clockwise.
    ///
    /// `//rotate 90` builds `RotationY(-90)`, because a positive yaw in Minecraft
    /// turns clockwise and `rotateY` turns counter-clockwise.
    RotateY(f64),
    /// A mirror along one axis, from `//flip`.
    Flip(BlockAxis),
    /// A rotation applied and then a mirror, which is what stacking the two
    /// commands produces. Both sides are boxed because either can be another
    /// `Then`, and an unbounded chain has to go through a pointer.
    Then(Box<Transform>, Box<Transform>),
}

/// The axis a mirror flips along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockAxis {
    /// The X axis.
    X,
    /// The Y axis.
    Y,
    /// The Z axis.
    Z,
}

impl BlockAxis {
    /// The per-axis sign for a mirror along this axis.
    #[must_use]
    pub fn flip_scale(self) -> Vec3 {
        match self {
            Self::X => Vec3::new(-1.0, 1.0, 1.0),
            Self::Y => Vec3::new(1.0, -1.0, 1.0),
            Self::Z => Vec3::new(1.0, 1.0, -1.0),
        }
    }
}

impl Transform {
    /// This transform applied after `self`.
    #[must_use]
    pub fn then(self, other: Transform) -> Transform {
        if other == Transform::Identity {
            return self;
        }
        if self == Transform::Identity {
            return other;
        }
        Transform::Then(Box::new(self), Box::new(other))
    }

    /// Whether this transform would leave every coordinate alone.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        match self {
            Self::Identity => true,
            // A whole number of turns is the identity even though it was asked for.
            Self::RotateY(degrees) => degrees % 360.0 == 0.0,
            Self::Flip(_) => false,
            // Both sides, as `CombinedTransform#isIdentity` does — it returns false if *any*
            // of its transforms is not the identity. `then` collapses an identity operand
            // away, so no `Then` reachable from the command layer has one, and checking the
            // tail alone happens to agree; checking both is what keeps that an accident of
            // the builder rather than the rule.
            Self::Then(first, second) => first.is_identity() && second.is_identity(),
        }
    }

    /// This transform applied to `point`.
    ///
    /// The point is already relative to the pivot, so no pivot is threaded
    /// through: only the rotation and the mirror are affine about the origin, and
    /// [`Transform::paste_position`] does the subtracting.
    #[must_use]
    pub fn apply(&self, point: Vec3) -> Vec3 {
        match self {
            Self::Identity => point,
            Self::RotateY(degrees) => {
                let (sin, cos) = degrees.to_radians().sin_cos();
                Vec3::new(
                    point.x * cos + point.z * sin,
                    point.y,
                    -point.x * sin + point.z * cos,
                )
            }
            Self::Flip(axis) => {
                let scale = axis.flip_scale();
                Vec3::new(point.x * scale.x, point.y * scale.y, point.z * scale.z)
            }
            Self::Then(first, second) => second.apply(first.apply(point)),
        }
    }

    /// Where a block of the clipboard ends up.
    ///
    /// The order matters and is the one `PositionTransformExtent.getPos` uses:
    /// transform, round half away from zero, then floor to a block position.
    #[must_use]
    pub fn paste_position(&self, source: BlockPos, pivot: BlockPos) -> BlockPos {
        if self.is_identity() {
            return source;
        }
        let relative = Vec3::new(
            f64::from(source.x - pivot.x),
            f64::from(source.y - pivot.y),
            f64::from(source.z - pivot.z),
        );
        let moved = self.apply(relative);
        BlockPos::new(
            pivot.x + moved.x.round() as i32,
            pivot.y + moved.y.round() as i32,
            pivot.z + moved.z.round() as i32,
        )
    }

    /// The block as it comes out of this transform, or unchanged if the turn
    /// cannot be resolved.
    ///
    /// FAWE does not rotate a property's *value*. It rotates the direction each
    /// value stands for and asks which value the answer landed on
    /// (`BlockTransformExtent#getDirections` and `adapt`), which is why the
    /// tables below are direction sets rather than a rotation offset - and why a
    /// two-value `axis` pairs `y` with south-north rather than with up-down, so
    /// `//rotate 90` lays a standing log on its side.
    ///
    /// A state the server cannot resolve is a state it does not have, so the
    /// block goes back as it came rather than becoming a default: refusing the
    /// paste would turn one unknown state into a whole missing build.
    pub fn transform_block(&self, block: &BlockValue, blocks: &dyn BlockResolver) -> BlockValue {
        let Some(info) = blocks.info(block.state) else {
            return block.clone();
        };
        let name = info.name.as_str();
        let mut properties = info.properties.clone();

        // The four sides a fence joins in, turned as a set rather than one by one.
        // `transformState` does this first and on its own, because a block can have
        // all four properties at once and each value has to land on a different
        // property (`BlockTransformExtent.java:437-447`).
        //
        // Every value is read from the ORIGINAL state before any is written, and it is
        // written ONLY to the property of the direction it turned into - which is
        // what `transformState` does. Because a turn is a bijection of the four
        // sides, all four properties get overwritten and the source never needs
        // clearing. Writing to both is what put `east=false` on top of the `true`
        // that `north` had just landed there: north-and-west came back unchanged.
        let original = properties.clone();
        let mut moved: Vec<(&str, String)> = Vec::new();
        for side in Direction::SIDES {
            let Some(value) = property_value(&original, side.name()) else {
                continue;
            };
            let Some(target) = self.turn_direction(side).side() else {
                continue;
            };
            moved.push((target, value));
        }
        for (key, value) in moved {
            set_property(&mut properties, key, value);
        }

        // `axis` is the one property whose table depends on the block, so it is
        // resolved before the walk. The resolver does not report a property's value
        // list, so the three-value case is recognised the way the port already
        // recognises an unknown state: by asking whether `axis=z` resolves at all.
        let axis_values = self.axis_values(&properties, name, blocks);

        for (property, value) in properties.clone() {
            let Some(turned) = self.turn_property(&property, &value, &axis_values) else {
                continue;
            };
            set_property(&mut properties, &property, &turned);
        }

        let Some(state) = blocks.resolve(name, &properties) else {
            return block.clone();
        };

        let entity = match &block.entity {
            Some(entity) => self.turn_sign_entity(entity),
            None => None,
        };
        match entity {
            Some(entity) => BlockValue::with_entity(state, entity),
            None => BlockValue::new(state),
        }
    }

    /// The values `axis` can hold, which decides whether `y` means up-down or
    /// south-north.
    fn axis_values(
        &self,
        properties: &[(String, String)],
        name: &str,
        blocks: &dyn BlockResolver,
    ) -> Vec<String> {
        let axis = match property_value(properties, "axis") {
            Some(axis) => axis,
            None => return Vec::new(),
        };
        let mut probe = properties.to_vec();
        set_property(&mut probe, "axis", "z");
        if blocks.resolve(name, &probe).is_some() {
            vec!["x".to_string(), "y".to_string(), "z".to_string()]
        } else {
            // Two values, and the only one they can be is the pair FAWE uses for a
            // log: x is east-west and y is north-south.
            vec![axis.clone(), axis.clone()]
                .into_iter()
                .zip(["x", "y"])
                .map(|(_, value)| value.to_string())
                .collect()
        }
    }

    /// The value `property` holds after this transform, or `None` when FAWE does
    /// not rotate it.
    fn turn_property(&self, property: &str, value: &str, axis_values: &[String]) -> Option<String> {
        // `facing` is its own case: every value is exactly one direction, so the
        // answer is just the nearest direction the turn produced.
        if property == "facing" {
            let direction = Direction::from_name(value)?;
            let turned = self.turn_direction(direction);
            return Some(turned.name().to_string());
        }

        let groups = direction_groups(property, axis_values)?;
        let current = groups.iter().find(|(name, _)| name == value)?.1.clone();
        let wanted: Vec<Direction> = current
            .iter()
            .map(|direction| self.turn_direction(*direction))
            .collect();
        // A group's directions are a SET, so the order the answer arrives in does not
        // matter: `x` is east-then-west while a turn may answer west-then-east.
        groups
            .iter()
            .find(|(_, directions)| same_set(directions, &wanted))
            .map(|(name, _)| name.clone())
    }

    /// Where one direction ends up. A `Then` folds left to right, as
    /// `CombinedTransform#apply` does.
    fn turn_direction(&self, direction: Direction) -> Direction {
        Direction::closest(self.apply(direction.vector()))
    }

    /// A sign's `Rot` byte, which FAWE turns alongside the state so the two agree.
    ///
    /// `MCDirections#fromRotation` runs south at 0, west at 4, north at 8 and east
    /// at 12, in 22.5 degree steps, so the value is the angle rather than a table
    /// lookup - and a quarter turn sends `Rot 0`, which is south, to `4`, west.
    fn turn_sign_entity(&self, entity: &[u8]) -> Option<Box<[u8]>> {
        let mut compound = crate::nbt::read(entity).ok()?;
        let crate::nbt::Value::Byte(rotation) = *compound.get("Rot")? else {
            return None;
        };
        let angle = f64::from(rotation).rem_euclid(16.0) * 22.5_f64.to_radians();
        let turned = Vec3::new(-angle.sin(), 0.0, angle.cos());
        let landed = self.apply(turned);
        // Back to an index, measured the same way round: the vector is
        // (-sin t, 0, cos t), so t is atan2(-x, z).
        let stepped =
            (landed.x.atan2(landed.z) * -180.0 / std::f64::consts::PI / 22.5).rem_euclid(16.0);
        let turned_rotation = stepped.round() as i64 % 16;
        compound.insert(
            "Rot".to_string(),
            crate::nbt::Value::Byte(turned_rotation as i8),
        );
        Some(crate::nbt::write(&compound).into_boxed_slice())
    }
}

/// A direction a block state property can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Direction {
    North,
    East,
    South,
    West,
    Up,
    Down,
}

impl Direction {
    /// The four horizontal sides, in `Direction.VALUES` order.
    const SIDES: [Self; 4] = [Self::North, Self::East, Self::South, Self::West];

    /// `Direction#toVector`.
    fn vector(self) -> Vec3 {
        match self {
            Self::North => Vec3::new(0.0, 0.0, -1.0),
            Self::East => Vec3::new(1.0, 0.0, 0.0),
            Self::South => Vec3::new(0.0, 0.0, 1.0),
            Self::West => Vec3::new(-1.0, 0.0, 0.0),
            Self::Up => Vec3::new(0.0, 1.0, 0.0),
            Self::Down => Vec3::new(0.0, -1.0, 0.0),
        }
    }

    /// `Direction#findClosest`, over the six axis directions.
    ///
    /// `Direction.VALUES` runs down, up, north, south, west, east and
    /// `findClosest` keeps a strictly larger dot product, so the earlier of two
    /// tied directions wins. The order below is that one, which decides a vector
    /// sitting exactly between two faces.
    fn closest(vector: Vec3) -> Self {
        let mut best = (Self::Down, f64::NEG_INFINITY);
        for (direction, dot) in [
            (Self::Down, -vector.y),
            (Self::Up, vector.y),
            (Self::North, -vector.z),
            (Self::South, vector.z),
            (Self::West, -vector.x),
            (Self::East, vector.x),
        ] {
            if dot > best.1 {
                best = (direction, dot);
            }
        }
        best.0
    }

    /// The property name a side connection is stored under.
    const fn side(self) -> Option<&'static str> {
        match self {
            Self::North => Some("north"),
            Self::East => Some("east"),
            Self::South => Some("south"),
            Self::West => Some("west"),
            _ => None,
        }
    }

    /// The property name this direction is written as.
    const fn name(self) -> &'static str {
        match self {
            Self::North => "north",
            Self::East => "east",
            Self::South => "south",
            Self::West => "west",
            Self::Up => "up",
            Self::Down => "down",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "north" => Some(Self::North),
            "east" => Some(Self::East),
            "south" => Some(Self::South),
            "west" => Some(Self::West),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            _ => None,
        }
    }
}

/// The directions each value of `property` stands for.
///
/// The odd entries are the whole reason FAWE rotates directions rather than
/// indices. `axis` has three values on a pillar and two on a log, and on the two
/// it pairs `y` with south-north rather than up-down
/// (`BlockTransformExtent.java:186-193`) - so `//rotate 90` genuinely lays a
/// standing log down, which is surprising but correct.
fn direction_groups(
    property: &str,
    axis_values: &[String],
) -> Option<Vec<(String, Vec<Direction>)>> {
    let sides = Direction::SIDES.to_vec();
    let group = |name: &str, directions: Vec<Direction>| (name.to_string(), directions);
    match property {
        "half" => Some(vec![
            group("bottom", vec![Direction::Down]),
            group("top", vec![Direction::Up]),
        ]),
        "face" => Some(vec![
            group("up", vec![Direction::Up]),
            group("wall", sides),
            group("down", vec![Direction::Down]),
        ]),
        "hinge" => Some(vec![
            group(
                "left",
                vec![
                    Direction::North,
                    Direction::East,
                    Direction::South,
                    Direction::West,
                ],
            ),
            group(
                "right",
                vec![
                    Direction::North,
                    Direction::East,
                    Direction::South,
                    Direction::West,
                ],
            ),
        ]),
        "axis" => match axis_values.len() {
            2 => Some(vec![
                group("x", vec![Direction::East, Direction::West]),
                group("y", vec![Direction::South, Direction::North]),
            ]),
            3 => Some(vec![
                group("x", vec![Direction::East, Direction::West]),
                group("y", vec![Direction::Up, Direction::Down]),
                group("z", vec![Direction::South, Direction::North]),
            ]),
            _ => None,
        },
        _ => None,
    }
}

/// Whether two direction lists hold the same directions, in any order.
fn same_set(left: &[Direction], right: &[Direction]) -> bool {
    let mut left: Vec<Direction> = left.to_vec();
    let mut right: Vec<Direction> = right.to_vec();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}

/// The value of `name` in a property list.
fn property_value(properties: &[(String, String)], name: &str) -> Option<String> {
    properties
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

/// Replaces a property's value, keeping the position it was written at so the
/// resolved state still has the block's own property order.
fn set_property(properties: &mut Vec<(String, String)>, name: &str, value: impl Into<String>) {
    let value = value.into();
    match properties.iter_mut().find(|(key, _)| key == name) {
        Some(entry) => entry.1 = value,
        None => properties.push((name.to_string(), value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockStateInfo;

    /// A quarter turn has to move the clipboard's own axes, not the world's, or
    /// `//rotate 90` before `//paste` lands the copy beside itself.
    #[test]
    fn a_quarter_turn_moves_the_copy_about_the_clipboard_origin() {
        let pivot = BlockPos::new(10, 64, 10);
        // `//rotate 90` becomes `rotateY(-90)`, which is `x' = x*cos - z*sin` and
        // `z' = x*sin + z*cos` at theta = -90. So +X swings to +Z.
        let transform = Transform::RotateY(-90.0);

        assert_eq!(
            transform.paste_position(BlockPos::new(11, 64, 10), pivot),
            BlockPos::new(10, 64, 11)
        );
        assert_eq!(
            transform.paste_position(BlockPos::new(10, 64, 11), pivot),
            BlockPos::new(9, 64, 10)
        );
        assert_eq!(
            transform.paste_position(BlockPos::new(10, 70, 10), pivot),
            BlockPos::new(10, 70, 10),
            "rotation must not touch Y"
        );
    }

    /// Four quarter turns land back where they started, so a build that stacks
    /// four `//rotate 90` calls must not shift the copy.
    #[test]
    fn four_quarter_turns_are_the_identity() {
        let pivot = BlockPos::new(0, 0, 0);
        let transform = Transform::RotateY(-90.0)
            .then(Transform::RotateY(-90.0))
            .then(Transform::RotateY(-90.0))
            .then(Transform::RotateY(-90.0));
        assert_eq!(
            transform.paste_position(BlockPos::new(7, 0, -3), pivot),
            BlockPos::new(7, 0, -3)
        );
        assert!(!transform.is_identity());
    }

    #[test]
    fn a_full_turn_is_recognised_as_the_identity() {
        assert!(Transform::RotateY(360.0).is_identity());
        assert!(Transform::RotateY(-720.0).is_identity());
        assert!(!Transform::RotateY(90.0).is_identity());
    }

    #[test]
    fn a_flip_mirrors_along_one_axis() {
        let pivot = BlockPos::new(0, 0, 0);
        assert_eq!(
            Transform::Flip(BlockAxis::X).paste_position(BlockPos::new(3, 0, 0), pivot),
            BlockPos::new(-3, 0, 0)
        );
        assert_eq!(
            Transform::Flip(BlockAxis::Z).paste_position(BlockPos::new(3, 0, 0), pivot),
            BlockPos::new(3, 0, 0)
        );
    }

    /// Rotation is about the pivot, so a block at the pivot never moves however
    /// many transforms are stacked.
    #[test]
    fn the_pivot_itself_never_moves() {
        let pivot = BlockPos::new(5, 5, 5);
        let transform = Transform::RotateY(45.0)
            .then(Transform::Flip(BlockAxis::Y))
            .then(Transform::RotateY(-135.0));
        assert_eq!(transform.paste_position(pivot, pivot), pivot);
    }

    /// One staircase state: the three properties a turn touches and the one it
    /// must not.
    struct Stair {
        facing: &'static str,
        half: &'static str,
        shape: &'static str,
        waterlogged: bool,
    }

    /// The staircase states these tests name. A real stair has forty, which
    /// would be forty lines of table and no more coverage than these five give.
    const STAIRS: &[Stair] = &[
        Stair {
            facing: "north",
            half: "bottom",
            shape: "straight",
            waterlogged: false,
        },
        Stair {
            facing: "east",
            half: "bottom",
            shape: "straight",
            waterlogged: false,
        },
        Stair {
            facing: "south",
            half: "bottom",
            shape: "straight",
            waterlogged: false,
        },
        Stair {
            facing: "west",
            half: "bottom",
            shape: "straight",
            waterlogged: false,
        },
        Stair {
            facing: "north",
            half: "top",
            shape: "outer_left",
            waterlogged: true,
        },
    ];

    /// A fence's four sides, in the order the properties are named.
    const FENCES: &[(bool, bool, bool, bool)] = &[
        (false, false, false, false),
        // North and west, and the same fence after a quarter turn.
        (true, false, false, true),
        (true, true, false, false),
    ];

    /// A sign's sixteen rotations, as the `rotation` property names them.
    const SIGN_ROTATIONS: &[&str] = &[
        "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15",
    ];

    /// A log's `axis`, which is x or y and never z.
    const LOG_AXES: &[&str] = &["x", "y"];

    const STAIRS_ID: u16 = 0;
    const FENCE_OFFSET: u16 = STAIRS_ID + STAIRS.len() as u16;
    const SIGN_OFFSET: u16 = FENCE_OFFSET + FENCES.len() as u16;
    const LOG_OFFSET: u16 = SIGN_OFFSET + SIGN_ROTATIONS.len() as u16;

    /// Whether a request pins `key` to `value`. A property it does not mention
    /// is not a mismatch, because a pattern may name only some of a block's.
    fn pins(properties: &[(String, String)], key: &str, value: &str) -> bool {
        properties
            .iter()
            .all(|(name, given)| name != key || given == value)
    }

    fn boolean(value: bool) -> &'static str {
        if value { "true" } else { "false" }
    }

    /// A registry over the tables above, with the property names the server
    /// uses, so a turn has to go through the same name-value pairs a real paste
    /// would.
    struct TestBlocks;

    impl BlockResolver for TestBlocks {
        fn resolve(&self, name: &str, properties: &[(String, String)]) -> Option<u16> {
            match name {
                "minecraft:oak_stairs" => STAIRS
                    .iter()
                    .position(|stair| {
                        pins(properties, "facing", stair.facing)
                            && pins(properties, "half", stair.half)
                            && pins(properties, "shape", stair.shape)
                            && pins(properties, "waterlogged", boolean(stair.waterlogged))
                    })
                    .map(|index| STAIRS_ID + index as u16),
                "minecraft:oak_fence" => FENCES
                    .iter()
                    .position(|(north, east, south, west)| {
                        pins(properties, "north", boolean(*north))
                            && pins(properties, "east", boolean(*east))
                            && pins(properties, "south", boolean(*south))
                            && pins(properties, "west", boolean(*west))
                    })
                    .map(|index| FENCE_OFFSET + index as u16),
                "minecraft:oak_sign" => SIGN_ROTATIONS
                    .iter()
                    .position(|rotation| {
                        pins(properties, "rotation", rotation)
                            && pins(properties, "waterlogged", "false")
                    })
                    .map(|index| SIGN_OFFSET + index as u16),
                "minecraft:oak_log" => LOG_AXES
                    .iter()
                    .position(|axis| pins(properties, "axis", axis))
                    .map(|index| LOG_OFFSET + index as u16),
                _ => None,
            }
        }

        fn info(&self, state: u16) -> Option<BlockStateInfo> {
            let properties = |name: &str, pairs: Vec<(&str, String)>| {
                Some(BlockStateInfo {
                    name: name.to_string(),
                    properties: pairs
                        .into_iter()
                        .map(|(key, value)| (key.to_string(), value))
                        .collect(),
                })
            };
            if let Some(stair) = STAIRS.get(state as usize) {
                return properties(
                    "minecraft:oak_stairs",
                    vec![
                        ("facing", stair.facing.to_string()),
                        ("half", stair.half.to_string()),
                        ("shape", stair.shape.to_string()),
                        ("waterlogged", boolean(stair.waterlogged).to_string()),
                    ],
                );
            }
            if let Some((north, east, south, west)) = state
                .checked_sub(FENCE_OFFSET)
                .and_then(|index| FENCES.get(index as usize))
            {
                return properties(
                    "minecraft:oak_fence",
                    vec![
                        ("north", boolean(*north).to_string()),
                        ("east", boolean(*east).to_string()),
                        ("south", boolean(*south).to_string()),
                        ("west", boolean(*west).to_string()),
                    ],
                );
            }
            if let Some(rotation) = state
                .checked_sub(SIGN_OFFSET)
                .and_then(|index| SIGN_ROTATIONS.get(index as usize))
            {
                return properties(
                    "minecraft:oak_sign",
                    vec![
                        ("rotation", (*rotation).to_string()),
                        ("waterlogged", "false".to_string()),
                    ],
                );
            }
            let axis = state
                .checked_sub(LOG_OFFSET)
                .and_then(|index| LOG_AXES.get(index as usize))?;
            properties("minecraft:oak_log", vec![("axis", (*axis).to_string())])
        }

        fn is_air(&self, _state: u16) -> bool {
            false
        }

        fn is_solid(&self, _state: u16) -> bool {
            true
        }

        fn is_liquid(&self, _state: u16) -> bool {
            false
        }

        fn block_name(&self, _state: u16) -> Option<&str> {
            Some("minecraft:oak_stairs")
        }
    }

    /// The value of `key` on `state`, for the assertions below.
    fn property(state: u16, key: &str) -> Option<String> {
        TestBlocks
            .info(state)?
            .properties
            .into_iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// `//rotate 90`: a quarter turn clockwise, which is `rotateY(-90)` because
    /// a positive yaw already turns clockwise.
    const QUARTER_TURN: Transform = Transform::RotateY(-90.0);

    /// A turn has to reach the block as well as the position, or a rotated copy
    /// is a sideways copy of the wrong way round. This is the whole point of
    /// `BlockTransformExtent`: the stair's `facing` follows the copy.
    #[test]
    fn a_quarter_turn_turns_a_stairs_facing() {
        let block = BlockValue::new(STAIRS_ID);

        let turned = QUARTER_TURN.transform_block(&block, &TestBlocks);

        assert_eq!(
            property(turned.state, "facing").as_deref(),
            Some("east"),
            "//rotate 90 sends north to east, as Direction.rotateY(-90) does"
        );
        assert_ne!(turned.state, block.state, "the state id has to change");
        assert_eq!(
            property(turned.state, "half").as_deref(),
            Some("bottom"),
            "a turn about Y does not move the half"
        );
        assert_eq!(
            property(turned.state, "shape").as_deref(),
            Some("straight"),
            "a turn about Y does not move the shape"
        );
        assert_eq!(
            property(turned.state, "waterlogged").as_deref(),
            Some("false"),
            "a property with no direction stays as it was"
        );
    }

    /// All four of them, because one is a special case: north and west are the
    /// ones a quarter turn sends to the two directions either side.
    #[test]
    fn a_quarter_turn_turns_every_facing() {
        for (facing, turned_facing) in [
            ("north", "east"),
            ("east", "south"),
            ("south", "west"),
            ("west", "north"),
        ] {
            let block = BlockValue::new(
                TestBlocks
                    .resolve(
                        "minecraft:oak_stairs",
                        &[
                            ("facing".to_string(), facing.to_string()),
                            ("half".to_string(), "bottom".to_string()),
                            ("shape".to_string(), "straight".to_string()),
                            ("waterlogged".to_string(), "false".to_string()),
                        ],
                    )
                    .expect("the test registry has this stair"),
            );

            let turned = QUARTER_TURN.transform_block(&block, &TestBlocks);

            assert_eq!(
                property(turned.state, "facing").as_deref(),
                Some(turned_facing),
                "facing={facing} should come out facing {turned_facing}"
            );
        }
    }

    /// `//flip` mirrors the copy, so the state has to be mirrored with it. A
    /// player looking north flips Z, which swaps north with south; a player
    /// looking east flips X, which swaps east with west.
    #[test]
    fn a_flip_mirrors_a_stairs_facing() {
        let north = BlockValue::new(STAIRS_ID);
        let east = BlockValue::new(STAIRS_ID + 1);

        assert_eq!(
            property(
                Transform::Flip(BlockAxis::Z)
                    .transform_block(&north, &TestBlocks)
                    .state,
                "facing"
            )
            .as_deref(),
            Some("south"),
            "a mirror in Z swaps north and south"
        );
        assert_eq!(
            property(
                Transform::Flip(BlockAxis::X)
                    .transform_block(&east, &TestBlocks)
                    .state,
                "facing"
            )
            .as_deref(),
            Some("west"),
            "a mirror in X swaps east and west"
        );
        assert_eq!(
            property(
                Transform::Flip(BlockAxis::Z)
                    .transform_block(&north, &TestBlocks)
                    .state,
                "half"
            )
            .as_deref(),
            Some("bottom"),
            "a mirror does not turn the stair over"
        );
    }

    /// A state the server cannot resolve is a state it does not have, so the
    /// block has to come out as it went in. Failing the paste instead would
    /// turn one unknown state into a whole missing build.
    #[test]
    fn a_property_the_server_will_not_resolve_is_left_alone() {
        let block = BlockValue::new(STAIRS_ID);

        let turned = QUARTER_TURN.transform_block(&block, &NorthFacingOnly);

        assert_eq!(
            turned, block,
            "a stair whose turned state does not resolve keeps the state it had"
        );
        assert_eq!(turned.state, block.state);
    }

    /// The same for a state the registry knows nothing about at all.
    #[test]
    fn a_state_the_registry_does_not_know_is_left_alone() {
        let block = BlockValue::new(STAIRS_ID);

        assert_eq!(QUARTER_TURN.transform_block(&block, &NoBlocks), block);
    }

    /// A fence does not name a direction: it says which of its four sides reach
    /// a neighbour, and those have to be turned as a set. A fence joining north
    /// and west faces the same way after a quarter turn as one joining east and
    /// north does today.
    #[test]
    fn a_quarter_turn_carries_a_fences_connections_round() {
        let fence = BlockValue::new(FENCE_OFFSET + 1);

        let turned = QUARTER_TURN.transform_block(&fence, &TestBlocks);

        assert_eq!(
            turned.state,
            FENCE_OFFSET + 2,
            "north and west should come out as east and north"
        );
    }

    /// `//rotate 90` is `rotateY(-90)`, and its `axis` is the one property where
    /// a two-value table differs from a three-value one: FAWE pairs a log's `y`
    /// with north-south rather than up-down, so a standing log ends up lying
    /// down. A block's value list is not something the resolver reports, so the
    /// one value the two tables disagree about is settled by asking the server
    /// whether the block has an `axis=z` state at all.
    #[test]
    fn a_logs_two_wide_axis_follows_the_turn() {
        let log = BlockValue::new(
            TestBlocks
                .resolve(
                    "minecraft:oak_log",
                    &[("axis".to_string(), "y".to_string())],
                )
                .expect("the log is in the test registry"),
        );

        let turned = QUARTER_TURN.transform_block(&log, &TestBlocks);

        assert_eq!(
            property(turned.state, "axis").as_deref(),
            Some("x"),
            "a two-value axis reads y as north-south, so a quarter turn makes it x"
        );
    }

    /// A sign's `Rot` byte is turned alongside the state.
    ///
    /// FAWE does turn it (`transformBaseBlockNBT`, `BlockTransformExtent.java:403`),
    /// and `MCDirections#fromRotation` runs south at 0, west at 4, north at 8, east
    /// at 12 - so `Rot 0` is south and a quarter turn sends it to west.
    ///
    /// The `rotation` *property* is deliberately not turned. FAWE reaches it through
    /// `Direction#fromRotationIndex` (`:181`), which is neither in FAWE's tree nor in
    /// Pumpkin's, so the mapping is not verifiable from any source available here and
    /// a guess would turn every sign in a copied region the wrong way while passing
    /// its own test. `BlockTransformExtent` rotates the byte and the property
    /// independently, so turning one without the other is visibly inconsistent in
    /// game - which is the honest state to be in until the other can be checked.
    #[test]
    fn a_quarter_turn_turns_a_signs_rotation_byte() {
        let sign = BlockValue::with_entity(
            TestBlocks
                .resolve(
                    "minecraft:oak_sign",
                    &[
                        ("rotation".to_string(), "0".to_string()),
                        ("waterlogged".to_string(), "false".to_string()),
                    ],
                )
                .expect("the sign is in the test registry"),
            sign_with_rotation(0),
        );

        let turned = QUARTER_TURN.transform_block(&sign, &TestBlocks);

        assert_eq!(
            rotation_of(turned.entity.as_deref().expect("the sign keeps its nbt")),
            Some(4),
            "Rot 0 is south, and a quarter turn sends south to west"
        );
        assert_eq!(
            property(turned.state, "rotation").as_deref(),
            Some("0"),
            "the rotation property is not turned yet, and this pins that we know it"
        );
    }

    /// The `Rot` byte of a sign's block entity NBT.
    fn rotation_of(entity: &[u8]) -> Option<i8> {
        match crate::nbt::read(entity).ok()?.get("Rot")? {
            crate::nbt::Value::Byte(rotation) => Some(*rotation),
            _ => None,
        }
    }

    /// A sign's block entity NBT, with the text and the rotation a real one has.
    fn sign_with_rotation(rotation: i8) -> Box<[u8]> {
        crate::nbt::write(&crate::nbt::Compound::from([
            (
                "Text".to_string(),
                crate::nbt::Value::String("hello".to_string()),
            ),
            ("Rot".to_string(), crate::nbt::Value::Byte(rotation)),
        ]))
        .into_boxed_slice()
    }

    /// A registry holding one state and refusing to resolve any other, standing
    /// in for a server that does not have what a turn asks for.
    struct NorthFacingOnly;

    impl BlockResolver for NorthFacingOnly {
        fn resolve(&self, name: &str, properties: &[(String, String)]) -> Option<u16> {
            if name != "minecraft:oak_stairs" {
                return None;
            }
            let stair = &STAIRS[0];
            (pins(properties, "facing", stair.facing)
                && pins(properties, "half", stair.half)
                && pins(properties, "shape", stair.shape)
                && pins(properties, "waterlogged", boolean(stair.waterlogged)))
            .then_some(STAIRS_ID)
        }

        fn info(&self, state: u16) -> Option<BlockStateInfo> {
            TestBlocks.info(state)
        }

        fn is_air(&self, _state: u16) -> bool {
            false
        }

        fn is_solid(&self, _state: u16) -> bool {
            true
        }

        fn is_liquid(&self, _state: u16) -> bool {
            false
        }

        fn block_name(&self, _state: u16) -> Option<&str> {
            Some("minecraft:oak_stairs")
        }
    }

    /// A registry that has resolved nothing, so every state is unknown to it.
    struct NoBlocks;

    impl BlockResolver for NoBlocks {
        fn resolve(&self, _name: &str, _properties: &[(String, String)]) -> Option<u16> {
            None
        }

        fn info(&self, _state: u16) -> Option<BlockStateInfo> {
            None
        }

        fn is_air(&self, _state: u16) -> bool {
            false
        }

        fn is_solid(&self, _state: u16) -> bool {
            false
        }

        fn is_liquid(&self, _state: u16) -> bool {
            false
        }

        fn block_name(&self, _state: u16) -> Option<&str> {
            None
        }
    }
}
