// SPDX-License-Identifier: GPL-3.0-only

//! Hot Edge's settings, as the extension's preferences window has them, plus
//! which edge and what it opens. The daemon picks them up as they change.

use std::{os::unix::process::CommandExt, process::Command, time::Duration};

use cosmic::{
    ApplicationExt, Element, Task, app,
    cosmic_config::{self, CosmicConfigEntry},
    iced::{Alignment, Length, Size, Subscription},
    theme,
    widget::{self, settings},
};

use cosmic_ext_hot_edge::{
    config::{self, Action, Config, Edge, State},
    i18n::strings,
};

fn main() -> cosmic::iced::Result {
    let settings = app::Settings::default().size(Size::new(640.0, 860.0));
    app::run::<App>(settings, ())
}

const EDGES: [Edge; 4] = [Edge::Bottom, Edge::Top, Edge::Left, Edge::Right];

#[derive(Clone, Debug)]
pub enum Message {
    Enabled(bool),
    Pressure(u32),
    Timeout(u32),
    EdgeSize(u32),
    SuppressButton(bool),
    SuppressFullscreen(bool),
    PrimaryOnly(bool),
    Animation(bool),
    Edge(usize),
    Action(usize),
    /// A command being typed, saved as it is typed.
    Command(String),
    Reset,
    StartDaemon,
    /// Whether the daemon runs, checked now and then.
    Daemon(bool),
    /// The settings or the daemon's state changed on disk.
    Config(Config),
    State(State),
}

pub struct App {
    core: app::Core,
    settings: Option<cosmic_config::Config>,
    config: Config,
    state: State,
    daemon: Option<bool>,
    edges: Vec<&'static str>,
    actions: Vec<&'static str>,
}

impl App {
    fn change(&mut self, change: impl FnOnce(&mut Config)) {
        change(&mut self.config);
        if let Some(settings) = &self.settings
            && let Err(err) = self.config.write_entry(settings)
        {
            eprintln!("can't save the settings: {err}");
        }
    }

    fn status(&self) -> Option<Element<'_, Message>> {
        let t = strings();
        if !self.config.enabled || self.daemon != Some(false) {
            return None;
        }
        Some(
            widget::container(
                widget::row::with_children(vec![
                    widget::icon::from_name("dialog-warning-symbolic").size(24).into(),
                    widget::column::with_children(vec![
                        widget::text::body(t.stopped).into(),
                        widget::text::caption(t.stopped_about).into(),
                    ])
                    .width(Length::Fill)
                    .into(),
                    widget::button::suggested(t.start)
                        .on_press(Message::StartDaemon)
                        .into(),
                ])
                .spacing(theme::spacing().space_s)
                .align_y(Alignment::Center),
            )
            .padding(theme::spacing().space_s)
            .class(theme::Container::Card)
            .width(Length::Fill)
            .into(),
        )
    }

    fn spin<'a>(
        title: &'a str,
        about: &'a str,
        unit: &str,
        value: u32,
        (step, min, max): (u32, u32, u32),
        message: fn(u32) -> Message,
    ) -> Element<'a, Message> {
        settings::item::builder(title)
            .description(about)
            .control(widget::spin_button(
                format!("{value} {unit}"),
                value,
                step,
                min,
                max,
                message,
            ))
            .into()
    }

    fn behavior(&self) -> Element<'_, Message> {
        let t = strings();
        let c = &self.config;
        let mut section = settings::section().title(t.behavior);
        // As in Hot Edge, the one in use; the timeout also serves drags.
        if !self.state.fallback_in_use {
            section = section.add(Self::spin(
                t.pressure,
                t.pressure_about,
                t.pixels,
                c.pressure_threshold,
                (25, 0, 500),
                Message::Pressure,
            ));
        }
        if self.state.fallback_in_use || !c.suppress_activation_when_button_held {
            let about = if self.state.fallback_in_use {
                t.timeout_about
            } else {
                t.timeout_drag_about
            };
            section = section.add(Self::spin(
                t.timeout,
                about,
                t.milliseconds,
                c.fallback_timeout,
                (50, 0, 1000),
                Message::Timeout,
            ));
        }
        section
            .add(Self::spin(
                t.edge_size,
                t.edge_size_about,
                "%",
                c.edge_size,
                (10, 1, 100),
                Message::EdgeSize,
            ))
            .add(
                settings::item::builder(t.suppress_button)
                    .description(t.suppress_button_about)
                    .toggler(c.suppress_activation_when_button_held, Message::SuppressButton),
            )
            .add(
                settings::item::builder(t.suppress_fullscreen)
                    .description(t.suppress_fullscreen_about)
                    .toggler(c.suppress_activation_when_fullscreen, Message::SuppressFullscreen),
            )
            .add(
                settings::item::builder(t.primary_only)
                    .description(t.primary_only_about)
                    .toggler(c.primary_monitor_only, Message::PrimaryOnly),
            )
            .add(settings::item::builder(t.animation).toggler(c.show_animation, Message::Animation))
            .into()
    }

    fn edge_and_action(&self) -> Element<'_, Message> {
        let t = strings();
        let edge = EDGES.iter().position(|e| *e == self.config.edge);
        let action = match self.config.action {
            Action::Workspaces => 0,
            Action::AppLibrary => 1,
            Action::Launcher => 2,
            Action::Command(_) => 3,
        };
        let mut section = settings::section()
            .title(t.edge_and_action)
            .add(settings::item(
                t.edge,
                widget::dropdown(self.edges.as_slice(), edge, Message::Edge),
            ))
            .add(settings::item(
                t.action,
                widget::dropdown(self.actions.as_slice(), Some(action), Message::Action),
            ));
        if let Action::Command(command) = &self.config.action {
            section = section.add(settings::item_row(vec![
                widget::text_input(t.command_placeholder, command.as_str())
                    .on_input(Message::Command)
                    .width(Length::Fill)
                    .into(),
            ]));
        }
        section.into()
    }

    fn defaults(&self) -> Element<'_, Message> {
        let t = strings();
        settings::section()
            .add(
                settings::item::builder(t.defaults)
                    .description(t.defaults_about)
                    .control(widget::button::standard(t.reset).on_press(Message::Reset)),
            )
            .into()
    }
}

