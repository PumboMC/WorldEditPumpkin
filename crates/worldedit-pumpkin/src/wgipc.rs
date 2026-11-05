//! WorldGuard's selection exchange, in both directions.
//!
//! WorldGuard creates regions from the player's current selection, which lives
//! in WorldEdit's per-player session, and `/rg select` hands a region back as a
//! selection. The two plugins cannot share a Rust type —
//! each is compiled to its own WebAssembly component, and Pumpkin's plugin API
//! has no way to link them — so the exchange goes through the host's IPC
//! interface as raw bytes.
//!
//! ## Wire format
//!
//! Hand-rolled rather than derived from `serde`, for two reasons. The two
//! plugins are versioned independently and are frequently built against
//! different `pumpkin-plugin-api` versions, so a derived format would break on a
//! dependency bump rather than on a real protocol change. And a derived format
//! puts the compatibility question in the compiler's hands, where it belongs; a
//! one-byte version prefix puts it in the decoder.
//!
//! All integers are little-endian.
//!
//! ```text
//! Request:  version:u8  kind:u8  <kind payload>
//!   kind 1 (SELECTION)     player:u16-len  player-name:utf8
//!   kind 2 (SET_SELECTION) player:u16-len  player-name:utf8
//!                          world:u16-len  world-name:utf8
//!                          min:x:i32 y:i32 z:i32
//!                          max:x:i32 y:i32 z:i32
//!
//! Response: version:u8  status:u8  <status payload>
//!   status 0 (NO_SELECTION)
//!   status 1 (SELECTION)  world:u16-len  world-name:utf8
//!                         min:x:i32 y:i32 z:i32
//!                         max:x:i32 y:i32 z:i32
//!   status 2 (APPLIED)
//! ```
//!
//! `PROTOCOL_VERSION` is the version byte this build speaks, and
//! [`decode_request`] rejects anything else rather than guessing. Adding a
//! request kind or a status does not need a new version: an older peer rejects
//! the unknown kind outright, which is the right outcome, whereas bumping the
//! version would break the *other* direction too.
//!
//! ## Why coordinates are inclusive
//!
//! Both ends are inclusive, in both directions of the exchange.
//!
//! WorldEdit records `pos1` and `pos2` as the blocks the player clicked, so the maximum is
//! the last block of the selection rather than one past it. WorldGuard's own regions are
//! inclusive the same way — `ProtectedCuboidRegion#contains` reads `x >= min.x && x < max.x + 1`
//! — and `RegionCommandsBase#checkRegionFromSelection` hands a selection's two corners
//! straight to `new ProtectedCuboidRegion(...)` with nothing added to either. So the two
//! agree by construction and neither side may widen.
//!
//! Getting this wrong silently produces a region one block too big on every face, which is
//! why it is called out here rather than left to the caller.
//!
//! `__global__` is the one place the two conventions meet: it is reserved as a region *id*,
//! and WorldGuard's `RegionCommandsBase#checkRegionId` refuses it for any command that takes
//! an id a player chooses.

use crate::state;
use worldedit_core::math::BlockPos;

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u8 = 1;

/// The only plugin allowed to ask.
pub const WORLDGUARD: &str = "worldguard";

/// Request kinds.
mod kind {
    /// "What is this player's selection?"
    pub const SELECTION: u8 = 1;
    /// "Give this player this selection."
    pub const SET_SELECTION: u8 = 2;
}

/// Response statuses.
mod status {
    /// The player has no complete selection.
    pub const NO_SELECTION: u8 = 0;
    /// The selection, with inclusive bounds.
    pub const SELECTION: u8 = 1;
    /// A set request was carried out.
    ///
    /// The only status a set request can answer with. WorldEdit's own
    /// `sessionManager.get(actor)` creates the session rather than refusing an unknown
    /// player, so a set cannot fail once the request has decoded; there is no second
    /// status to spell.
    ///
    /// Numbered past the selection statuses rather than reusing them: the two answers
    /// mean opposite things, and a stale client that read the wrong one would either
    /// report a selection that was never sent or claim a set failed when it succeeded.
    pub const APPLIED: u8 = 2;
}

/// A decoded request.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    /// Report the selection belonging to this player name.
    Selection {
        /// The player whose selection is wanted.
        player: String,
    },
    /// Give a player a selection, which is what `/rg select` asks for.
    SetSelection {
        /// The player to give it to.
        player: String,
        /// The world to make the selection in.
        world: String,
        /// Inclusive lower bound.
        min: (i32, i32, i32),
        /// Inclusive upper bound.
        max: (i32, i32, i32),
    },
}

