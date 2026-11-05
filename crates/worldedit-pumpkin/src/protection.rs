//! WorldGuard write authorization. Activation is sticky for this server run:
//! if an activated provider fails/unloads, edits fail closed rather than bypass it.
//!
//! Wire v1, kind 4: player/name, level/name, dimension/name (u16 LE UTF-8 lengths),
//! then x/y/z i32 LE and action bits (1 = break, 2 = place).
//! Response is exactly [1, 1] for allow, [1, 0] for deny.

use std::sync::atomic::{AtomicBool, Ordering};
use worldedit_core::math::BlockPos;

static REQUIRED: AtomicBool = AtomicBool::new(false);

pub fn require_worldguard() {
    REQUIRED.store(true, Ordering::Release);
}

fn request(
    player: &str,
    level: &str,
    dimension: &str,
    position: BlockPos,
    actions: u8,
) -> Option<Vec<u8>> {
    let mut out = vec![1, 4];
    for value in [player, level, dimension] {
        let length = u16::try_from(value.len()).ok()?;
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    for coordinate in [position.x, position.y, position.z] {
        out.extend_from_slice(&coordinate.to_le_bytes());
    }
    out.push(actions);
    Some(out)
}

pub fn allows(
    player: Option<&str>,
    level: &str,
    dimension: &str,
    position: BlockPos,
    actions: u8,
) -> bool {
    if !REQUIRED.load(Ordering::Acquire) {
        return true;
    }
    let Some(message) =
        player.and_then(|player| request(player, level, dimension, position, actions))
    else {
        return false;
    };
    matches!(pumpkin_plugin_api::ipc::send_ipc_message("worldguard", &message), Ok(Ok(answer)) if answer == [1, 1])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authorization_wire_is_bounded_and_dimension_specific() {
        let request = request(
            "Alex",
            "world",
            "minecraft:the_nether",
            BlockPos::new(-1, 64, 20),
            3,
        )
        .unwrap();
        assert_eq!(&request[..8], &[1, 4, 4, 0, b'A', b'l', b'e', b'x']);
        assert!(
            request
                .windows(b"minecraft:the_nether".len())
                .any(|bytes| bytes == b"minecraft:the_nether")
        );
        assert_eq!(
            &request[request.len() - 13..request.len() - 9],
            &(-1_i32).to_le_bytes()
        );
        assert!(
            super::request(
                &"x".repeat(65536),
                "world",
                "minecraft:overworld",
                BlockPos::new(0, 0, 0),
                3
            )
            .is_none()
        );
    }
}
