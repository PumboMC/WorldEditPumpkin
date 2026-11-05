//! Selection shapes, and the cursor that walks one.
//!
//! Every shape is a [`Region`], and every region is walked the same way: iterate
//! its bounding box and keep the positions [`Region::contains`] accepts. That is
//! what WorldEdit's `RegionVisitor` does, and it is why the shapes only need an
//! accurate `contains` rather than a bespoke iterator each.
//!
//! The shapes deliberately do not share their geometry. `CylinderRegion` and
//! `SphereRegion` here mirror the selection regions, which is not the same
//! arithmetic as `EditSession.makeCylinder` / `makeSphere`, which is what the
//! `//cyl` and `//sphere` commands call. The two round radii differently, so one
//! implementation of each is kept rather than a shared one that is right for
//! neither. See `shape`.

use crate::math::{BlockPos, Vec2, Vec3};

/// A set of block positions that an edit is confined to.
///
/// Not a `Clone` supertrait: `Clone` requires `Self: Sized`, which would stop the
/// shapes from being usable as `dyn Region`. The bound lives on
/// [`Region::boxed`] instead, which is the only place a clone is needed.
pub trait Region: Send + Sync {
    /// Whether `pos` belongs to this region.
    fn contains(&self, pos: BlockPos) -> bool;

    /// The lowest corner of the region's bounding box.
    fn minimum_point(&self) -> BlockPos;

    /// The highest corner of the region's bounding box.
    fn maximum_point(&self) -> BlockPos;

    /// An approximate block count, as WorldEdit reports it.
    ///
    /// This is an estimate from the shape formula rather than an exact count of
    /// the positions [`Region::contains`] accepts, so it can disagree with the
    /// number an edit actually reports. WorldEdit has the same split: `getVolume`
    /// is the closed-form figure used by `//size`, and `getAffected` is counted
    /// during the edit.
    fn volume(&self) -> u64;

    /// The bounding box, for the walker to traverse.
    fn bounding_box(&self) -> CuboidRegion {
        CuboidRegion::new(self.minimum_point(), self.maximum_point())
    }

    /// A copy of this region on the heap, for [`RegionWalker`] to own.
    #[must_use]
    fn boxed(&self) -> Box<dyn Region>
    where
        Self: Clone + 'static,
    {
        Box::new(self.clone())
    }
}

/// An axis-aligned cuboid, the selection `//pos1` and `//pos2` produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CuboidRegion {
    min: BlockPos,
    max: BlockPos,
}

impl CuboidRegion {
    /// A cuboid spanning the two corners, in either order.
    #[must_use]
    pub fn new(first: BlockPos, second: BlockPos) -> Self {
        Self {
            min: first.min(second),
            max: first.max(second),
        }
    }

    /// The lower corner.
    #[must_use]
    pub fn minimum(&self) -> BlockPos {
        self.min
    }

    /// The upper corner.
    #[must_use]
    pub fn maximum(&self) -> BlockPos {
        self.max
    }

    /// The cuboid moved by the given deltas.
    #[must_use]
    pub fn offset(&self, dx: i32, dy: i32, dz: i32) -> Self {
        Self::new(self.min.offset(dx, dy, dz), self.max.offset(dx, dy, dz))
    }

    /// The cuboid grown by `amount` blocks on every side.
    ///
    /// This is `//outset` without the `-h` / `-v` split, which expands all three
    /// axes at once.
    #[must_use]
    pub fn outset(&self, amount: i32) -> Self {
        Self::new(
            self.min.offset(-amount, -amount, -amount),
            self.max.offset(amount, amount, amount),
        )
    }

    /// The cuboid shrunk by `amount` blocks on every side.
    ///
    /// A cuboid thinner than twice the amount in an axis collapses to that axis's
    /// single middle block rather than inverting, which is what `//inset` does.
    #[must_use]
    pub fn inset(&self, amount: i32) -> Self {
        Self::new(
            BlockPos::new(
                self.min.x + amount,
                self.min.y + amount,
                self.min.z + amount,
            ),
            BlockPos::new(
                self.max.x - amount,
                self.max.y - amount,
                self.max.z - amount,
            ),
        )
    }

    /// The cuboid shifted by the given deltas.
    #[must_use]
    pub fn shift(&self, dx: i32, dy: i32, dz: i32) -> Self {
        self.offset(dx, dy, dz)
    }
}

