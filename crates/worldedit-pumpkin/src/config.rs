//! The plugin's `config.yml`, and the settings read out of it.

use pumpkin_plugin_api::{Context, permissions};

/// The settings that change behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// How many edits a player can undo.
    pub history_size: usize,
    /// How many blocks one edit may change before it is refused.
    ///
    /// This is `worldedit.edit.block-limit`. Counted in blocks actually changed,
    /// not positions visited, so a `//replace` over a huge selection with few
    /// matches still succeeds.
    pub block_limit: Option<i64>,
    /// How many blocks a single `//set`-style command may change per tick.
    ///
    /// FAWE spreads an edit over time rather than blocking the server tick. A Wasm
    /// guest cannot reach Pumpkin's worker threads, so the same idea is expressed
    /// as a per-tick budget driven by the plugin scheduler.
    pub blocks_per_tick: usize,
    /// The world's height in blocks.
    ///
    /// The plugin WIT exposes `get-min-y` but not `get-max-y`, so this cannot be
    /// read from the host. It defaults to the overworld's 384 and has to be raised
    /// for a datapack that changes the height.
    pub world_height: i32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            history_size: 15,
            block_limit: Some(0),
            blocks_per_tick: 10_000,
            world_height: 384,
        }
    }
}

impl Config {
    /// Reads the settings from the data folder, falling back to the defaults.
    ///
    /// A malformed file is not fatal: the plugin loads with the defaults and says
    /// so. Refusing to load would leave an admin with no WorldEdit at all because
    /// of one bad line, which is worse than a logged warning.
    #[must_use]
    pub fn load(folder: &str) -> (Self, Vec<String>) {
        let mut config = Self::default();
        let mut problems = Vec::new();

        let path = format!("{folder}/config.yml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return (config, problems);
        };

        for (number, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                problems.push(format!("line {}: expected `key: value`", number + 1));
                continue;
            };
            let key = key.trim();
            let value = value.trim();

            match key {
                "history-size" => match value.parse() {
                    Ok(size) => config.history_size = size,
                    Err(_) => {
                        problems.push(format!("line {}: '{value}' is not a number", number + 1))
                    }
                },
                "block-limit" => match value.parse() {
                    // Zero means unlimited, which is `//limit 0`.
                    Ok(limit) => config.block_limit = Some(limit),
                    Err(_) => {
                        problems.push(format!("line {}: '{value}' is not a number", number + 1))
                    }
                },
                "blocks-per-tick" => match value.parse() {
                    Ok(budget) if budget > 0 => config.blocks_per_tick = budget,
                    _ => problems.push(format!(
                        "line {}: '{value}' is not a positive number of blocks",
                        number + 1
                    )),
                },
                "world-height" => match value.parse() {
                    Ok(height) if height > 0 => config.world_height = height,
                    _ => problems.push(format!(
                        "line {}: '{value}' is not a positive height",
                        number + 1
                    )),
                },
                other => problems.push(format!("line {}: unknown setting '{other}'", number + 1)),
            }
        }

        (config, problems)
    }

    /// The block limit as the engine wants it, where `None` means no limit.
    #[must_use]
    pub fn block_limit(&self) -> Option<usize> {
        match self.block_limit {
            Some(limit) if limit > 0 => usize::try_from(limit).ok(),
            _ => None,
        }
    }

    /// The default config file, written on first run.
    #[must_use]
    pub fn template() -> &'static str {
        "\
# WorldEdit for Pumpkin

# How many edits a player can undo. Counts edits, not blocks, so a player who
# fills a world twice loses the first fill rather than losing undo entirely.
history-size: 15

# The most blocks one command may change. 0 means no limit.
block-limit: 0

# How many blocks one command may change per server tick. Lower this if edits
# make the server stutter; raise it to finish a large selection sooner.
blocks-per-tick: 10000

