//! The generated shapes: spheres, cylinders and lines.
//!
//! These do not go through [`crate::region`]. `//sphere` calls
//! `EditSession.makeSphere` and `//sel ellipsoid` builds an `EllipsoidRegion`, and
//! the two round radii differently — `makeSphere` adds `0.5` and then tests the
//! loop variables, while `EllipsoidRegion` stores the radius with `0.5` added and
//! tests block offsets. A shape that agrees with neither would be wrong for one
//! command and right for the other, so each keeps its own arithmetic.
//!
//! Every generator returns positions rather than writing blocks, because the edit
//! job needs to be able to stop part-way and resume, and because the shape
//! commands have to be able to report how many blocks they covered.

use std::collections::{HashSet, VecDeque};

use crate::{
    math::{BlockPos, math_round},
    region::Region,
};

/// The bounds a generated shape is clipped to, which is what stops `//sphere` at
/// the world ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Lowest buildable Y.
    pub min_y: i32,
    /// Highest buildable Y.
    pub max_y: i32,
}

impl Bounds {
    /// Whether `position` is inside the world vertically.
    ///
    /// A shape can be clipped by simply never generating the positions that are
    /// outside, but a command that computes one — `//overlay`'s `+1`, a `//fill`'s
    /// step, a `//curve`'s control point — has to ask. X and Z are not checked: the
    /// world is effectively infinite in both, so the only bound that exists is this one.
    #[must_use]
    pub fn holds(&self, position: BlockPos) -> bool {
        (self.min_y..=self.max_y).contains(&position.y)
    }
}

/// A filled or hollow ellipsoid centred on `center`.
///
/// This is `EditSession.makeSphere`: each radius has `0.5` added, and a point is
/// inside when its normalised offsets sum to at most one. A hollow sphere keeps
/// the shell, which is every block with at least one neighbour outside.
#[must_use]
pub fn sphere(
    center: BlockPos,
    radius: (f64, f64, f64),
    hollow: bool,
    bounds: Bounds,
) -> Vec<BlockPos> {
    let (rx, ry, rz) = (radius.0 + 0.5, radius.1 + 0.5, radius.2 + 0.5);
    let (inv_x, inv_y, inv_z) = (1.0 / rx, 1.0 / ry, 1.0 / rz);
    let ceil_x = rx.ceil() as i32;
    let ceil_y = ry.ceil() as i32;
    let ceil_z = rz.ceil() as i32;

    let mut positions = Vec::new();
    let mut next_x = 0.0;
    'x: for x in 0..=ceil_x {
        let xn = next_x;
        let dx = xn * xn;
        next_x = f64::from(x + 1) * inv_x;
        let next_x_squared = next_x * next_x;

        let mut next_z = 0.0;
        'z: for z in 0..=ceil_z {
            let zn = next_z;
            let dz = zn * zn;
            let dxz = dx + dz;
            next_z = f64::from(z + 1) * inv_z;
            let next_z_squared = next_z * next_z;

            let mut next_y = 0.0;
            'y: for y in 0..=ceil_y {
                let yn = next_y;
                let dy = yn * yn;
                let dxyz = dxz + dy;
                next_y = f64::from(y + 1) * inv_y;

                if dxyz > 1.0 {
                    if y == 0 {
                        if z == 0 {
                            // Nothing further out can be inside either.
                            break 'x;
                        }
                        break 'z;
                    }
                    break 'y;
                }

                let next_y_squared = next_y * next_y;
                if hollow
                    && next_x_squared + dy + dz <= 1.0
                    && next_y_squared + dxz <= 1.0
                    && next_z_squared + dx + dy <= 1.0
                {
                    // Every neighbour is inside, so this is interior.
                    continue 'y;
                }

                // Inside-ness is monotone in each axis, so testing the block one
                // step further out in X, Y and Z covers both signs.
                let plus_y = center.y + y;
                let minus_y = center.y - y;
                let mirror_x = x != 0;
                let mirror_z = z != 0;

                if plus_y <= bounds.max_y {
                    positions.push(BlockPos::new(center.x + x, plus_y, center.z + z));
                    if mirror_x {
                        positions.push(BlockPos::new(center.x - x, plus_y, center.z + z));
                    }
                    if mirror_z {
                        positions.push(BlockPos::new(center.x + x, plus_y, center.z - z));
                    }
                    if mirror_x && mirror_z {
                        positions.push(BlockPos::new(center.x - x, plus_y, center.z - z));
                    }
                }
                if y != 0 && minus_y >= bounds.min_y {
                    positions.push(BlockPos::new(center.x + x, minus_y, center.z + z));
                    if mirror_x {
                        positions.push(BlockPos::new(center.x - x, minus_y, center.z + z));
                    }
                    if mirror_z {
                        positions.push(BlockPos::new(center.x + x, minus_y, center.z - z));
                    }
                    if mirror_x && mirror_z {
                        positions.push(BlockPos::new(center.x - x, minus_y, center.z - z));
                    }
                }
            }
        }
    }
    positions
}