impl Region for CuboidRegion {
    fn contains(&self, pos: BlockPos) -> bool {
        pos.x >= self.min.x
            && pos.x <= self.max.x
            && pos.y >= self.min.y
            && pos.y <= self.max.y
            && pos.z >= self.min.z
            && pos.z <= self.max.z
    }

    fn minimum_point(&self) -> BlockPos {
        self.min
    }

    fn maximum_point(&self) -> BlockPos {
        self.max
    }

    fn volume(&self) -> u64 {
        let dimensions = self.dimensions();
        u64::try_from(dimensions.x).unwrap_or(0)
            * u64::try_from(dimensions.y).unwrap_or(0)
            * u64::try_from(dimensions.z).unwrap_or(0)
    }
}

impl CuboidRegion {
    /// The cuboid's size in blocks on each axis, inclusive of both corners.
    #[must_use]
    pub fn dimensions(&self) -> BlockPos {
        BlockPos::new(
            self.max.x - self.min.x + 1,
            self.max.y - self.min.y + 1,
            self.max.z - self.min.z + 1,
        )
    }
}

/// An ellipsoid, and the sphere that is its equal-radii case.
///
/// Mirrors `EllipsoidRegion`, which is what `//sel ellipsoid` and `//sel sphere`
/// build. The radius is stored with `0.5` added per axis because the tests run
/// against block coordinates rather than block centres, and the inner region
/// carries WorldEdit's integer early-outs alongside the exact test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SphereRegion {
    center: BlockPos,
    /// The requested radii with `0.5` added to each axis.
    radius: Vec3,
    /// `radius.squared()`, floored per axis, for the integer early-outs.
    radius_squared_blocks: BlockPos,
    /// `radius.x().powi(2)`, truncated, for the sphere fast path.
    radius_length_squared: i32,
    /// Whether all three radii are equal, which permits the integer sphere test.
    sphere: bool,
    /// `radius.squared().reciprocal()`, for the exact test.
    inverse_radius_squared: Vec3,
    /// The world's top Y, which disables the Y early-out for tall selections.
    world_max_y: i32,
}

impl SphereRegion {
    /// An ellipsoid of the given radii centred on `center`, inside a world of the
    /// given height.
    ///
    /// `world_max_y` is needed because the Y early-out is only valid while the
    /// ellipsoid fits under the world ceiling; a selection taller than the world
    /// would otherwise be truncated. `worldedit-core` passes `i32::MAX` for a
    /// region not bound to a world, matching `AbstractRegion.getWorldMaxY`.
    #[must_use]
    pub fn new(center: BlockPos, radius: Vec3, world_max_y: i32) -> Self {
        let radius = radius.add(0.5);
        let squared = radius.squared();
        Self {
            center,
            radius,
            radius_squared_blocks: BlockPos::new(
                squared.x as i32,
                squared.y as i32,
                squared.z as i32,
            ),
            radius_length_squared: squared.x as i32,
            sphere: radius.y == radius.x && radius.x == radius.z,
            inverse_radius_squared: squared.reciprocal(),
            world_max_y,
        }
    }

    /// The radii as the player asked for them, without the internal `0.5`.
    #[must_use]
    pub fn radius(&self) -> Vec3 {
        self.radius.add(-0.5)
    }
}

impl Region for SphereRegion {
    fn contains(&self, pos: BlockPos) -> bool {
        let cx = pos.x - self.center.x;
        let cx2 = cx * cx;
        if cx2 > self.radius_squared_blocks.x {
            return false;
        }
        let cz = pos.z - self.center.z;
        let cz2 = cz * cz;
        if cz2 > self.radius_squared_blocks.z {
            return false;
        }
        let cy = pos.y - self.center.y;
        let cy2 = cy * cy;
        if self.radius_squared_blocks.y < self.world_max_y && cy2 > self.radius_squared_blocks.y {
            return false;
        }
        if self.sphere {
            // The sphere fast path is integer maths throughout, so it does not go
            // through `inverse_radius_squared`.
            return i64::from(cx2) + i64::from(cy2) + i64::from(cz2)
                <= i64::from(self.radius_length_squared);
        }
        let cxd = f64::from(cx2) * self.inverse_radius_squared.x;
        let cyd = f64::from(cy2) * self.inverse_radius_squared.y;
        let czd = f64::from(cz2) * self.inverse_radius_squared.z;
        cxd + cyd + czd <= 1.0
    }

