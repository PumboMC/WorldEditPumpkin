//! Block and vector types shared by the engine.

/// `java.lang.Math.round(double)`, which is `floor(value + 0.5)`.
///
/// Ties go toward positive infinity, so this and [`round_half_away_from_zero`] differ
/// on every negative half: `math_round(-0.5)` is `0` where `(-0.5).round()` is `-1`.
///
/// `EditSession.drawLine` interpolates with `Math.round`, so a descending line puts
/// every intermediate block one row higher than Rust's `f64::round` would.
#[must_use]
pub fn math_round(value: f64) -> i32 {
    // Clamped before the cast because Java's `(int)` saturates. A NaN falls through
    // both comparisons and casts to 0, which is also what Java does for it.
    (value + 0.5)
        .floor()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// An integer block position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockPos {
    /// Block X.
    pub x: i32,
    /// Block Y.
    pub y: i32,
    /// Block Z.
    pub z: i32,
}

impl BlockPos {
    /// A position from its three coordinates.
    #[must_use]
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// The position moved by the given deltas.
    #[must_use]
    pub const fn offset(self, dx: i32, dy: i32, dz: i32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.z + dz)
    }

    /// The same X and Z, at a different height.
    ///
    /// Named for the fact that only one axis moves: a command that finds a surface and
    /// places blocks above it is changing height and nothing else, and saying so makes it
    /// obvious the X and Z are the same values.
    #[must_use]
    pub const fn with_y(self, y: i32) -> Self {
        Self::new(self.x, y, self.z)
    }

    /// The coordinate-wise smaller of two positions.
    ///
    /// Mirrors `BlockVector3.getMinimum`, which is how a selection's two corners
    /// become a bounding box regardless of which was clicked first.
    #[must_use]
    pub fn min(self, other: Self) -> Self {
        Self::new(
            self.x.min(other.x),
            self.y.min(other.y),
            self.z.min(other.z),
        )
    }

    /// The coordinate-wise larger of two positions.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self::new(
            self.x.max(other.x),
            self.y.max(other.y),
            self.z.max(other.z),
        )
    }

    /// The product of the three components as a count of blocks.
    ///
    /// Used where a region's size has to become an allocation length, such as a
    /// clipboard's backing store.
    #[must_use]
    pub fn volume(self) -> usize {
        (self.x.max(0) as usize) * (self.y.max(0) as usize) * (self.z.max(0) as usize)
    }

    /// The chunk X this position falls in.
    ///
    /// Arithmetic shift, not division: Minecraft chunk coordinates are negative
    /// west and south of the origin, and `/` rounds towards zero where the floor
    /// is wanted. Matches `x >> ChunkStore.CHUNK_SHIFTS` in WorldEdit.
    #[must_use]
    pub const fn chunk_x(self) -> i32 {
        self.x >> 4
    }

    /// The chunk Z this position falls in.
    #[must_use]
    pub const fn chunk_z(self) -> i32 {
        self.z >> 4
    }
}

/// A double-precision vector with X and Z components.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec2 {
    /// The X component.
    pub x: f64,
    /// The Z component.
    pub z: f64,
}

impl Vec2 {
    /// A vector from its two components.
    #[must_use]
    pub const fn new(x: f64, z: f64) -> Self {
        Self { x, z }
    }

    /// Both components plus `amount`.
    ///
    /// Named after FAWE's `Vector2.add`, which adds a scalar to both axes.
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn add(self, amount: f64) -> Self {
        Self::new(self.x + amount, self.z + amount)
    }

    /// The component-wise larger of two vectors.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self::new(self.x.max(other.x), self.z.max(other.z))
    }

    /// This vector's components as a scalar multiplier, for the `1 / radius`
    /// factor both the cylinder and the ellipsoid divide by.
    #[must_use]
    pub fn reciprocal(self) -> Self {
        Self::new(1.0 / self.x, 1.0 / self.z)
    }
}

/// A double-precision vector with three components.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    /// The X component.
    pub x: f64,
    /// The Y component.
    pub y: f64,
    /// The Z component.
    pub z: f64,
}

impl Vec3 {
    /// A vector from its three components.
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    /// Every component plus `amount`.
    ///
    /// Named after FAWE's `Vector3.add`, which adds a scalar to all three axes.
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn add(self, amount: f64) -> Self {
        Self::new(self.x + amount, self.y + amount, self.z + amount)
    }

    /// The component-wise larger of two vectors.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self::new(
            self.x.max(other.x),
            self.y.max(other.y),
            self.z.max(other.z),
        )
    }

    /// This vector squared, component-wise.
    #[must_use]
    pub fn squared(self) -> Self {
        Self::new(self.x * self.x, self.y * self.y, self.z * self.z)
    }

    /// The component-wise reciprocal, for dividing out a radius.
    #[must_use]
    pub fn reciprocal(self) -> Self {
        Self::new(1.0 / self.x, 1.0 / self.y, 1.0 / self.z)
    }
}