# The world's height in blocks. The server reports its floor but not its ceiling,
# so this has to match the dimension: the overworld is 384. Raise it for a
# datapack that changes the height.
world-height: 384
"
    }
}

/// Writes the default config if it is not already there, and says whether it did.
///
/// Returns `Err` with the reason rather than panicking, so `on_load` can report a
/// filesystem problem the admin can actually act on.
pub fn write_default(folder: &str) -> Result<bool, String> {
    let path = format!("{folder}/config.yml");
    if std::path::Path::new(&path).exists() {
        return Ok(false);
    }
    std::fs::write(&path, Config::template())
        .map_err(|error| format!("could not write {path}: {error}"))?;
    Ok(true)
}

/// The capability list this plugin needs.
///
/// `fs.read.data` and `fs.write.data` are the data folder, which is where the
/// config lives. Nothing else is requested: the plugin opens no sockets and reads
/// no environment.
#[must_use]
pub fn capabilities() -> Vec<String> {
    vec![
        permissions::FS_READ_DATA.to_string(),
        permissions::FS_WRITE_DATA.to_string(),
    ]
}

/// The plugin's data folder, from the context.
#[must_use]
pub fn data_folder(context: &Context) -> String {
    context.get_data_folder()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line the file gets wrong has to be reported by line number, so the admin
    /// can find it, rather than silently leaving the default in place.
    #[test]
    fn a_bad_line_is_reported_rather_than_silently_defaulted() {
        let folder = std::env::temp_dir().join("worldedit-config-test");
        std::fs::create_dir_all(&folder).expect("a temp folder");
        let path = folder.join("config.yml");
        std::fs::write(&path, "history-size: not-a-number\n").expect("a config");

        let (_, problems) = Config::load(folder.to_str().expect("a utf-8 path"));
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("line 1"), "{problems:?}");
        std::fs::remove_dir_all(&folder).ok();
    }

    /// An unknown key is a typo, and saying so is the whole point of reading the
    /// file at all.
    #[test]
    fn an_unknown_setting_is_reported() {
        let folder = std::env::temp_dir().join("worldedit-config-unknown");
        std::fs::create_dir_all(&folder).expect("a temp folder");
        let path = folder.join("config.yml");
        std::fs::write(&path, "historysize: 5\n").expect("a config");

        let (_, problems) = Config::load(folder.to_str().expect("a utf-8 path"));
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("historysize"), "{problems:?}");
        std::fs::remove_dir_all(&folder).ok();
    }

    /// Comments and blank lines are ignored, because the shipped file has both.
    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let folder = std::env::temp_dir().join("worldedit-config-comments");
        std::fs::create_dir_all(&folder).expect("a temp folder");
        let path = folder.join("config.yml");
        std::fs::write(&path, "# a comment\n\nhistory-size: 3\n").expect("a config");

        let (config, problems) = Config::load(folder.to_str().expect("a utf-8 path"));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(config.history_size, 3);
        std::fs::remove_dir_all(&folder).ok();
    }

    /// `block-limit: 0` means unlimited in WorldEdit, and has to mean the same
    /// thing here or `//limit 0` would freeze every admin out of editing.
    #[test]
    fn a_zero_block_limit_means_no_limit() {
        assert_eq!(Config::default().block_limit(), None);
        assert_eq!(
            Config {
                block_limit: Some(1000),
                ..Config::default()
            }
            .block_limit(),
            Some(1000)
        );
    }

    /// The shipped template has to parse back to the defaults, or the file the
    /// plugin writes would immediately disagree with what it just used.
    #[test]
    fn the_template_parses_back_to_the_defaults() {
        let folder = std::env::temp_dir().join("worldedit-config-template");
        std::fs::create_dir_all(&folder).expect("a temp folder");
        std::fs::write(folder.join("config.yml"), Config::template()).expect("a config");

        let (config, problems) = Config::load(folder.to_str().expect("a utf-8 path"));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(config, Config::default());
        std::fs::remove_dir_all(&folder).ok();
    }
}
