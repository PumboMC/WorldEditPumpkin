//! The plugin's shared state.
//!
//! One `OnceLock`, installed at load. Every command and event handler reads it
//! through here rather than holding a reference, because the plugin API hands out
//! `&self` to lifecycle callbacks and re-entrant callbacks must not deadlock.

use std::sync::{
    Mutex, MutexGuard, OnceLock,
    atomic::{AtomicU32, Ordering},
};

use pumpkin_plugin_api::Context;

use crate::{
    config::Config,
    session::{DEFAULT_HISTORY_SIZE, Sessions},
};

/// Everything the plugin holds while it is loaded.
pub struct State {
    config: Mutex<Config>,
    sessions: Mutex<Sessions>,
    /// The plugin's data folder, for `//schem`.
    ///
    /// Empty until `on_load` records it. Empty rather than a fallback path so that a
    /// caller before load gets a clear "no folder yet" rather than a host call made
    /// from a context it should not have.
    folder: Mutex<String>,
    /// The scheduler task that advances queued edits, cancelled on unload.
    ///
    /// An `AtomicU32` rather than part of the config mutex because it is written once
    /// at load and read once at unload, and neither is on a path where a lock is
    /// wanted.
    task: std::sync::atomic::AtomicU32,
}

/// No task has been registered yet.
///
/// Distinct from task zero, which is a real id: the host numbers from zero, so zero
/// cannot double as "none".
static NO_TASK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

/// The installed state.
///
/// `None` until `on_load` has run. A command cannot arrive before that, but a test
/// or an early callback can, and the accessors say so rather than panicking.
static STATE: OnceLock<State> = OnceLock::new();

impl State {
    /// A state with the given settings and no sessions.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config: Mutex::new(config),
            sessions: Mutex::new(Sessions::new()),
            folder: Mutex::new(String::new()),
            task: AtomicU32::new(NO_TASK.load(Ordering::Relaxed)),
        }
    }

    /// Records the plugin's data folder, for `//schem`.
    ///
    /// Held rather than asking the host each time because the `Context` `on_load` was
    /// given is not kept: a plugin holding a host handle past load is holding
    /// something it has no business holding.
    pub fn store_folder(&self, folder: String) {
        *self
            .folder
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = folder;
    }

    /// Records the scheduler task that advances queued edits.
    pub fn set_task(&self, task: u32) {
        self.task.store(task, Ordering::Relaxed);
    }

    /// The edit-queue task, or `None` if there is not one to cancel.
    ///
    /// Taken rather than read, so that two unloads cannot both try to cancel it.
    pub fn take_task(&self) -> Option<u32> {
        let previous = self
            .task
            .swap(NO_TASK.load(Ordering::Relaxed), Ordering::Relaxed);
        (previous != NO_TASK.load(Ordering::Relaxed)).then_some(previous)
    }

    /// The current settings.
    ///
    /// Cloned rather than borrowed because every caller here wants to hold the
    /// settings across a host call, and the lock must not be held then.
    #[must_use]
    pub fn config(&self) -> Config {
        self.config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replaces the settings, as a reload would.
    pub fn set_config(&self, config: Config) {
        *self.config.lock().unwrap_or_else(|e| e.into_inner()) = config;
    }
}

/// Installs the state. Returns `false` if it was already installed.
///
/// `on_load` runs once, so the `false` would mean something unexpected. It is
/// ignored rather than treated as an error so that a host that loads twice does not
/// fail to start.
pub fn install(state: State) -> bool {
    STATE.set(state).is_ok()
}

/// The installed state, if there is one.
#[must_use]
pub fn state() -> Option<&'static State> {
    STATE.get()
}

/// The current settings, whether or not the state is installed.
#[must_use]
pub fn config() -> Config {
    state().map_or_else(
        || Config {
            history_size: DEFAULT_HISTORY_SIZE,
            ..Config::default()
        },
        State::config,
    )
}

/// Every player's sessions.
pub fn sessions() -> MutexGuard<'static, Sessions> {
    lock(match state() {
        Some(state) => &state.sessions,
        None => fallback(),
    })
}

/// Forgets a player who left.
pub fn forget(player: &str) {
    sessions().remove(player);
}

/// The plugin's data folder, for `//schem`.
///
/// Empty before `on_load` has run, which cannot happen in practice: a command cannot
/// arrive before the plugin has loaded.
#[must_use]
pub fn data_folder() -> String {
    state().map(folder_of).unwrap_or_default()
}

/// The recorded data folder.
#[must_use]
fn folder_of(state: &State) -> String {
    state
        .folder
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Records the data folder.
pub fn set_folder(context: &Context) {
    let folder = context.get_data_folder();
    if let Some(state) = state() {
        state.store_folder(folder);
    }
}

/// Records the scheduler task that advances queued edits.
pub fn set_task(task: u32) {
    if let Some(state) = state() {
        state.set_task(task);
    }
}

/// The edit-queue task, so `on_unload` can cancel it.
pub fn take_task() -> Option<u32> {
    state().and_then(State::take_task)
}

/// Locks a mutex, recovering from a poisoned one.
///
/// A panic inside a command handler poisons the state lock, and every later command
/// would then fail. Recovering the data is better than refusing every command for
/// the rest of the process's life.
fn lock<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The sessions used before the state is installed, or if installing failed.
///
/// A `OnceLock` rather than a `static Mutex` because `Sessions::new` is not const
/// and an empty `HashMap` cannot be built in a static without one. Without this a
/// caller before `on_load` would have to unwrap the state at every call site, and
/// the alternative is panicking in a command handler.
static FALLBACK_SESSIONS: OnceLock<Mutex<Sessions>> = OnceLock::new();

/// The fallback sessions.
fn fallback() -> &'static Mutex<Sessions> {
    FALLBACK_SESSIONS.get_or_init(|| Mutex::new(Sessions::new()))
}