/// A cylinder centred on `center`, `height` blocks tall upwards.
///
/// This is `EditSession.makeCylinder`. Each radius has `0.5` added. A negative
/// height extends downwards instead, which moves the base rather than the top.
#[must_use]
pub fn cylinder(
    center: BlockPos,
    radius: (f64, f64),
    height: i32,
    hollow: bool,
    thickness: f64,
    bounds: Bounds,
) -> Vec<BlockPos> {
    if height == 0 {
        return Vec::new();
    }
    let (rx, rz) = (radius.0 + 0.5, radius.1 + 0.5);
    let (inv_x, inv_z) = (1.0 / rx, 1.0 / rz);

    let (mut base, mut height) = if height < 0 {
        (center.y + height, -height)
    } else {
        (center.y, height)
    };
    // Clip the shape into the world, growing it to the ceiling if it would
    // overshoot, which is what `makeCylinder` does before it loops.
    if base < bounds.min_y {
        base = bounds.min_y;
    } else if base + height - 1 > bounds.max_y {
        height = bounds.max_y - base + 1;
    }
    if height <= 0 {
        return Vec::new();
    }

    let ceil_x = rx.ceil() as i32;
    let ceil_z = rz.ceil() as i32;

    // A non-zero thickness generates the band between the outer radius and the
    // outer radius less the thickness, which is `//hcyl`'s fourth argument. The
    // two radii are normalised separately, exactly as `makeCylinder`'s
    // `thickness != 0` branch does with its `minInvRadius*` pair.
    //
    // `makeCylinder` divides by `radius - thickness` without checking it
    // (EditSession.java:2275-2276), so a band as thick as the cylinder gets an inverse
    // of 2 and never skips anything, and a thicker one gets a negative inverse that the
    // squaring on the next iteration turns back into a large number. Both fill the
    // cylinder. No guard belongs here: there is no thickness that means "nothing".
    let inner = if thickness > 0.0 {
        Some((1.0 / (rx - thickness), 1.0 / (rz - thickness)))
    } else {
        None
    };

    let mut positions = Vec::new();
    let mut next_x = 0.0;
    // Tracks `(x) / innerRadius` before it is stepped, and after, because the
    // band test needs one of each.
    let mut next_inner_x = 0.0;
    'x: for x in 0..=ceil_x {
        let xn = next_x;
        let x_squared = xn * xn;
        next_x = f64::from(x + 1) * inv_x;

        let inner_x_squared = match inner {
            Some((inv_inner_x, _)) => {
                let value = next_inner_x * next_inner_x;
                next_inner_x = f64::from(x + 1) * inv_inner_x;
                value
            }
            None => 0.0,
        };
        let stepped_inner_x = match inner {
            Some((inv_inner_x, _)) => f64::from(x + 1) * inv_inner_x,
            None => 0.0,
        };

        let mut next_z = 0.0;
        let mut next_inner_z = 0.0;
        'z: for z in 0..=ceil_z {
            let zn = next_z;
            let z_squared = zn * zn;
            if x_squared + z_squared > 1.0 {
                if z == 0 {
                    break 'x;
                }
                break 'z;
            }
            let inner_z_squared = match inner {
                Some((_, inv_inner_z)) => {
                    let value = next_inner_z * next_inner_z;
                    next_inner_z = f64::from(z + 1) * inv_inner_z;
                    value
                }
                None => 0.0,
            };
            next_z = f64::from(z + 1) * inv_z;
            let next_z_squared = next_z * next_z;
            let next_inner_z_squared = match inner {
                Some((_, inv_inner_z)) => {
                    f64::from(z + 1) * inv_inner_z * f64::from(z + 1) * inv_inner_z
                }
                None => 0.0,
            };

            // Skip a block when both horizontal neighbours are still inside the
            // inner region: the block is interior, so it is not part of the band.
            let interior = match inner {
                Some(_) => {
                    inner_z_squared + stepped_inner_x * stepped_inner_x <= 1.0
                        && next_inner_z_squared + inner_x_squared <= 1.0
                }
                // Without a thickness the shell is one block deep, tested against
                // the outer radius.
                None if hollow => {
                    z_squared + next_x * next_x <= 1.0 && next_z_squared + x_squared <= 1.0
                }
                None => false,
            };
            if interior {
                continue 'z;
            }

            for y in 0..height {
                let y = base + y;
                positions.push(BlockPos::new(center.x + x, y, center.z + z));
                if x != 0 {
                    positions.push(BlockPos::new(center.x - x, y, center.z + z));
                }
                if z != 0 {
                    positions.push(BlockPos::new(center.x + x, y, center.z - z));
                }
                if x != 0 && z != 0 {
                    positions.push(BlockPos::new(center.x - x, y, center.z - z));
                }
            }
        }
    }
    positions
}

/// The blocks of a straight line from `start` to `end`.
///
/// This is `EditSession.drawLine`: it walks the dominant axis and rounds the other
/// two proportionally, which is why the line looks slightly different from a
/// Bresenham line in the y direction. `radius` inflates the line into a tube,
/// `hollow` keeps only the shell of that tube, and `flat` inflates in the
/// horizontal plane only.
#[must_use]
pub fn line(
    start: BlockPos,
    end: BlockPos,
    radius: f64,
    hollow: bool,
    flat: bool,
) -> Vec<BlockPos> {
    let base = line_core(start, end);
    let inflated = if radius >= 1.0 {
        if flat {
            inflate_flat(&base, radius)
        } else {
            inflate(&base, radius)
        }
    } else {
        base
    };
    if !hollow {
        return inflated;
    }
    if flat {
        outline(&inflated)
    } else {
        shell(&inflated)
    }
}

/// The un-inflated line, deduplicated.
fn line_core(start: BlockPos, end: BlockPos) -> Vec<BlockPos> {
    let dx = (end.x - start.x).abs();
    let dy = (end.y - start.y).abs();
    let dz = (end.z - start.z).abs();
    let mut positions = Vec::new();

    if dx + dy + dz == 0 {
        positions.push(start);
        return positions;
    }
    let longest = dx.max(dy).max(dz);

    let step_x = (end.x - start.x).signum();
    let step_y = (end.y - start.y).signum();
    let step_z = (end.z - start.z).signum();

    // The dominant axis is stepped exactly; the others are rounded against it.
    // `longest` is one of the three differences, so it is never zero here.
    if longest == dx {
        for step in 0..=dx {
            let x = start.x + step * step_x;
            let y = f64::from(start.y)
                + f64::from(step) * f64::from(dy) / f64::from(dx) * f64::from(step_y);
            let z = f64::from(start.z)
                + f64::from(step) * f64::from(dz) / f64::from(dx) * f64::from(step_z);
            positions.push(BlockPos::new(x, math_round(y), math_round(z)));
        }
    } else if longest == dy {
        for step in 0..=dy {
            let y = start.y + step * step_y;
            let x = f64::from(start.x)
                + f64::from(step) * f64::from(dx) / f64::from(dy) * f64::from(step_x);
            let z = f64::from(start.z)
                + f64::from(step) * f64::from(dz) / f64::from(dy) * f64::from(step_z);
            positions.push(BlockPos::new(math_round(x), y, math_round(z)));
        }
    } else {
        for step in 0..=dz {
            let z = start.z + step * step_z;
            let y = f64::from(start.y)
                + f64::from(step) * f64::from(dy) / f64::from(dz) * f64::from(step_y);
            let x = f64::from(start.x)
                + f64::from(step) * f64::from(dx) / f64::from(dz) * f64::from(step_x);
            positions.push(BlockPos::new(math_round(x), math_round(y), z));
        }
    }
    dedupe(positions)
}

