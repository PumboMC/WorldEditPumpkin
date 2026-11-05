"""Correct three assertions that contradict the rest of the suite.

`a_relative_direction_follows_the_players_yaw` and
`a_relative_name_is_an_offset_from_the_facing` assert *opposite* results for the
identical input `parse_direction("e", 90.0)`: the first wants west, the second
wants east. They cannot both hold.

The implementation returns east, and it is the one that is right:

  * WorldEdit's `MultiDirection` treats the compass letters as fixed. Only
    `me`/`aim`/`f`/`forward`/`b`/`back`/`l`/`left`/`r`/`right` resolve against
    the player's yaw.
  * The other three assertions in the second test are all consistent with fixed
    cardinals.
  * The implementation's own `facing` matches the two `aim` assertions that
    `a_relative_direction_follows_the_players_yaw` gets right (316 and -90
    degrees).

The three lines below read as if `e` and `b` were both `facing(yaw)`: `e` at 90
degrees gives the facing rather than east, and `b` at 180 and 270 give the
facing rather than its opposite. They are corrected to the real semantics, with
the contradiction recorded so the next reader does not have to re-derive it.
"""

import io

p = r"C:\dBackup\personal\work\WorldEdit\crates\worldedit-pumpkin\src\commands.rs"
s = io.open(p, encoding="utf-8").read()

old = """        assert_eq!(
            parse_direction("e", 90.0),
            Some((-1, 0, 0)),
            "a quarter turn anticlockwise is west"
        );
        assert_eq!(
            parse_direction("b", 180.0),
            Some((0, 0, -1)),
            "the opposite of the way you face"
        );
        assert_eq!(
            parse_direction("b", 270.0),
            Some((1, 0, 0)),
            "back at 270 degrees is east"
        );"""
new = """        // The compass letters are fixed, not relative: this used to expect
        // west, which contradicted `a_relative_name_is_an_offset_from_the_facing`
        // asserting east for the very same input.
        assert_eq!(
            parse_direction("e", 90.0),
            Some((1, 0, 0)),
            "east is east whatever the player is facing"
        );
        // `b` is the opposite of the facing, so it is 180 degrees round from
        // `facing(yaw)`. These used to assert the facing itself.
        assert_eq!(
            parse_direction("b", 180.0),
            Some((0, 0, 1)),
            "facing north at 180 degrees, so back is south"
        );
        assert_eq!(
            parse_direction("b", 270.0),
            Some((-1, 0, 0)),
            "facing west at 270 degrees, so back is east"
        );"""
assert old in s
s = s.replace(old, new, 1)

io.open(p, "w", encoding="utf-8", newline="\n").write(s)
print("ok")