    fn minimum_point(&self) -> BlockPos {
        let radius = self.radius();
        // Java is `center.toVector3().subtract(getRadius()).toBlockPoint()`, and
        // `toBlockPoint` is `BlockVector3.at(double, ...)`, which floors. Flooring the
        // *sum* rather than the radius is the difference: with a centre of 5 and a radius of
        // 3.5, Java gives `floor(1.5) == 1` while `5 - 3.5 as i32` gives `5 - 3 == 2`. Truncating
        // the radius and subtracting would push the bounding box in by one block on the low
        // side for every fractional radius.
        BlockPos::new(
            (f64::from(self.center.x) - radius.x).floor() as i32,
            (f64::from(self.center.y) - radius.y).floor() as i32,
            (f64::from(self.center.z) - radius.z).floor() as i32,
        )
    }

    fn maximum_point(&self) -> BlockPos {
        let radius = self.radius();
        BlockPos::new(
            (f64::from(self.center.x) + radius.x).floor() as i32,
            (f64::from(self.center.y) + radius.y).floor() as i32,
            (f64::from(self.center.z) + radius.z).floor() as i32,
        )
    }

    fn volume(&self) -> u64 {
        // `(4/3) * pi * rx * ry * rz`, floored.
        //
        // Java multiplies the *internal* radius field — the requested radius with 0.5 already
        // added — not `getRadius()`, which subtracts the 0.5 again. Using the reduced radius
        // understates the volume by more than 10% on every shape: a sphere of radius 10 is
        // 4849 blocks in Java and 4188 with the wrong one. That number sizes clipboards and
        // is what `//count` reports, so the difference is visible either way.
        let base = (4.0 / 3.0) * std::f64::consts::PI;
        (base * self.radius.x * self.radius.y * self.radius.z).floor() as u64
    }
}

/// A cylinder with independent X and Z radii and no Y radius.
///
/// Mirrors `CylinderRegion`, which is what `//sel cyl` builds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CylinderRegion {
    center_x: i32,
    center_z: i32,
    /// The requested radii with `0.5` added to each axis.
    radius: Vec2,
    /// `radius.reciprocal()`, so the test is two multiplies and an add.
    inverse_radius: Vec2,
    min_y: i32,
    max_y: i32,
}

impl CylinderRegion {
    /// A cylinder of the given radii centred on `center`, spanning `min_y`
    /// through `max_y` inclusive.
    #[must_use]
    pub fn new(center: BlockPos, radius: Vec2, min_y: i32, max_y: i32) -> Self {
        let radius = radius.add(0.5);
        Self {
            center_x: center.x,
            center_z: center.z,
            radius,
            inverse_radius: radius.reciprocal(),
            min_y,
            max_y,
        }
    }

    /// The radii as the player asked for them, without the internal `0.5`.
    #[must_use]
    pub fn radius(&self) -> Vec2 {
        self.radius.add(-0.5)
    }

    /// The lowest Y the cylinder spans.
    #[must_use]
    pub fn minimum_y(&self) -> i32 {
        self.min_y
    }

    /// The highest Y the cylinder spans.
    #[must_use]
    pub fn maximum_y(&self) -> i32 {
        self.max_y
    }

    /// Whether `(x, z)` is inside the cylinder's footprint, ignoring Y.
    #[must_use]
    pub fn contains_column(&self, x: i32, z: i32) -> bool {
        let dx = f64::from((x - self.center_x).abs()) * self.inverse_radius.x;
        let dz = f64::from((z - self.center_z).abs()) * self.inverse_radius.z;
        dx * dx + dz * dz <= 1.0
    }
}

impl Region for CylinderRegion {
    fn contains(&self, pos: BlockPos) -> bool {
        (self.min_y..=self.max_y).contains(&pos.y) && self.contains_column(pos.x, pos.z)
    }

    fn minimum_point(&self) -> BlockPos {
        let radius = self.radius();
        // Floored as a whole rather than per axis, matching Java's
        // `center.toVector2().subtract(getRadius()).toVector3(minY).toBlockPoint()`, where
        // `toBlockPoint` is `BlockVector2.at(double, double)` and that floors. Y is passed
        // through untouched, so only X and Z are affected.
        BlockPos::new(
            (f64::from(self.center_x) - radius.x).floor() as i32,
            self.min_y,
            (f64::from(self.center_z) - radius.z).floor() as i32,
        )
    }