/// Grows every block into a sphere of the given radius, which is `getBallooned`.
fn inflate(positions: &[BlockPos], radius: f64) -> Vec<BlockPos> {
    let reach = radius.ceil() as i32;
    let mut result = Vec::with_capacity(positions.len());
    for position in positions {
        for dx in -reach..=reach {
            for dy in -reach..=reach {
                for dz in -reach..=reach {
                    let distance = f64::from(dx * dx + dy * dy + dz * dz).sqrt();
                    if distance <= radius {
                        result.push(position.offset(dx, dy, dz));
                    }
                }
            }
        }
    }
    dedupe(result)
}

/// Grows every block in the horizontal plane only, which is `getStretched`.
fn inflate_flat(positions: &[BlockPos], radius: f64) -> Vec<BlockPos> {
    let reach = radius.ceil() as i32;
    let mut result = Vec::with_capacity(positions.len());
    for position in positions {
        for dx in -reach..=reach {
            for dz in -reach..=reach {
                let distance = f64::from(dx * dx + dz * dz).sqrt();
                if distance <= radius {
                    result.push(position.offset(dx, 0, dz));
                }
            }
        }
    }
    dedupe(result)
}

/// Keeps the blocks with at least one horizontal neighbour outside, which is
/// `getOutline`.
fn outline(positions: &[BlockPos]) -> Vec<BlockPos> {
    let set: HashSet<BlockPos> = positions.iter().copied().collect();
    positions
        .iter()
        .copied()
        .filter(|position| {
            !HORIZONTAL_NEIGHBOURS
                .iter()
                .all(|(dx, dz)| set.contains(&position.offset(*dx, 0, *dz)))
        })
        .collect()
}

/// Keeps the blocks with at least one of their six neighbours outside, which is
/// `getHollowed`.
fn shell(positions: &[BlockPos]) -> Vec<BlockPos> {
    let set: HashSet<BlockPos> = positions.iter().copied().collect();
    positions
        .iter()
        .copied()
        .filter(|position| {
            !NEIGHBOURS
                .iter()
                .all(|(dx, dy, dz)| set.contains(&position.offset(*dx, *dy, *dz)))
        })
        .collect()
}

const NEIGHBOURS: [(i32, i32, i32); 6] = [
    (1, 0, 0),
    (-1, 0, 0),
    (0, 1, 0),
    (0, -1, 0),
    (0, 0, 1),
    (0, 0, -1),
];

const HORIZONTAL_NEIGHBOURS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

/// Removes duplicates while keeping the original order.
fn dedupe(positions: Vec<BlockPos>) -> Vec<BlockPos> {
    let mut seen = HashSet::with_capacity(positions.len());
    positions
        .into_iter()
        .filter(|position| seen.insert(*position))
        .collect()
}

/// Whether any face neighbour of `position` is in `outside`.
///
/// The six are `EditSession.recurseDirections` (EditSession.java:3982-3989); order does
/// not matter to a test that only asks whether any of them matches.
fn touches_outside(position: BlockPos, outside: &HashSet<BlockPos>) -> bool {
    NEIGHBOURS
        .iter()
        .any(|(dx, dy, dz)| outside.contains(&position.offset(*dx, *dy, *dz)))
}

/// Floods air in from `origin`, the way `EditSession.recurseHollow` does.
///
/// A block the mask rejects is neither recorded nor expanded from, which is why a world
/// of solid blocks leaves the flood nowhere to start. A block is recorded *before* the
/// region is checked, so the set ends up holding one ring of blocks outside the region
/// too — and that ring is what keeps the selection's own surface alive in
/// [`hollow_out`].
fn recurse_hollow(
    origin: BlockPos,
    region: &crate::region::CuboidRegion,
    outside: &mut HashSet<BlockPos>,
    solid: &impl Fn(BlockPos) -> bool,
) {
    let mut queue = VecDeque::from([origin]);
    while let Some(current) = queue.pop_front() {
        if solid(current) {
            continue;
        }
        if !outside.insert(current) {
            continue;
        }
        if !region.contains(current) {
            continue;
        }
        for (dx, dy, dz) in NEIGHBOURS {
            queue.push_back(current.offset(dx, dy, dz));
        }
    }
}

