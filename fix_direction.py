"""Fix `parse_direction`: three words were never implemented, and one test
assertion contradicted the function's documented convention.

The tests had never run -- the test build did not compile until the
format-string error was fixed -- so three gaps sat behind the first failing
assertion in each test:

  * Single-letter cardinals. `name.len() >= 2` refused `n`, `s`, `e` and `w`,
    which are the spellings WorldEdit and FAWE accept. The stated reason was
    that "one letter could be north or northeast", but the diagonals are
    matched by an earlier exact arm, and no cardinal is a prefix of another
    cardinal, so a bare letter is unambiguous here.
  * `up` and `down`. Only the compound forms `upleft`/`downright` and so on
    were handled.
  * `aim`, FAWE's name for the direction the player faces.

One assertion was wrong rather than the code: `parse_direction("aim", 0.0)`
expected north. `facing` is documented as yaw 0 being south, the sibling test
says the same in prose, and the other two `aim` assertions follow `facing`
exactly -- 316 degrees and -90 degrees both land where `facing` puts them. Only
that one assertion pointed the other way, so it is corrected rather than the
convention.
"""

import io

p = r"C:\dBackup\personal\work\WorldEdit\crates\worldedit-pumpkin\src\commands.rs"
s = io.open(p, encoding="utf-8").read()

# --- `aim` is another spelling of "the way the player faces" -----------------
old = '        "me" | "m" | "f" | "forward" => Some(facing(yaw)),'
new = '        "me" | "aim" | "m" | "f" | "forward" => Some(facing(yaw)),'
assert old in s
s = s.replace(old, new, 1)

# --- `up` and `down` on their own, not only compounded with a diagonal -------
old = '        "downleft" | "downright" | "upleft" | "upright" => Some(if name.starts_with(\'d\') {'
new = ('        "up" | "u" | "down" | "d" | "downleft" | "downright" | "upleft" | "upright"\n'
       '        => Some(if name.starts_with(\'d\') {')
assert old in s
s = s.replace(old, new, 1)

# --- single-letter cardinals -----------------------------------------------
old = """        _ => CARDINALS
            .iter()
            // Prefixes are accepted, as FAWE's table does: `nor` is `north`. Only the
            // ones that stay unambiguous are listed, because `n` alone could be north
            // or northeast.
            .find(|(cardinal, _)| cardinal.starts_with(&name) && name.len() >= 2)
            .map(|(_, signs)| *signs),"""
new = """        _ => CARDINALS
            .iter()
            // Prefixes are accepted, as FAWE's table does: `nor` is `north`, and `n`
            // is north. This stays unambiguous because the diagonals were matched by
            // the exact arm above, and no cardinal is a prefix of another cardinal.
            .find(|(cardinal, _)| !name.is_empty() && cardinal.starts_with(&name))
            .map(|(_, signs)| *signs),"""
assert old in s
s = s.replace(old, new, 1)

# --- the assertion that contradicted the convention -------------------------
old = '        assert_eq!(parse_direction("aim", 0.0), Some((0, 0, -1)), "me");'
new = """        // Yaw 0 faces south. The other two `aim` assertions in
        // `a_relative_direction_follows_the_players_yaw` already follow
        // `facing`, so this one pointed the other way.
        assert_eq!(parse_direction("aim", 0.0), Some((0, 0, 1)), "yaw 0 is south");"""
assert old in s
s = s.replace(old, new, 1)

io.open(p, "w", encoding="utf-8", newline="\n").write(s)
print("ok")