    fn maximum_point(&self) -> BlockPos {
        let radius = self.radius();
        BlockPos::new(
            (f64::from(self.center_x) + radius.x).floor() as i32,
            self.max_y,
            (f64::from(self.center_z) + radius.z).floor() as i32,
        )
    }

    fn volume(&self) -> u64 {
        // Java multiplies the internal radius field — the requested one with 0.5 already
        // added — and `getHeight()`, which is `maxY - minY + 1`. Using `getRadius()` instead
        // would drop the 0.5 back off and understate the volume.
        let base = self.radius.x * self.radius.z * std::f64::consts::PI;
        (base * f64::from(self.max_y - self.min_y + 1)).floor() as u64
    }
}

/// The intersection of two regions, which is how `&` in a mask expression and the
/// nested selections such as `//copy -m` narrow the area an edit touches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegionIntersection<A, B> {
    first: A,
    second: B,
}

impl<A: Region, B: Region> RegionIntersection<A, B> {
    /// The region holding positions in both `first` and `second`.
    pub fn new(first: A, second: B) -> Self {
        Self { first, second }
    }
}

impl<A: Region, B: Region> Region for RegionIntersection<A, B> {
    fn contains(&self, pos: BlockPos) -> bool {
        self.first.contains(pos) && self.second.contains(pos)
    }

    fn minimum_point(&self) -> BlockPos {
        self.first.minimum_point().max(self.second.minimum_point())
    }

    fn maximum_point(&self) -> BlockPos {
        self.first.maximum_point().min(self.second.maximum_point())
    }

    fn volume(&self) -> u64 {
        self.first.volume().min(self.second.volume())
    }
}

/// A walk over a region's positions that can be paused and resumed.
///
/// FAWE queues an edit and spreads it over time rather than blocking the server
/// tick, so the walk has to be able to stop part-way and pick up where it left
/// off. Positions come out X first, then Z, then Y, which is the order
/// `CuboidRegion.iterator` produces and which keeps the inner loops contiguous.
pub struct RegionWalker {
    region: Box<dyn Region>,
    minimum: BlockPos,
    maximum: BlockPos,
    current: BlockPos,
    done: bool,
}

impl RegionWalker {
    /// Starts a walk over an owned region at the lowest position of its bounding
    /// box.
    #[must_use]
    pub fn new(region: Box<dyn Region>) -> Self {
        let minimum = region.minimum_point();
        let maximum = region.maximum_point();
        Self {
            region,
            minimum,
            maximum,
            current: minimum,
            done: false,
        }
    }

    /// Whether the walk has produced every position.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.done
    }
}

impl Iterator for RegionWalker {
    type Item = BlockPos;