/// The positions of a region that `//hollow <thickness> [pattern]` replaces.
///
/// This is `EditSession.hollowOutRegion` (EditSession.java:3338-3420). Air is flooded in
/// from all six faces of the selection's bounding box, stopping at anything `solid`
/// reports as blocking movement, and then grown one further layer inwards per remaining
/// thickness. A position is replaced when *none* of its six neighbours was reached.
///
/// Two consequences of that, neither of which a distance from the boundary could have
/// given:
///
/// * a selection made entirely of movement-blocking blocks is replaced in full at every
///   thickness, because every seed is refused and nothing is ever reached; and
/// * a thickness of 0 is a thickness of 1, because the growing loop is
///   `for (int i = 1; i < thickness; ++i)` and runs for neither.
///
/// `solid` stands in for `SolidBlockMask`, which is what `//hollow` uses when no `-m` is
/// given (RegionCommands.java:867-869) and which tests movement-blocking blocks
/// (SolidBlockMask.java:34).
///
/// A `Region` rather than a cuboid: FAWE takes any selection, and its own javadoc calls
/// the result semi-well-defined for anything else. The flood only walks the region, so
/// a non-cuboid would have to answer `contains`; this takes a [`crate::region::CuboidRegion`]
/// because that is the only kind of selection this port hands `//hollow`.
#[must_use]
pub fn hollow_out(
    region: crate::region::CuboidRegion,
    thickness: i32,
    solid: &impl Fn(BlockPos) -> bool,
) -> Vec<BlockPos> {
    let min = region.minimum();
    let max = region.maximum();
    let mut outside: HashSet<BlockPos> = HashSet::new();

    for x in min.x..=max.x {
        for y in min.y..=max.y {
            recurse_hollow(BlockPos::new(x, y, min.z), &region, &mut outside, solid);
            recurse_hollow(BlockPos::new(x, y, max.z), &region, &mut outside, solid);
        }
    }
    for y in min.y..=max.y {
        for z in min.z..=max.z {
            recurse_hollow(BlockPos::new(min.x, y, z), &region, &mut outside, solid);
            recurse_hollow(BlockPos::new(max.x, y, z), &region, &mut outside, solid);
        }
    }
    for z in min.z..=max.z {
        for x in min.x..=max.x {
            recurse_hollow(BlockPos::new(x, min.y, z), &region, &mut outside, solid);
            recurse_hollow(BlockPos::new(x, max.y, z), &region, &mut outside, solid);
        }
    }

    // `for (int i = 1; i < thickness; ++i)`: a thickness of 0 or of 1 grows nothing,
    // and `1..thickness` is empty for both.
    for _ in 1..thickness {
        let mut grown = Vec::new();
        for x in min.x..=max.x {
            for y in min.y..=max.y {
                for z in min.z..=max.z {
                    let position = BlockPos::new(x, y, z);
                    if touches_outside(position, &outside) {
                        grown.push(position);
                    }
                }
            }
        }
        outside.extend(grown);
    }

    let mut positions = Vec::new();
    for x in min.x..=max.x {
        for y in min.y..=max.y {
            for z in min.z..=max.z {
                let position = BlockPos::new(x, y, z);
                if !touches_outside(position, &outside) {
                    positions.push(position);
                }
            }
        }
    }
    positions
}

/// A square pyramid, `size` blocks tall, standing on `base`.
///
/// This is `//pyramid`: the bottom layer is `2*size - 1` square and each layer above
/// loses two blocks in both X and Z, so the apex is a single block.
///
/// A `size` of 0 or less is empty, because `makePyramid` is
/// `for (int y = 0; y <= height; ++y)` with `height = size` (EditSession.java:2745-2748):
/// a negative size never enters the loop, and a zero size enters it once with the layer
/// already shrunk to `-1`. Neither builds an inverted pyramid — `//pyramid stone -5`
/// places no blocks at all.
#[must_use]
pub fn pyramid(base: BlockPos, size: i32, hollow: bool) -> Vec<BlockPos> {
    if size <= 0 {
        return Vec::new();
    }
    let mut positions = Vec::new();
    for step in 0..size {
        // The layer's half-width shrinks by one each step up, which is what makes the
        // sides slope at 45 degrees.
        let half = size - 1 - step;
        let y = base.y + step;
        for x in -half..=half {
            for z in -half..=half {
                // A hollow pyramid keeps only its faces, so the edges stay where the
                // block is on the boundary of its own layer.
                if hollow && x.abs() != half && z.abs() != half {
                    continue;
                }
                positions.push(BlockPos::new(base.x + x, y, base.z + z));
            }
        }
    }
    positions
}

/// The wall a hollow cone is given when the caller does not say.
///
/// `makeCone` takes a `thickness` and `GenerationCommands.cone` defaults it to `1`
/// (`@Arg(desc = "Thickness of the hollow cone", def = "1")`,
/// GenerationCommands.java:216-217). The argument is not plumbed through to
/// [`cone`], so the command default is what `//hcone` has always built here.
const HOLLOW_CONE_THICKNESS: f64 = 1.0;

