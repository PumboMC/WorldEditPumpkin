//! Permission node names, and registering them with the host.
//!
//! Every node this plugin defines starts with `WorldEdit:`, and that is not
//! a style choice. `Context::register_permission` rejects any node that is not
//! prefixed with the plugin's metadata name followed by a colon, matched case
//! sensitively. A node outside that prefix is never registered, and an
//! unregistered node is denied to everyone at every op level, so the command behind
//! it reports *"Unknown command"* and looks like it does not exist.
//!
//! Note what `PluginMetadata::permissions` is *not*: it is the WASI capability
//! list, `fs.read.data` and friends. A node written there is a no-op.

use pumpkin_plugin_api::{
    Context,
    permission::{Permission, PermissionDefault, PermissionLevel},
};

/// The plugin name, and therefore the only namespace this plugin may register.
pub const NAMESPACE: &str = crate::PLUGIN;

/// Gates `/worldedit`, `/we`, `//*` and their aliases.
///
/// Spelled as literals because `concat!` needs literals, not consts. The test
/// below is what ties these strings back to [`NAMESPACE`].
pub const USE: &str = "WorldEdit:use";

/// The selection commands: `//pos1`, `//size`, `//outset` and the rest.
///
/// Separate from [`EDIT`] because selecting is not editing: a player who may only
/// be allowed to *look at* a region still needs to select it.
pub const SELECTION: &str = "WorldEdit:selection";

/// The commands that write blocks: `//set`, `//replace`, `//air` and the rest.
pub const EDIT: &str = "WorldEdit:edit";

/// The generated-shape commands: `//sphere`, `//cyl`, `//line` and the rest.
pub const SHAPE: &str = "WorldEdit:shape";

/// `//copy`, `//cut`, `//paste`, `//rotate`, `//flip`, `//clearclipboard`.
///
/// Separate from [`EDIT`] because a clipboard write is bounded by what was copied
/// rather than by `block-limit`, and an admin may reasonably want them apart.
pub const CLIPBOARD: &str = "WorldEdit:clipboard";

/// `//undo`, `//redo`, `//clearhistory`.
pub const HISTORY: &str = "WorldEdit:history";

/// `//gmask`, `//limit`, `//world`.
pub const LIMIT: &str = "WorldEdit:limit";

/// `//info`.
pub const INFO: &str = "WorldEdit:info";

/// Every node this plugin registers, with the command category it gates.
///
/// The pairs are `(node, description)`. Kept together so a new command cannot be
/// added to the tree without the node existing, which is the failure that makes a
/// command silently invisible to everybody.
pub const ALL: [(&str, &str); 8] = [
    (USE, "Allows using /worldedit and the // commands."),
    (
        SELECTION,
        "Allows selecting and adjusting a region with //pos1, //pos2, //sel, //size, //count, //chunk, \
         //outset, //inset, //contract and //shift.",
    ),
    (
        EDIT,
        "Allows changing blocks with //set, //air, //replace, //center, //walls and //faces.",
    ),
    (
        SHAPE,
        "Allows generating shapes with //sphere, //hsphere, //cyl, //hcyl and //line.",
    ),
    (
        CLIPBOARD,
        "Allows //copy, //cut, //paste, //rotate, //flip and //clearclipboard.",
    ),
    (HISTORY, "Allows //undo, //redo and //clearhistory."),
    (LIMIT, "Allows //gmask, //limit and //world."),
    (INFO, "Allows //info."),
];

/// Whether `node` is inside this plugin's namespace, and so registerable.
#[must_use]
pub fn is_own(node: &str) -> bool {
    node.starts_with(&format!("{NAMESPACE}:"))
}

/// Registers one node, logging a real failure and treating a repeat as success.
///
/// Registration is not idempotent on the host: a second call for the same node
/// returns `Err("Permission … is already registered")`. That is not fatal, so it
/// is treated as success, which is what lets the host load the plugin twice.
///
/// Returns whether the node is registered, so the caller can report a count
/// instead of leaving the admin to infer it from silence.
pub fn register(
    context: &Context,
    node: &str,
    description: &str,
    default: PermissionDefault,
) -> bool {
    match context.register_permission(&Permission {
        node: node.to_string(),
        description: description.to_string(),
        default,
        // Always empty on purpose. The host only reads `children` while walking a
        // player's *attachment* parents; the registry-default path reads `default`
        // and nothing else, so declaring children would have no effect on any
        // default and would read as if it did.
        children: Vec::new(),
    }) {
        Ok(()) => true,
        Err(message) if message.contains("already registered") => true,
        Err(message) => {
            // A namespace mistake lands here, and every command behind the node
            // becomes invisible to everybody, operators included. Loud is the only
            // safe response.
            tracing::error!("could not register {node}: {message}");
            false
        }
    }
}

/// Registers every node in [`ALL`], and returns how many are live.
///
/// The namespace has to match the plugin's metadata name or the host refuses every
/// one of them, which is why this is called from `on_load` rather than lazily.
#[must_use]
pub fn register_all(context: &Context) -> usize {
    ALL.iter()
        .filter(|(node, description)| register(context, node, description, op_two()))
        .count()
}

/// The default for a node an operator should hold.
///
/// Operator level two, so a plain survival player gets none of them. `//set` and
/// `//undo` hand out the ability to rewrite and revert a build.
#[must_use]
pub fn op_two() -> PermissionDefault {
    PermissionDefault::Op(PermissionLevel::Two)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_plugin_api::Plugin;

    /// Every node this plugin registers is inside its own namespace.
    ///
    /// The host enforces this with an error, and the failure is invisible in game:
    /// the command reports "Unknown command" for everybody, operators included.
    #[test]
    fn every_node_this_plugin_registers_is_in_its_own_namespace() {
        assert_eq!(NAMESPACE, crate::PLUGIN);
        for (node, _) in ALL {
            assert!(is_own(node), "{node} is outside the {NAMESPACE}: namespace");
            assert!(
                node.starts_with(&format!("{NAMESPACE}:")),
                "{node} does not start with the plugin name"
            );
        }
    }

    /// The host's namespace check is against `metadata.name`, not against a
    /// constant, so these two have to be the same string.
    #[test]
    fn the_metadata_name_is_the_namespace() {
        assert_eq!(crate::WorldEdit::new().metadata().name, "WorldEdit");
        assert_eq!(NAMESPACE, "WorldEdit");
    }

    /// The prefix check is case sensitive on the host, so our own namespace
    /// spelled any other way is foreign as far as registration is concerned.
    #[test]
    fn a_mistyped_namespace_is_not_recognised_as_ours() {
        assert!(!is_own("worldedit:use"));
        assert!(!is_own("WorldEditPumpkin:use"));
        assert!(!is_own("WorldEditUse"));
        assert!(!is_own("minecraft:command.stop"));
    }

    /// No two categories may share a node, or granting one would silently grant the
    /// other.
    #[test]
    fn no_two_categories_share_a_node() {
        let mut seen: Vec<&str> = ALL.iter().map(|(node, _)| *node).collect();
        let count = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), count, "a node is registered twice");
    }
}
