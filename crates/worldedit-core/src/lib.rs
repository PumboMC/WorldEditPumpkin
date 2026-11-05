//! The WorldEdit engine, with no dependency on the server it runs on.
//!
//! This crate holds everything that decides *what* an edit does: which positions
//! a shape covers, which blocks a mask lets through, what a pattern places, and
//! what has to be recorded so the edit can be undone. The Pumpkin side supplies
//! two things and nothing else — an [`Extent`] over the world and a
//! [`BlockResolver`] over the server's block registry — through the traits in
//! [`block`] and [`extent`].
//!
//! Keeping the split here is what lets the parsing and geometry be tested on the
//! host target, rather than only through a Wasm guest inside a running server.
//!
//! ## What a command does
//!
//! 1. Parse the pattern and the mask against the server's registry, so an unknown
//!    block is reported before any of the world is touched.
//! 2. Build a position source: a [`region::RegionWalker`] for a selection, or a
//!    generator from [`shape`] for `//sphere` and friends.
//! 3. Hand both to an [`edit::EditJob`] and drive it with a per-tick block budget,
//!    so a large edit does not stall the server the way a single blocking call
//!    would.
//! 4. Push the resulting [`history::ChangeSet`] onto the player's history, from
//!    where `//undo` and `//redo` replay it.

#![warn(missing_docs)]

/// Block values and the registry hook that names them.
pub mod block;
/// The clipboard and its paste transform holder.
pub mod clipboard;
/// Resumable edits: `//set`, `//replace`, and the undo and redo replays.
pub mod edit;
/// The engine's view of a world.
pub mod extent;
/// Recorded edits and the per-player undo and redo stacks.
pub mod history;
/// Masks: which positions an edit may touch.
pub mod mask;
/// Blocks and vectors.
pub mod math;
/// Enough NBT to read and write a schematic.
pub mod nbt;
/// Block patterns: what an edit places.
pub mod pattern;
/// Selection shapes and the resumable walk over one.
pub mod region;

/// Moving and stacking a selection's own contents.
pub mod relocate;
/// Reading Sponge schematic v2 and v3 files and writing v3 files.
pub mod schematic;
/// The generated shapes behind `//sphere`, `//cyl` and `//line`.
pub mod shape;
/// Paste transforms: `//rotate` and `//flip`.
pub mod transform;
