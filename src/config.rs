// SPDX-License-Identifier: GPL-3.0-only

//! The settings, in `~/.config/cosmic/io.github.DrakRosmann.CosmicHotEdge/v1/`,
//! written by the app and watched by the daemon. They are Hot Edge's
//! (`org.gnome.shell.extensions.hotedge`), with the same defaults, plus which
//! edge and what it opens, since COSMIC has no single overview.

use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};
use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "io.github.DrakRosmann.CosmicHotEdge";
pub const DAEMON: &str = "cosmic-ext-hot-edge-daemon";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    Top,
    #[default]
    Bottom,
    Left,
    Right,
}

/// What pushing into the edge does. Each opens what it names, or closes it if
/// it is open, as their keyboard shortcuts do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// COSMIC's overview (Super+W).
    #[default]
    Workspaces,
    /// Super+A.
    AppLibrary,
    /// Super.
    Launcher,
    Command(String),
}

impl Action {
    pub fn command(&self) -> &str {
        match self {
            Action::Workspaces => "cosmic-workspaces",
            Action::AppLibrary => "cosmic-app-library",
            Action::Launcher => "cosmic-launcher",
            Action::Command(command) => command,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CosmicConfigEntry)]
#[version = 1]
pub struct Config {
    /// The daemon keeps running when off, without edges.
    pub enabled: bool,
    /// How far the pointer must be pushed "into" the edge, in pixels.
    pub pressure_threshold: u32,
    /// How long the pointer must touch the edge, in milliseconds, where
    /// pressure can't be measured (and while dragging something).
    pub fallback_timeout: u32,
    /// The hot part of the edge, centered, in percent of its length.
    pub edge_size: u32,
    pub suppress_activation_when_button_held: bool,
    pub suppress_activation_when_fullscreen: bool,
    /// Only on the primary monitor (Xwayland's primary, set in Displays).
    pub primary_monitor_only: bool,
    pub show_animation: bool,
    pub edge: Edge,
    pub action: Action,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            pressure_threshold: 150,
            fallback_timeout: 250,
            edge_size: 100,
            suppress_activation_when_button_held: true,
            suppress_activation_when_fullscreen: true,
            primary_monitor_only: false,
            show_animation: true,
            edge: Edge::Bottom,
            action: Action::Workspaces,
        }
    }
}

/// What the daemon found, in `~/.local/state/cosmic/…/v1/`, for the app.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, CosmicConfigEntry)]
#[version = 1]
pub struct State {
    /// The compositor can't tell how far the pointer is pushed
    /// (no `zwp_relative_pointer_manager_v1`): the edges use the timeout.
    pub fallback_in_use: bool,
}

pub fn config() -> Option<cosmic_config::Config> {
    cosmic_config::Config::new(APP_ID, Config::VERSION).ok()
}

pub fn state() -> Option<cosmic_config::Config> {
    cosmic_config::Config::new_state(APP_ID, State::VERSION).ok()
}

/// The settings, with the defaults for what is missing or unreadable.
pub fn load(config: &cosmic_config::Config) -> Config {
    Config::get_entry(config).unwrap_or_else(|(_, config)| config)
}

pub fn load_state(state: &cosmic_config::Config) -> State {
    State::get_entry(state).unwrap_or_else(|(_, state)| state)
}

/// The daemon holds a lock on this file while it runs, so that one runs per
/// session and the app can tell whether it does.
pub fn lock_path() -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    dir.join(format!("{APP_ID}-{display}.lock"))
}

/// Whether a daemon runs in this session.
pub fn daemon_running() -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path())
    else {
        return false;
    };
    // Locked by the daemon: it runs. Our lock goes with the file.
    file.try_lock().is_err()
}