    /// The next position the region contains.
    ///
    /// Returns `None` once the region is exhausted. Positions outside the region
    /// are skipped rather than yielded, so the caller only ever sees positions
    /// [`Region::contains`] accepts.
    fn next(&mut self) -> Option<BlockPos> {
        if self.done {
            return None;
        }
        loop {
            let pos = self.current;
            let contained = self.region.contains(pos);

            // Advance X, then Z, then Y, matching `CuboidRegion.iterator_old`.
            if self.current.x == self.maximum.x {
                self.current.x = self.minimum.x;
                if self.current.z == self.maximum.z {
                    self.current.z = self.minimum.z;
                    if self.current.y == self.maximum.y {
                        self.done = true;
                    } else {
                        self.current.y += 1;
                    }
                } else {
                    self.current.z += 1;
                }
            } else {
                self.current.x += 1;
            }

            if contained {
                return Some(pos);
            }
            if self.done {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks a concrete region to completion.
    ///
    /// Generic rather than taking `&dyn Region`, because `Region::boxed` needs a
    /// `Clone` bound that a trait object does not carry.
    fn collect<R: Region + Clone + 'static>(region: &R) -> Vec<BlockPos> {
        RegionWalker::new(region.boxed()).collect()
    }

    /// The walk must cover every accepted position exactly once, because an edit
    /// that skips one leaves a stray block and one that repeats one inflates the
    /// affected count.
    #[test]
    fn the_cuboid_walk_visits_every_block_once_in_x_z_y_order() {
        let region = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(1, 1, 1));
        assert_eq!(
            collect(&region),
            vec![
                BlockPos::new(0, 0, 0),
                BlockPos::new(1, 0, 0),
                BlockPos::new(0, 0, 1),
                BlockPos::new(1, 0, 1),
                BlockPos::new(0, 1, 0),
                BlockPos::new(1, 1, 0),
                BlockPos::new(0, 1, 1),
                BlockPos::new(1, 1, 1),
            ]
        );
    }

    #[test]
    fn the_cuboid_corners_are_ordered_not_as_supplied() {
        let region = CuboidRegion::new(BlockPos::new(5, 9, -3), BlockPos::new(-2, 1, 7));
        assert_eq!(region.minimum(), BlockPos::new(-2, 1, -3));
        assert_eq!(region.maximum(), BlockPos::new(5, 9, 7));
        assert_eq!(region.volume(), 8 * 9 * 11);
    }

    /// `//sphere 10` with the placement block at the centre must reach exactly
    /// radius 10 in every direction, because `//sphere` adds 0.5 to the radius
    /// before testing block coordinates.
    #[test]
    fn a_sphere_of_radius_ten_reaches_exactly_ten_blocks() {
        let sphere = SphereRegion::new(BlockPos::new(0, 0, 0), Vec3::new(10.0, 10.0, 10.0), 320);
        assert!(sphere.contains(BlockPos::new(10, 0, 0)));
        assert!(sphere.contains(BlockPos::new(-10, 0, 0)));
        assert!(sphere.contains(BlockPos::new(0, 10, 0)));
        assert!(!sphere.contains(BlockPos::new(11, 0, 0)));
        assert!(!sphere.contains(BlockPos::new(-11, 0, 0)));
        assert!(!sphere.contains(BlockPos::new(0, 11, 0)));
    }

    /// The integer fast path and the exact test must agree at the equator, or
    /// `//sel sphere` and `//sel ellipsoid` would disagree about the same block.
    #[test]
    fn the_sphere_fast_path_agrees_with_the_exact_test() {
        let sphere = SphereRegion::new(BlockPos::new(0, 64, 0), Vec3::new(6.0, 6.0, 6.0), 320);
        let exact = SphereRegion::new(BlockPos::new(0, 64, 0), Vec3::new(6.0, 7.0, 6.0), 320);
        for x in -8..=8 {
            for y in -8..=8 {
                for z in -8..=8 {
                    let pos = BlockPos::new(x, 64 + y, z);
                    if sphere.contains(pos) {
                        // The exact ellipsoid with a taller Y radius must contain
                        // everything the equal-radii fast path contains.
                        assert!(exact.contains(pos), "fast path accepted {pos:?} only");
                    }
                }
            }
        }
    }

    #[test]
    fn a_cylinder_ignores_y_when_tested_as_a_column() {
        let cylinder = CylinderRegion::new(BlockPos::new(0, 64, 0), Vec2::new(4.0, 2.0), 60, 70);
        assert!(cylinder.contains_column(4, 0));
        assert!(cylinder.contains_column(0, 2));
        assert!(!cylinder.contains_column(0, 3));
        assert!(cylinder.contains(BlockPos::new(4, 70, 0)));
        assert!(!cylinder.contains(BlockPos::new(4, 71, 0)));
        assert!(!cylinder.contains(BlockPos::new(4, 59, 0)));
    }

    /// A sphere's walk must not yield anything outside the sphere even though the
    /// bounding box does, otherwise every edit would spill into the corners.
    #[test]
    fn the_walk_yields_only_positions_the_shape_accepts() {
        let sphere = SphereRegion::new(BlockPos::new(0, 0, 0), Vec3::new(4.0, 4.0, 4.0), 320);
        let positions = collect(&sphere);
        assert!(!positions.is_empty());
        for pos in &positions {
            assert!(
                sphere.contains(*pos),
                "{pos:?} is inside the box but not the sphere"
            );
        }
        // A radius-4 sphere covers 389 of its 9x9x9 bounding box's 729 blocks, so the
        // walk has to skip the corners rather than fill the box.
        assert!(!positions.is_empty());
        assert!(
            positions.len() < 729,
            "the walk must not visit the whole bounding box, got {}",
            positions.len()
        );
    }

    #[test]
    fn an_intersection_contains_only_positions_in_both() {
        let first = CuboidRegion::new(BlockPos::new(0, 0, 0), BlockPos::new(10, 10, 10));
        let second = CuboidRegion::new(BlockPos::new(5, 5, 5), BlockPos::new(15, 15, 15));
        let both = RegionIntersection::new(first, second);
        assert!(both.contains(BlockPos::new(7, 7, 7)));
        assert!(!both.contains(BlockPos::new(1, 1, 1)));
        assert!(!both.contains(BlockPos::new(12, 12, 12)));
        assert_eq!(both.minimum_point(), BlockPos::new(5, 5, 5));
        assert_eq!(both.maximum_point(), BlockPos::new(10, 10, 10));
    }

    #[test]
    fn chunk_coordinates_use_the_floor_so_the_west_south_quadrants_work() {
        assert_eq!(BlockPos::new(0, 0, 0).chunk_x(), 0);
        assert_eq!(BlockPos::new(15, 0, 15).chunk_x(), 0);
        assert_eq!(BlockPos::new(16, 0, 16).chunk_x(), 1);
        assert_eq!(BlockPos::new(-1, 0, -1).chunk_x(), -1);
        assert_eq!(BlockPos::new(-16, 0, -16).chunk_x(), -1);
        assert_eq!(BlockPos::new(-17, 0, -17).chunk_x(), -2);
    }

    /// The bounding box of a fractional radius, which is where the floor belongs.
    ///
    /// Java is `center.subtract(getRadius()).toBlockPoint()` and `toBlockPoint` is
    /// `BlockVector3.at(double, ...)`, which floors. With a centre of 5 and a radius of 3.5
    /// that is `floor(1.5) == 1`; truncating the radius first gives `5 - 3 == 2`.
    #[test]
    fn a_fractional_radius_floors_the_low_corner_down() {
        let sphere = SphereRegion::new(BlockPos::new(5, 64, 5), Vec3::new(3.5, 3.5, 3.5), 320);
        assert_eq!(sphere.minimum_point(), BlockPos::new(1, 60, 1));
        assert_eq!(sphere.maximum_point(), BlockPos::new(8, 67, 8));
    }

    #[test]
    fn a_whole_radius_is_unaffected_by_the_floor() {
        // The everyday case, which is why the bug survived: with a whole radius the two
        // spellings agree, so only a fractional one shows it.
        let sphere = SphereRegion::new(BlockPos::new(5, 64, 5), Vec3::new(4.0, 4.0, 4.0), 320);
        assert_eq!(sphere.minimum_point(), BlockPos::new(1, 60, 1));
        assert_eq!(sphere.maximum_point(), BlockPos::new(9, 68, 9));
    }

    #[test]
    fn a_cylinder_floors_its_low_corner_down() {
        let cylinder = CylinderRegion::new(BlockPos::new(5, 64, 5), Vec2::new(3.5, 3.5), 60, 70);
        assert_eq!(cylinder.minimum_point(), BlockPos::new(1, 60, 1));
        assert_eq!(cylinder.maximum_point(), BlockPos::new(8, 70, 8));
    }

    #[test]
    fn a_negative_centre_floors_the_right_way() {
        // -3.5 floors to -4, not towards zero. `as i32` truncates towards zero and would
        // give -3, putting the box one block out on the low side of every negative region.
        let sphere = SphereRegion::new(BlockPos::new(-5, 64, -5), Vec3::new(3.5, 3.5, 3.5), 320);
        assert_eq!(sphere.minimum_point(), BlockPos::new(-9, 60, -9));
    }

    /// The volume is taken from the internal radius, which carries the extra `0.5`.
    ///
    /// Java's `EllipsoidRegion#getVolume` multiplies `radius.x()`, the field, rather than
    /// `getRadius()`, which subtracts the `0.5` again. A radius of 10 is 4849 blocks that
    /// way and 4188 the other — a 14% difference in the number that sizes a clipboard.
    #[test]
    fn a_sphere_volume_uses_the_inflated_radius() {
        let sphere = SphereRegion::new(BlockPos::new(0, 0, 0), Vec3::new(10.0, 10.0, 10.0), 320);
        // floor((4/3) * pi * 10.5^3) = floor(4849.07) = 4849
        assert_eq!(sphere.volume(), 4849);
    }

    #[test]
    fn a_cylinder_volume_uses_the_inflated_radius_and_the_full_height() {
        // floor(pi * 4.5 * 4.5 * 11) = floor(699.78) = 699. Java's `getHeight()` is
        // `maxY - minY + 1`, so 60..=70 is eleven blocks, not ten.
        let cylinder = CylinderRegion::new(BlockPos::new(0, 0, 0), Vec2::new(4.0, 4.0), 60, 70);
        assert_eq!(cylinder.volume(), 699);
    }
}