/// Append a length-prefixed UTF-8 string.
fn put_str(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    // Truncate on a char boundary rather than emitting invalid UTF-8: a player
    // name that long cannot exist, so the limit is a guard against corruption
    // rather than a real constraint.
    let mut end = bytes.len().min(u16::MAX as usize);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    out.extend_from_slice(&(end as u16).to_le_bytes());
    out.extend_from_slice(&bytes[..end]);
}

/// Read a length-prefixed UTF-8 string, or `None` if the buffer is short.
fn take_str(input: &[u8]) -> Option<String> {
    if input.len() < 2 {
        return None;
    }
    let len = u16::from_le_bytes([input[0], input[1]]) as usize;
    let body = input.get(2..2 + len)?;
    Some(String::from_utf8_lossy(body).into_owned())
}

/// Decode a request, or explain why it could not be read.
pub fn decode_request(message: &[u8]) -> Result<Request, String> {
    let (version, rest) = message
        .split_first()
        .ok_or_else(|| "empty request".to_string())?;
    if *version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version {version}, this build speaks {PROTOCOL_VERSION}"
        ));
    }
    let (kind, rest) = rest
        .split_first()
        .ok_or_else(|| "request has no kind".to_string())?;
    match *kind {
        kind::SELECTION => Ok(Request::Selection {
            player: take_str(rest).ok_or_else(|| "truncated player name".to_string())?,
        }),
        kind::SET_SELECTION => {
            let player = take_str(rest).ok_or_else(|| "truncated player name".to_string())?;
            let rest = rest.get(2 + player.len()..).unwrap_or_default();
            let world = take_str(rest).ok_or_else(|| "truncated world name".to_string())?;
            let body = rest.get(2 + world.len()..).unwrap_or_default();
            if body.len() < 24 {
                return Err("truncated selection bounds".to_string());
            }
            let coord = |offset: usize| -> i32 {
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(&body[offset..offset + 4]);
                i32::from_le_bytes(bytes)
            };
            Ok(Request::SetSelection {
                player,
                world,
                min: (coord(0), coord(4), coord(8)),
                max: (coord(12), coord(16), coord(20)),
            })
        }
        other => Err(format!("unknown request kind {other}")),
    }
}

/// Encode a request.
pub fn encode_request(request: &Request) -> Vec<u8> {
    let mut out = vec![PROTOCOL_VERSION];
    match request {
        Request::Selection { player } => {
            out.push(kind::SELECTION);
            put_str(&mut out, player);
        }
        Request::SetSelection {
            player,
            world,
            min,
            max,
        } => {
            out.push(kind::SET_SELECTION);
            put_str(&mut out, player);
            put_str(&mut out, world);
            for (x, y, z) in [min, max] {
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
                out.extend_from_slice(&z.to_le_bytes());
            }
        }
    }
    out
}

/// The player's current selection, in WorldEdit's inclusive coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The world the selection was made in, if the session recorded one.
    pub world: Option<String>,
    /// Inclusive lower bound.
    pub min: (i32, i32, i32),
    /// Inclusive upper bound.
    pub max: (i32, i32, i32),
}

/// Answer a request.
///
/// Takes the session lock once for the whole call rather than per accessor, so a
/// selection cannot change between reading `pos1` and `pos2`.
pub fn handle(request: &Request) -> Vec<u8> {
    let mut out = vec![PROTOCOL_VERSION];

    match request {
        Request::Selection { player } => {
            let sessions = state::sessions();
            let Some(session) = sessions.get(player) else {
                out.push(status::NO_SELECTION);
                return out;
            };
            // `selection()` is the same path `/sel` and the wand use, so what WorldGuard
            // sees is exactly what the player sees.
            let Some(region) = session.selection() else {
                out.push(status::NO_SELECTION);
                return out;
            };
            let min = region.minimum();
            let max = region.maximum();

            out.push(status::SELECTION);
            put_str(&mut out, session.edit_world().unwrap_or_default());
            for (x, y, z) in [(min.x, min.y, min.z), (max.x, max.y, max.z)] {
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
                out.extend_from_slice(&z.to_le_bytes());
            }
        }
        Request::SetSelection {
            player,
            world,
            min,
            max,
        } => {
            let mut sessions = state::sessions();
            // `entry`, not `get_mut`: a player who has never touched the wand still needs
            // somewhere to keep the selection, and `/rg select` is how a lot of players
            // get their first one. Creating the session here is what WorldEdit's own
            // `sessionManager.get(actor)` does, so a later `/rg claim` finds it.
            let session = sessions.entry(player);
            session.set_edit_world(world.clone());
            session.set_pos1(BlockPos::new(min.0, min.1, min.2));
            session.set_pos2(BlockPos::new(max.0, max.1, max.2));
            out.push(status::APPLIED);
        }
    }
    out
}