/// A cone: the set of blocks inside one 3-D inequality.
///
/// This is `EditSession.makeCone`, and it is *not* a stack of shrinking ellipses. Each
/// block is tested against `x²/rx² + z²/rz² - (y-layers)²/height² <= 0`, which is widest
/// at `y = 0` and closes to a single block at the tip; scaling the base layer's offsets
/// down instead, as a stack of ellipses would, rounds several of them onto the same
/// block. A negative `height` extends downwards rather than doing nothing.
///
/// Unlike [`sphere`] and [`cylinder`], the radii are used exactly as given — `makeCone`
/// takes `ceil(radiusX)` where the other two add `0.5` first.
#[must_use]
pub fn cone(
    base: BlockPos,
    radius: (f64, f64),
    height: i32,
    hollow: bool,
    bounds: Bounds,
) -> Vec<BlockPos> {
    // `makeCone` loops `y` from `0` to `Math.abs(height)`, so a zero height is a loop
    // that never runs rather than a special case.
    let layers = height.unsigned_abs() as i32;
    if layers == 0 {
        return Vec::new();
    }

    let radius_x_pow = radius.0.powi(2);
    let radius_z_pow = radius.1.powi(2);
    let height_pow = f64::from(height).powi(2);
    let ceil_x = radius.0.ceil() as i32;
    let ceil_z = radius.1.ceil() as i32;
    let thickness = HOLLOW_CONE_THICKNESS;

    let mut positions = Vec::new();
    for step in 0..layers {
        let y = if height < 0 {
            base.y - step
        } else {
            base.y + step
        };
        if !bounds.holds(BlockPos::new(base.x, y, base.z)) {
            continue;
        }
        // The `(y - layers)² / height²` term is what makes this one inequality rather
        // than a per-layer radius: it grows towards the tip and so narrows the layer.
        let height_term = f64::from(step - layers).powi(2) / height_pow;

        'x: for x in 0..=ceil_x {
            let x_term = f64::from(x).powi(2) / radius_x_pow;
            for z in 0..=ceil_z {
                let z_term = f64::from(z).powi(2) / radius_z_pow;
                let distance = x_term + z_term - height_term;
                if distance > 1.0 {
                    // The inequality is monotone in x and z, so once a row is outside,
                    // every row below it is too.
                    if z == 0 {
                        break 'x;
                    }
                    break;
                }

                if hollow {
                    // `makeCone`'s own arithmetic, denominators and all: `y_next`
                    // divides by the *Z* radius and `z_next` by the *height*, which
                    // reads like an upstream slip in EditSession.java:2431-2433 rather
                    // than something to quietly correct — `//hcone` has to come out the
                    // way FAWE's does. A hollow cone only ever removes blocks; it never
                    // widens the shell out past `distance <= 0`.
                    let x_next =
                        (f64::from(x) + thickness).powi(2) / radius_x_pow + z_term - height_term;
                    let y_next = x_term + z_term
                        - (f64::from(step) + thickness - f64::from(layers)).powi(2) / radius_z_pow;
                    let z_next =
                        x_term + (f64::from(z) + thickness).powi(2) / height_pow - height_term;
                    // `y + thickness != layers` keeps the tip solid: a cone one layer
                    // from its apex has nothing to hollow out.
                    if x_next <= 0.0
                        && z_next <= 0.0
                        && y_next <= 0.0
                        && f64::from(step) + thickness != f64::from(layers)
                    {
                        continue;
                    }
                }

                if distance <= 0.0 {
                    positions.push(BlockPos::new(base.x + x, y, base.z + z));
                    if x != 0 {
                        positions.push(BlockPos::new(base.x - x, y, base.z + z));
                    }
                    if z != 0 {
                        positions.push(BlockPos::new(base.x + x, y, base.z - z));
                    }
                    if x != 0 && z != 0 {
                        positions.push(BlockPos::new(base.x - x, y, base.z - z));
                    }
                }
            }
        }
    }
    positions
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: Bounds = Bounds {
        min_y: -64,
        max_y: 319,
    };

    fn count(positions: &[BlockPos]) -> usize {
        let set: HashSet<BlockPos> = positions.iter().copied().collect();
        assert_eq!(
            set.len(),
            positions.len(),
            "a shape must not repeat a block"
        );
        positions.len()
    }

    /// A mask that blocks movement everywhere inside `inside` except at `open`, and
    /// blocks nothing outside it.
    ///
    /// The "nothing outside it" half matters: `recurseHollow` records a block before it
    /// checks whether the block is in the region (EditSession.java:3752-3757), so the
    /// flood keeps going one step past the selection and that ring is what keeps the
    /// selection's own surface alive. A test that called the world solid out there
    /// would silently remove the opening the player is standing in.
    fn solid_except(
        inside: &crate::region::CuboidRegion,
        open: &[BlockPos],
    ) -> impl Fn(BlockPos) -> bool {
        let open: HashSet<BlockPos> = open.iter().copied().collect();
        move |position: BlockPos| inside.contains(position) && !open.contains(&position)
    }

    /// A radius-1 sphere is the 3x3x3 cube minus its eight corners, which is 19
    /// blocks: the radii have `0.5` added, so an offset of one normalises to
    /// about `0.44` and three of them together exceed one.
    #[test]
    fn a_unit_sphere_is_the_cube_without_its_corners() {
        let positions = sphere(BlockPos::new(0, 0, 0), (1.0, 1.0, 1.0), false, BOUNDS);
        assert_eq!(count(&positions), 19);
        assert!(positions.contains(&BlockPos::new(0, 0, 0)));
        for neighbour in NEIGHBOURS {
            assert!(
                positions.contains(&BlockPos::new(neighbour.0, neighbour.1, neighbour.2)),
                "missing neighbour {neighbour:?}"
            );
        }
        assert!(
            !positions.contains(&BlockPos::new(1, 1, 1)),
            "a corner is outside a radius-1 sphere"
        );
    }

    /// A sphere is symmetric about its centre, which is only true if both signs of
    /// each axis are generated.
    #[test]
    fn a_sphere_is_symmetric_about_its_centre() {
        let centre = BlockPos::new(7, 64, -3);
        let positions = sphere(centre, (6.0, 4.0, 6.0), false, BOUNDS);
        let set: HashSet<BlockPos> = positions.iter().copied().collect();
        for position in &positions {
            let mirrored = BlockPos::new(
                2 * centre.x - position.x,
                2 * centre.y - position.y,
                2 * centre.z - position.z,
            );
            assert!(
                set.contains(&mirrored),
                "{position:?} has no mirror {mirrored:?}"
            );
        }
    }

    #[test]
    fn a_hollow_sphere_is_a_shell_and_not_a_solid() {
        let solid = sphere(BlockPos::new(0, 0, 0), (4.0, 4.0, 4.0), false, BOUNDS);
        let hollow = sphere(BlockPos::new(0, 0, 0), (4.0, 4.0, 4.0), true, BOUNDS);
        assert!(hollow.len() < solid.len());
        assert!(
            hollow.contains(&BlockPos::new(4, 0, 0)),
            "the surface has to be kept"
        );
        assert!(
            !hollow.contains(&BlockPos::new(0, 0, 0)),
            "the centre is interior and must be dropped"
        );
    }

    /// The shape is clipped to the world, so a tall sphere must not place blocks
    /// above the ceiling.
    #[test]
    fn a_sphere_is_clipped_to_the_world() {
        let ceiling = Bounds { min_y: 0, max_y: 4 };
        let positions = sphere(BlockPos::new(0, 2, 0), (3.0, 30.0, 3.0), false, ceiling);
        assert!(positions.iter().all(|p| (0..=4).contains(&p.y)));
    }

    #[test]
    fn a_zero_radius_sphere_is_a_single_block() {
        let positions = sphere(BlockPos::new(0, 0, 0), (0.0, 0.0, 0.0), false, BOUNDS);
        assert_eq!(positions, vec![BlockPos::new(0, 0, 0)]);
    }

    /// A radius-1 cylinder is a full 3x3 column, which is the 3x3 footprint with the
    /// `0.5` added to the radii.
    #[test]
    fn a_unit_cylinder_is_a_three_by_three_column() {
        let positions = cylinder(BlockPos::new(0, 64, 0), (1.0, 1.0), 3, false, 0.0, BOUNDS);
        assert_eq!(positions.len(), 27);
        for x in -1..=1 {
            for z in -1..=1 {
                for y in 64..=66 {
                    assert!(
                        positions.contains(&BlockPos::new(x, y, z)),
                        "missing {x},{y},{z}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_negative_height_grows_downwards() {
        let up = cylinder(BlockPos::new(0, 64, 0), (1.0, 1.0), 3, false, 0.0, BOUNDS);
        let down = cylinder(BlockPos::new(0, 64, 0), (1.0, 1.0), -3, false, 0.0, BOUNDS);
        assert!(up.iter().all(|p| p.y >= 64));
        assert!(down.iter().all(|p| p.y < 64));
        assert_eq!(up.len(), down.len());
    }

    #[test]
    fn a_zero_height_cylinder_is_nothing() {
        assert!(cylinder(BlockPos::new(0, 0, 0), (1.0, 1.0), 0, false, 0.0, BOUNDS).is_empty());
    }

    /// A hollow cylinder of radius 4 must be a ring, so the centre is absent and
    /// the rim is present.
    #[test]
    fn a_hollow_cylinder_is_a_ring() {
        let solid = cylinder(BlockPos::new(0, 64, 0), (4.0, 4.0), 1, false, 0.0, BOUNDS);
        let hollow = cylinder(BlockPos::new(0, 64, 0), (4.0, 4.0), 1, true, 0.0, BOUNDS);
        assert!(solid.contains(&BlockPos::new(0, 64, 0)));
        assert!(!hollow.contains(&BlockPos::new(0, 64, 0)));
        assert!(hollow.contains(&BlockPos::new(4, 64, 0)));
        assert!(hollow.len() < solid.len());
    }

    /// A band as thick as the cylinder is not a thinner ring, it is the whole cylinder.
    ///
    /// `makeCylinder` computes `1 / (radiusX - thickness)` (EditSession.java:2275-2276)
    /// against a radius that already had `0.5` added (EditSession.java:2232-2233), and
    /// never checks the result. At a thickness of the radius the inverse is already 2,
    /// so `(x + 1) * minInvRadiusX` is past 1 at `x == 0`, the interior test on
    /// EditSession.java:2303 can never hold, and nothing is skipped. Half a block past
    /// that the divisor is zero and the inverse is infinite, which is the same answer;
    /// further out it goes negative, and the squaring on that same line turns it back
    /// into a large number.
    ///
    /// This port returned nothing from a thickness of `radius + 0.5` upward, a special
    /// case Java does not have.
    #[test]
    fn a_band_as_thick_as_the_radius_is_the_whole_cylinder() {
        let base = BlockPos::new(0, 64, 0);
        let filled = cylinder(base, (4.0, 4.0), 1, false, 0.0, BOUNDS);
        for thickness in [4.0, 4.5, 5.0] {
            assert_eq!(
                cylinder(base, (4.0, 4.0), 1, false, thickness, BOUNDS),
                filled,
                "a thickness of {thickness} against a radius of 4 fills the cylinder"
            );
        }
    }

    /// A straight line runs from one end to the other with no gaps along the
    /// dominant axis.
    #[test]
    fn a_line_reaches_both_ends_without_gaps() {
        let start = BlockPos::new(0, 64, 0);
        let end = BlockPos::new(10, 64, 0);
        let positions = line(start, end, 0.0, false, false);
        assert_eq!(positions.len(), 11);
        assert_eq!(positions.first(), Some(&start));
        assert_eq!(positions.last(), Some(&end));
        for x in 0..=10 {
            assert!(positions.contains(&BlockPos::new(x, 64, 0)), "gap at x={x}");
        }
    }

    /// `EditSession.drawLine` rounds with `Math.round`, which is `floor(v + 0.5)`, so
    /// a tie goes toward positive infinity. Rust's `f64::round` goes away from zero,
    /// and the two only disagree on a *negative* half - so the line has to descend
    /// through one, which is why the test sits below y=0 rather than at y=64.
    #[test]
    fn a_descending_line_rounds_a_negative_half_toward_positive_infinity() {
        // x dominates, so y is the interpolated axis and is exactly -0.5 at the
        // middle step. Java's Math.round(-0.5) is 0; (-0.5f64).round() is -1.
        let start = BlockPos::new(0, 0, 0);
        let end = BlockPos::new(2, -1, 0);
        let positions = line(start, end, 0.0, false, false);

        assert_eq!(
            positions,
            vec![
                BlockPos::new(0, 0, 0),
                BlockPos::new(1, 0, 0),
                BlockPos::new(2, -1, 0),
            ],
            "the middle block rounds up to y=0, the way Math.round does"
        );
    }

    /// The same tie in the positive direction is where both roundings agree, which is
    /// why a descending line is the only way to see the difference at all.
    #[test]
    fn a_rising_line_rounds_the_same_half_both_ways() {
        let positions = line(
            BlockPos::new(0, 0, 0),
            BlockPos::new(2, 1, 0),
            0.0,
            false,
            false,
        );
        assert_eq!(
            positions,
            vec![
                BlockPos::new(0, 0, 0),
                BlockPos::new(1, 1, 0),
                BlockPos::new(2, 1, 0),
            ]
        );
    }

    /// The dominant axis decides the rounding, so an x-dominant line steps x
    /// exactly and rounds y and z.
    #[test]
    fn the_dominant_axis_is_stepped_exactly() {
        let positions = line(
            BlockPos::new(0, 0, 0),
            BlockPos::new(4, 2, 2),
            0.0,
            false,
            false,
        );
        assert_eq!(positions.len(), 5);
        assert_eq!(positions[4], BlockPos::new(4, 2, 2));
        assert_eq!(positions[2], BlockPos::new(2, 1, 1));
    }

    #[test]
    fn a_zero_length_line_is_one_block() {
        let positions = line(
            BlockPos::new(1, 2, 3),
            BlockPos::new(1, 2, 3),
            0.0,
            false,
            false,
        );
        assert_eq!(positions, vec![BlockPos::new(1, 2, 3)]);
    }

    /// A thick line has to be thicker than the thin one in every direction, and a
    /// hollow thick one must have an empty core.
    #[test]
    fn a_thick_line_is_inflated_and_a_hollow_one_is_not_solid() {
        let start = BlockPos::new(0, 64, 0);
        let end = BlockPos::new(8, 64, 0);
        let thin = line(start, end, 0.0, false, false);
        let thick = line(start, end, 2.0, false, false);
        let tube = line(start, end, 2.0, true, false);
        assert!(thick.len() > thin.len());
        assert!(tube.len() < thick.len());
        assert!(
            !tube.contains(&BlockPos::new(4, 64, 0)),
            "the core of a hollow tube must be empty"
        );
        assert!(
            tube.contains(&BlockPos::new(4, 66, 0)),
            "the surface of the tube must be kept"
        );
    }

    /// A flat thick line spreads in the horizontal plane only, so it must not gain
    /// any Y.
    #[test]
    fn a_flat_thick_line_does_not_gain_height() {
        let positions = line(
            BlockPos::new(0, 64, 0),
            BlockPos::new(8, 64, 0),
            2.0,
            false,
            true,
        );
        assert!(positions.iter().all(|p| p.y == 64));
        assert!(positions.contains(&BlockPos::new(4, 64, 2)));
    }

    #[test]
    fn an_inflated_line_reports_each_block_once() {
        let positions = line(
            BlockPos::new(0, 0, 0),
            BlockPos::new(6, 0, 0),
            2.0,
            false,
            false,
        );
        count(&positions);
    }
    /// `EditSession.hollowOutRegion` floods air in from the six faces of the bounding
    /// box and replaces whatever is *not* reached, so a selection of nothing but solid
    /// blocks is emptied completely at every thickness.
    ///
    /// This replaces `hollow_leaves_a_shell_of_the_requested_thickness`, which asserted
    /// the Chebyshev erosion this port used to do instead. That erosion had no way to
    /// look at a block, so it hollowed a solid cube where Java empties nothing and left
    /// a sealed room standing where Java empties it.
    ///
    /// `SolidBlockMask` tests a movement blocker (SolidBlockMask.java:34), so a world of
    /// solid blocks refuses every seed, `outside` stays empty and the final loop
    /// (EditSession.java:3402-3415) finds no neighbour of any position in it.
    #[test]
    fn a_fully_solid_selection_is_replaced_whole_at_every_thickness() {
        let cube = crate::region::CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(4, 4, 4));
        let solid = solid_except(&cube, &[]);
        for thickness in 0..=4 {
            assert_eq!(
                hollow_out(cube, thickness, &solid).len(),
                125,
                "a solid selection is emptied completely at thickness {thickness}"
            );
        }
    }

    /// `EditSession.hollowOutRegion` grows `outside` with `for (int i = 1; i < thickness;
    /// ++i)` (EditSession.java:3383), which never runs for a thickness of 0 or of 1, so
    /// the two are the same edit.
    ///
    /// A thickness of 0 used to clear the whole selection here, and did so only by
    /// accident of the erosion; Java treats it as 1.
    #[test]
    fn a_thickness_of_zero_is_a_thickness_of_one() {
        let cube = crate::region::CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(4, 4, 4));
        // A stone cube with its middle column open from top to bottom, so the flood has
        // somewhere to start and the two thicknesses could differ if the loop ran.
        let shaft = (0..=4).map(|y| BlockPos::new(2, y, 2)).collect::<Vec<_>>();
        let solid = solid_except(&cube, &shaft);
        assert_eq!(
            hollow_out(cube, 0, &solid),
            hollow_out(cube, 1, &solid),
            "//hollow 0 and //hollow 1 are the same edit"
        );
    }

    /// The flood is what hollows anything: air reachable from a face spreads inwards,
    /// and only blocks with no neighbour it reached are replaced.
    ///
    /// A 3x3x3 of stone with the top face's centre open. The flood reaches that one air
    /// block and the cell above it, which is outside the selection and is what keeps the
    /// selection's own top face alive, so at thickness 1 the six blocks around the
    /// opening survive and the other 21 are replaced. Thickness 2 grows `outside` once,
    /// reaching the five blocks under and beside the opening, which leaves 12.
    #[test]
    fn the_flood_reaches_in_through_an_opening() {
        let cube = crate::region::CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 2, 2));
        let opening = [BlockPos::new(1, 2, 1)];
        let solid = solid_except(&cube, &opening);
        assert_eq!(
            hollow_out(cube, 1, &solid).len(),
            21,
            "only the six blocks around the opening survive the flood"
        );
        assert_eq!(
            hollow_out(cube, 2, &solid).len(),
            12,
            "a second layer of outside swallows nine more blocks"
        );
    }

    /// The flood stops at a solid floor, so a room that is open above is left alone.
    ///
    /// This is the other half of the flood's behaviour and the half a player is most
    /// surprised by: `//hollow 1` over a floor with open air above it replaces nothing,
    /// because every block of the floor has a neighbour the flood reached. The old
    /// erosion replaced the 3x3x3 core of this same cube instead.
    #[test]
    fn a_solid_floor_under_open_air_is_left_alone() {
        let cube = crate::region::CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 2, 2));
        let open: Vec<_> = (1..=2)
            .flat_map(|y| (0..=2).flat_map(move |x| (0..=2).map(move |z| BlockPos::new(x, y, z))))
            .collect();
        assert_eq!(
            hollow_out(cube, 1, &solid_except(&cube, &open)),
            Vec::new(),
            "the flood reaches the top of the floor and no further"
        );
    }

    /// A pyramid is a stack of squares shrinking by one block a side, so a size-3
    /// pyramid has 5x5 + 3x3 + 1x1 blocks.
    #[test]
    fn a_pyramid_shrinks_by_one_block_a_side_each_layer() {
        let base = BlockPos::new(0, 0, 0);
        assert_eq!(pyramid(base, 3, false).len(), 25 + 9 + 1);
        assert_eq!(
            pyramid(base, 0, false).len(),
            0,
            "a zero-height pyramid is empty"
        );
        // The apex sits at the top and is a single block.
        assert!(pyramid(base, 3, false).contains(&BlockPos::new(0, 2, 0)));
        assert_eq!(pyramid(base, 1, false).len(), 1);
    }

    /// A hollow pyramid keeps only its faces, so it is markedly smaller than a solid
    /// one of the same size and contains no interior blocks at the base.
    #[test]
    fn a_hollow_pyramid_has_no_interior() {
        let base = BlockPos::new(0, 0, 0);
        let hollow = pyramid(base, 4, true);
        assert!(hollow.len() < pyramid(base, 4, false).len());
        // (0,0,0) is the centre of the 7x7 base layer, which is interior and so gone.
        assert!(!hollow.contains(&BlockPos::new(0, 0, 0)));
        // The corner of that layer is on its face and stays.
        assert!(hollow.contains(&BlockPos::new(-3, 0, -3)));
    }

    /// A cone tapers, so it has fewer blocks than a cylinder of the same radius and
    /// height, and every layer is no wider than the one below it.
    #[test]
    fn a_cone_tapers_to_nothing() {
        let bounds = Bounds {
            min_y: -64,
            max_y: 319,
        };
        let base = BlockPos::new(0, 64, 0);
        let cone_positions = cone(base, (3.0, 3.0), 4, false, bounds);
        let cylinder = cylinder(base, (3.0, 3.0), 4, false, 0.0, bounds);
        assert!(!cone_positions.is_empty());
        assert!(
            cone_positions.len() < cylinder.len(),
            "a tapered shape must be smaller than a straight one"
        );

        // Every layer is no wider than the base layer.
        let base_layer: Vec<_> = cone_positions.iter().filter(|p| p.y == 64).collect();
        for position in cone_positions.iter().filter(|p| p.y != 64) {
            assert!(
                base_layer
                    .iter()
                    .any(|b| b.x == position.x && b.z == position.z)
            );
        }
    }

    /// A cone is clipped to the world, the same as every other generated shape.
    #[test]
    fn a_cone_is_clipped_to_the_world() {
        let bounds = Bounds { min_y: 0, max_y: 3 };
        let positions = cone(BlockPos::new(0, 0, 0), (2.0, 2.0), 10, false, bounds);
        assert!(positions.iter().all(|p| (0..=3).contains(&p.y)));
    }

    /// A zero height is no cone at all: `makeCone` loops `y` from `0` to
    /// `Math.abs(height)`, so it never runs.
    #[test]
    fn a_cone_with_no_height_is_empty() {
        assert!(cone(BlockPos::new(0, 64, 0), (3.0, 3.0), 0, false, BOUNDS).is_empty());
    }

    /// `//cone stone 3 4` is not a stack of shrinking ellipses. `makeCone` tests one
    /// 3-D inequality per block, `x²/rx² + z²/rz² - (y-layers)²/height² <= 0`, which
    /// is widest at `y = 0` and closes to a single block at the tip: 29, 21, 9 and 1.
    ///
    /// The radii are used as given — `makeCone` takes `ceil(radiusX)` where `makeSphere`
    /// and `makeCylinder` add `0.5` first, so a cone of radius 3 is *not* a cone of
    /// radius 3.5.
    #[test]
    fn a_cone_is_one_three_dimensional_inequality() {
        let positions = cone(BlockPos::new(0, 64, 0), (3.0, 3.0), 4, false, BOUNDS);
        assert_eq!(count(&positions), 60);
        let layers: Vec<_> = (0..4)
            .map(|step| positions.iter().filter(|p| p.y == 64 + step).count())
            .collect();
        assert_eq!(layers, vec![29, 21, 9, 1]);
    }

    /// A negative height extends *downwards* rather than doing nothing: `makeCone` puts
    /// the block at `cy - y` when the height is negative, so `//cone stone 3 -4` is the
    /// same cone as `//cone stone 3 4`, pointing the other way.
    #[test]
    fn a_negative_height_cone_grows_downwards() {
        let base = BlockPos::new(0, 64, 0);
        let up = cone(base, (3.0, 3.0), 4, false, BOUNDS);
        let down = cone(base, (3.0, 3.0), -4, false, BOUNDS);

        assert_eq!(count(&down), up.len());
        assert!(
            down.contains(&base),
            "the base is the top of a downward cone"
        );
        assert!(
            down.iter().all(|p| p.y <= base.y),
            "a downward cone never rises above its base"
        );

        let mirrored: HashSet<BlockPos> = down
            .iter()
            .map(|p| BlockPos::new(p.x, 2 * base.y - p.y, p.z))
            .collect();
        assert_eq!(
            mirrored,
            up.iter().copied().collect::<HashSet<BlockPos>>(),
            "a downward cone is the same cone reflected about its base"
        );
    }

    /// A hollow cone is only ever a subset of the solid one: `makeCone`'s hollow branch
    /// only ever `continue`s, so it carves the inside out and cannot widen the shell
    /// past `distance <= 0`. The tip survives, because a cone one layer from its apex
    /// has nothing left to hollow.
    #[test]
    fn a_hollow_cone_is_the_solid_one_carved_out() {
        let base = BlockPos::new(0, 64, 0);
        let solid = cone(base, (3.0, 3.0), 4, false, BOUNDS);
        let hollow = cone(base, (3.0, 3.0), 4, true, BOUNDS);

        let hollow_set: HashSet<BlockPos> = hollow.iter().copied().collect();
        assert!(hollow.len() < solid.len(), "hollowing has to remove blocks");
        for position in &hollow {
            assert!(
                solid.contains(position),
                "{position:?} is outside the solid cone, so hollowing invented it"
            );
        }
        assert!(
            !hollow.contains(&base),
            "the middle of the base is interior and must go"
        );
        assert!(
            hollow.contains(&BlockPos::new(0, 67, 0)),
            "the tip is the last layer and stays"
        );
        assert!(
            hollow_set.contains(&BlockPos::new(3, 64, 0)),
            "the rim of the base is on the surface and stays"
        );
    }

    /// `makePyramid` is `for (int y = 0; y <= height; ++y)` with `height = size`, so a
    /// negative size never enters the body and places nothing at all.
    ///
    /// Nothing upstream stops `//pyramid stone -5` from arriving there: `checkMaxRadius`
    /// compares `radius > max`, so it is an upper bound only.
    #[test]
    fn a_negative_size_pyramid_is_nothing() {
        let base = BlockPos::new(0, 0, 0);
        assert_eq!(pyramid(base, -1, false).len(), 0);
        assert_eq!(pyramid(base, -5, false).len(), 0);
        assert_eq!(pyramid(base, -5, true).len(), 0);
    }
}