/// Starts the daemon, apart from the app: it is the one beside the app, or
/// on the PATH.
fn start_daemon() {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(config::DAEMON)))
        .filter(|path| path.exists());
    let mut command = match beside {
        Some(path) => Command::new(path),
        None => Command::new(config::DAEMON),
    };
    // Its own process group, so it outlives the app.
    match command.process_group(0).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(err) => eprintln!("can't start {}: {err}", config::DAEMON),
    }
}

fn check_daemon() -> Task<cosmic::Action<Message>> {
    Task::future(async {
        let running = tokio::task::spawn_blocking(config::daemon_running)
            .await
            .unwrap_or(false);
        cosmic::Action::App(Message::Daemon(running))
    })
}

impl cosmic::Application for App {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = config::APP_ID;

    fn core(&self) -> &app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut app::Core {
        &mut self.core
    }

    fn init(core: app::Core, _flags: ()) -> (Self, Task<cosmic::Action<Message>>) {
        let t = strings();
        let settings = config::config();
        let config = settings.as_ref().map(config::load).unwrap_or_default();
        let state = config::state().as_ref().map(config::load_state).unwrap_or_default();
        let mut app = App {
            core,
            settings,
            config,
            state,
            daemon: None,
            edges: vec![t.bottom, t.top, t.left, t.right],
            actions: vec![t.workspaces, t.app_library, t.launcher, t.command],
        };
        app.set_header_title(t.title.to_owned());
        let title = match app.core.main_window_id() {
            Some(id) => app.set_window_title(t.title.to_owned(), id),
            None => Task::none(),
        };
        // Just installed: no need to log in again for the edge.
        if app.config.enabled && !config::daemon_running() {
            start_daemon();
        }
        (app, Task::batch([title, check_daemon()]))
    }

    fn update(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::Enabled(on) => {
                self.change(|c| c.enabled = on);
                if on && !config::daemon_running() {
                    start_daemon();
                    return check_daemon();
                }
            }
            Message::Pressure(value) => self.change(|c| c.pressure_threshold = value),
            Message::Timeout(value) => self.change(|c| c.fallback_timeout = value),
            Message::EdgeSize(value) => self.change(|c| c.edge_size = value),
            Message::SuppressButton(on) => {
                self.change(|c| c.suppress_activation_when_button_held = on)
            }
            Message::SuppressFullscreen(on) => {
                self.change(|c| c.suppress_activation_when_fullscreen = on)
            }
            Message::PrimaryOnly(on) => self.change(|c| c.primary_monitor_only = on),
            Message::Animation(on) => self.change(|c| c.show_animation = on),
            Message::Edge(index) => {
                if let Some(edge) = EDGES.get(index).copied() {
                    self.change(|c| c.edge = edge);
                }
            }
            Message::Action(index) => {
                // A command is kept when picking it again.
                let command = match &self.config.action {
                    Action::Command(command) => command.clone(),
                    _ => String::new(),
                };
                let action = match index {
                    1 => Action::AppLibrary,
                    2 => Action::Launcher,
                    3 => Action::Command(command),
                    _ => Action::Workspaces,
                };
                self.change(|c| c.action = action);
            }
            Message::Command(command) => self.change(|c| c.action = Action::Command(command)),
            Message::Reset => {
                let enabled = self.config.enabled;
                self.change(|c| {
                    *c = Config {
                        enabled,
                        ..Config::default()
                    }
                });
            }
            Message::StartDaemon => {
                start_daemon();
                return check_daemon();
            }
            Message::Daemon(running) => self.daemon = Some(running),
            Message::Config(config) => self.config = config,
            Message::State(state) => self.state = state,
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            cosmic_config::config_subscription::<_, Config>(
                "config",
                config::APP_ID.into(),
                Config::VERSION,
            )
            .map(|update| Message::Config(update.config)),
            cosmic_config::config_state_subscription::<_, State>(
                "state",
                config::APP_ID.into(),
                State::VERSION,
            )
            .map(|update| Message::State(update.config)),
            cosmic::iced::time::every(Duration::from_secs(2))
                .map(|_| Message::Daemon(config::daemon_running())),
        ])
    }

    fn view(&self) -> Element<'_, Message> {
        let t = strings();
        let spacing = theme::spacing();
        let enabled = settings::section().add(
            settings::item::builder(t.enabled)
                .description(t.enabled_about)
                .toggler(self.config.enabled, Message::Enabled),
        );
        let mut column = widget::column::with_capacity(5);
        if let Some(status) = self.status() {
            column = column.push(status);
        }
        column = column.push(enabled);
        if self.config.enabled {
            column = column
                .push(self.behavior())
                .push(self.edge_and_action())
                .push(self.defaults());
        }
        let column = column
            .spacing(spacing.space_m)
            .padding([spacing.space_s, spacing.space_l]);
        widget::scrollable(widget::container(column).max_width(760).center_x(Length::Fill))
            .into()
    }
}