/// Decode a response to [`Request::Selection`].
///
/// Returns `Ok(None)` when the player had no selection, which is an ordinary
/// outcome and not an error.
pub fn decode_selection(message: &[u8]) -> Result<Option<Selection>, String> {
    let (version, rest) = message
        .split_first()
        .ok_or_else(|| "empty response".to_string())?;
    if *version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version {version}, this build speaks {PROTOCOL_VERSION}"
        ));
    }
    let (status, rest) = rest
        .split_first()
        .ok_or_else(|| "response has no status".to_string())?;
    match *status {
        status::NO_SELECTION => Ok(None),
        status::SELECTION => {
            let world = take_str(rest).ok_or_else(|| "truncated world name".to_string())?;
            let body = &rest[2 + world.len()..];
            if body.len() < 24 {
                return Err("truncated selection bounds".to_string());
            }
            let coord = |offset: usize| -> i32 {
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(&body[offset..offset + 4]);
                i32::from_le_bytes(bytes)
            };
            Ok(Some(Selection {
                world: if world.is_empty() { None } else { Some(world) },
                min: (coord(0), coord(4), coord(8)),
                max: (coord(12), coord(16), coord(20)),
            }))
        }
        other => Err(format!("unknown status {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selection_request_round_trips() {
        let request = Request::Selection {
            player: "Steve".into(),
        };
        let bytes = encode_request(&request);
        assert_eq!(decode_request(&bytes), Ok(request));
    }

    #[test]
    fn a_request_is_the_exact_bytes_worldguard_sends() {
        // Generated from WorldGuard's encoder. If either side changes the
        // format, this fails before a player does.
        assert_eq!(
            encode_request(&Request::Selection {
                player: "Steve".into()
            }),
            vec![1, 1, 5, 0, 83, 116, 101, 118, 101]
        );
    }

    #[test]
    fn a_non_ascii_player_name_round_trips() {
        let request = Request::Selection {
            player: "Świątopełć".into(),
        };
        assert_eq!(decode_request(&encode_request(&request)), Ok(request));
    }

    #[test]
    fn a_foreign_protocol_version_is_rejected() {
        let mut bytes = encode_request(&Request::Selection {
            player: "Steve".into(),
        });
        bytes[0] = 99;
        assert!(decode_request(&bytes).is_err());
    }

    #[test]
    fn a_truncated_request_is_rejected() {
        let bytes = encode_request(&Request::Selection {
            player: "Steve".into(),
        });
        assert!(decode_request(&bytes[..bytes.len() - 2]).is_err());
        assert!(decode_request(&[]).is_err());
        assert!(decode_request(&[PROTOCOL_VERSION]).is_err());
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        assert!(decode_request(&[PROTOCOL_VERSION, 200]).is_err());
    }

    #[test]
    fn a_selection_response_round_trips_with_inclusive_bounds() {
        let mut bytes = vec![PROTOCOL_VERSION, status::SELECTION];
        put_str(&mut bytes, "world");
        for (x, y, z) in [(1i32, 2i32, 3i32), (10i32, 20i32, 30i32)] {
            bytes.extend_from_slice(&x.to_le_bytes());
            bytes.extend_from_slice(&y.to_le_bytes());
            bytes.extend_from_slice(&z.to_le_bytes());
        }
        let decoded = decode_selection(&bytes).unwrap().unwrap();
        assert_eq!(decoded.world.as_deref(), Some("world"));
        assert_eq!(decoded.min, (1, 2, 3));
        assert_eq!(decoded.max, (10, 20, 30));
    }

    #[test]
    fn no_selection_is_an_ordinary_outcome() {
        assert_eq!(
            decode_selection(&[PROTOCOL_VERSION, status::NO_SELECTION]),
            Ok(None)
        );
    }

    #[test]
    fn an_odd_corner_selection_decodes_as_worldguard_wrote_it() {
        // WorldGuard's own `cuboid_bounds` output, fed back through the decoder.
        // The single-block case is the one that exposes an off-by-one.
        let mut bytes = vec![PROTOCOL_VERSION, status::SELECTION];
        put_str(&mut bytes, "world");
        for (x, y, z) in [(5i32, 64i32, 5i32), (6i32, 65i32, 6i32)] {
            bytes.extend_from_slice(&x.to_le_bytes());
            bytes.extend_from_slice(&y.to_le_bytes());
            bytes.extend_from_slice(&z.to_le_bytes());
        }
        let decoded = decode_selection(&bytes).unwrap().unwrap();
        assert_eq!(decoded.min, (5, 64, 5));
        assert_eq!(decoded.max, (6, 65, 6));
    }

    #[test]
    fn a_truncated_selection_response_is_rejected() {
        let mut bytes = vec![PROTOCOL_VERSION, status::SELECTION];
        put_str(&mut bytes, "world");
        bytes.extend_from_slice(&[0; 12]);
        assert!(decode_selection(&bytes).is_err());
    }

    #[test]
    fn a_set_selection_request_round_trips() {
        let request = Request::SetSelection {
            player: "Steve".into(),
            world: "world".into(),
            min: (1, 2, 3),
            max: (4, 5, 6),
        };
        assert_eq!(decode_request(&encode_request(&request)), Ok(request));
    }

    #[test]
    fn a_set_selection_request_is_the_exact_bytes_worldguard_sends() {
        // The mirror of WorldGuard's `the_set_request_is_the_exact_bytes_worldedit_accepts`.
        // Both sides pin the same bytes, so a change to one without the other fails here
        // rather than at a player.
        assert_eq!(
            encode_request(&Request::SetSelection {
                player: "Steve".into(),
                world: "world".into(),
                min: (1, 2, 3),
                max: (4, 5, 6),
            }),
            vec![
                1, 2, // version, kind
                5, 0, 83, 116, 101, 118, 101, // "Steve"
                5, 0, 119, 111, 114, 108, 100, // "world"
                1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, // min
                4, 0, 0, 0, 5, 0, 0, 0, 6, 0, 0, 0, // max
            ]
        );
    }

    #[test]
    fn a_truncated_set_selection_request_is_rejected() {
        let bytes = encode_request(&Request::SetSelection {
            player: "Steve".into(),
            world: "world".into(),
            min: (1, 2, 3),
            max: (4, 5, 6),
        });
        // Cutting anywhere inside the name, the world or the bounds must not be read as a
        // short-but-valid request: the trailing coordinates would be missing and the
        // selection would silently land at zero.
        for cut in 1..bytes.len() {
            assert!(
                decode_request(&bytes[..cut]).is_err(),
                "a request cut to {cut} bytes was accepted"
            );
        }
    }

    #[test]
    fn setting_a_selection_gives_the_player_exactly_those_bounds() {
        // `state::sessions()` falls back to a global when no state is installed, so this
        // exercises the real handler. The player name is unique to this test because that
        // global is shared by every test in the crate.
        let answer = handle(&Request::SetSelection {
            player: "BoundsProbe".into(),
            world: "bounds_world".into(),
            min: (-3, 64, 7),
            max: (9, 71, 7),
        });
        assert_eq!(answer, vec![PROTOCOL_VERSION, status::APPLIED]);

        let sessions = state::sessions();
        let session = sessions
            .get("BoundsProbe")
            .expect("the session was created");
        assert_eq!(session.edit_world(), Some("bounds_world"));
        let region = session.selection().expect("both points were set");
        assert_eq!(
            (region.minimum().x, region.minimum().y, region.minimum().z),
            (-3, 64, 7)
        );
        assert_eq!(
            (region.maximum().x, region.maximum().y, region.maximum().z),
            (9, 71, 7)
        );
    }

    #[test]
    fn a_selection_can_then_be_read_back_through_the_original_query() {
        // The round trip that matters: what `/rg select` sets is what `/rg claim` reads.
        // Without this, a set that only half-worked would still answer APPLIED.
        handle(&Request::SetSelection {
            player: "RoundTripProbe".into(),
            world: "world".into(),
            min: (1, 2, 3),
            max: (4, 5, 6),
        });
        let response = handle(&Request::Selection {
            player: "RoundTripProbe".into(),
        });
        let decoded = decode_selection(&response)
            .expect("a selection follows")
            .expect("some");
        assert_eq!(decoded.world.as_deref(), Some("world"));
        assert_eq!(decoded.min, (1, 2, 3));
        assert_eq!(decoded.max, (4, 5, 6));
    }

    #[test]
    fn setting_a_selection_creates_a_session_for_a_player_who_had_none() {
        // A player who has never touched the wand still needs somewhere to keep it: this
        // is how many players get their first selection.
        {
            let sessions = state::sessions();
            assert!(sessions.get("NeverSeenProbe").is_none());
        }
        handle(&Request::SetSelection {
            player: "NeverSeenProbe".into(),
            world: "world".into(),
            min: (0, 0, 0),
            max: (0, 0, 0),
        });
        let sessions = state::sessions();
        assert!(sessions.get("NeverSeenProbe").is_some());
    }
}
