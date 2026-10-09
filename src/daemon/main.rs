// SPDX-License-Identifier: GPL-3.0-only

//! The hot edges: a transparent strip one pixel thick along an edge of each
//! screen, on top of everything (a layer-shell surface). Pushing the pointer
//! into it, as far as the settings ask, opens the workspaces overview (or what
//! the settings say), as Hot Edge does in GNOME Shell.
//!
//! The pointer can't go past the screen's edge, but the compositor still
//! reports how far it was moved (`zwp_relative_pointer_v1`); that is the
//! pressure GNOME's barriers measure. Where it can't be had, the pointer only
//! has to stay on the edge for a while, as in Hot Edge's fallback.

mod pressure;
mod ripple;

use std::{
    fs::File,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use cosmic::cctk::{
    self,
    cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1,
    sctk::{
        self,
        compositor::{CompositorHandler, CompositorState, Region},
        data_device_manager::{
            DataDeviceManagerState, WritePipe,
            data_device::{DataDevice, DataDeviceHandler},
            data_offer::{DataOfferHandler, DragOffer},
            data_source::DataSourceHandler,
        },
        output::{OutputHandler, OutputInfo, OutputState},
        reexports::{
            calloop::{
                self, EventLoop, LoopHandle, RegistrationToken,
                timer::{TimeoutAction, Timer},
            },
            calloop_wayland_source::WaylandSource,
        },
        registry::{ProvidesRegistryState, RegistryState},
        registry_handlers,
        seat::{
            Capability, SeatHandler, SeatState,
            pointer::{PointerEvent, PointerEventKind, PointerHandler},
            relative_pointer::{RelativeMotionEvent, RelativePointerHandler, RelativePointerState},
        },
        shell::{
            WaylandSurface,
            wlr_layer::{
                Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
                LayerSurfaceConfigure,
            },
        },
        shm::{
            Shm, ShmHandler,
            slot::{Buffer, SlotPool},
        },
    },
    toplevel_info::{ToplevelInfoHandler, ToplevelInfoState},
    wayland_client::{
        Connection, QueueHandle,
        globals::registry_queue_init,
        protocol::{
            wl_data_device::WlDataDevice, wl_data_device_manager::DndAction,
            wl_data_source::WlDataSource, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
        },
    },
    wayland_protocols::{
        ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1,
    },
};
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic_ext_hot_edge::config::{self, Config, Edge};

/// After opening something, the edges are made again so that they are above
/// it (newer surfaces go on top), and pushing again closes it.
const RESTACK_DELAY: Duration = Duration::from_millis(500);
/// An edge made again this soon after triggering, under a pointer that is
/// there, is taken as the pointer not having left.
const STILL_THERE: Duration = Duration::from_secs(1);

/// With `HOT_EDGE_DEBUG=1`, what the edges see goes to stderr.
macro_rules! debug {
    ($($arg:tt)*) => {
        if *DEBUG.get_or_init(|| std::env::var_os("HOT_EDGE_DEBUG").is_some()) {
            eprintln!($($arg)*);
        }
    };
}
static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn main() {
    // One per session.
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(config::lock_path())
        .expect("can't open the lock file");
    if lock.try_lock().is_err() {
        eprintln!("{} is already running", config::DAEMON);
        return;
    }

    // Started with the session, maybe before the compositor is ready.
    let mut tries = 0;
    let conn = loop {
        match Connection::connect_to_env() {
            Ok(conn) => break conn,
            Err(err) if tries < 20 => {
                tries += 1;
                debug!("no Wayland connection yet: {err}");
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(err) => {
                eprintln!("{}: not in a Wayland session: {err}", config::DAEMON);
                std::process::exit(1);
            }
        }
    };
    let (globals, event_queue) = registry_queue_init::<Daemon>(&conn).expect("no Wayland registry");
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<Daemon> = EventLoop::try_new().expect("no event loop");
    let handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(handle.clone())
        .expect("can't watch the Wayland connection");

    let compositor = CompositorState::bind(&globals, &qh).expect("no wl_compositor");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("no wlr layer shell");
    let shm = Shm::bind(&globals, &qh).expect("no wl_shm");
    let pool = SlotPool::new(4096, &shm).expect("no shared memory");
    let registry = RegistryState::new(&globals);
    let toplevel_info = ToplevelInfoState::try_new(&registry, &qh);
    let pressure_supported = globals.contents().with_list(|list| {
        list.iter()
            .any(|global| global.interface == "zwp_relative_pointer_manager_v1")
    });

    // The settings, and a wake-up when they change.
    let settings = config::config();
    let (sender, channel) = calloop::channel::sync_channel(4);
    let watcher = settings.as_ref().and_then(|settings| {
        settings
            .watch(move |_, _| {
                let _ = sender.try_send(());
            })
            .ok()
    });
    handle
        .insert_source(channel, |event, _, daemon: &mut Daemon| {
            if let calloop::channel::Event::Msg(()) = event {
                daemon.reload();
            }
        })
        .expect("can't watch the settings");

    // Tell the app which way the edges work.
    if let Some(state) = config::state() {
        let _ = (config::State {
            fallback_in_use: !pressure_supported,
        })
        .write_entry(&state);
    }

    let mut daemon = Daemon {
        config: settings.as_ref().map(config::load).unwrap_or_default(),
        settings,
        registry,
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        relative_pointer: RelativePointerState::bind(&globals, &qh),
        data_devices: DataDeviceManagerState::bind(&globals, &qh).ok(),
        compositor,
        layer_shell,
        shm,
        pool,
        toplevel_info,
        pressure_supported,
        qh: qh.clone(),
        handle,
        pointers: Vec::new(),
        drag_devices: Vec::new(),
        edges: Vec::new(),
        ripples: Vec::new(),
        hover: None,
        pressure: pressure::Pressure::default(),
        latched: false,
        triggered_at: None,
        buttons: 0,
        timer: None,
        restack: None,
        dirty: true,
    };

    let _watcher = watcher;
    loop {
        if let Err(err) = event_loop.dispatch(None, &mut daemon) {
            eprintln!("{}: {err}", config::DAEMON);
            break;
        }
        if daemon.dirty {
            daemon.dirty = false;
            daemon.rebuild();
        }
    }
}

struct HotEdge {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    edge: Edge,
    /// The output's length along the edge, and the strip's, in logical pixels.
    output_length: u32,
    length: u32,
    buffer: Option<Buffer>,
}

/// The pointer (or a drag) on an edge.
struct Hover {
    surface: wl_surface::WlSurface,
    /// Along the edge, from the output's start, in logical pixels.
    along: f64,
    dragging: bool,
}

struct Ripple {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    size: (u32, u32),
    scale: i32,
    center: (f64, f64),
    start: Option<Instant>,
    buffer: Option<Buffer>,
}

struct Daemon {
    settings: Option<cosmic_config::Config>,
    config: Config,
    registry: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    relative_pointer: RelativePointerState,
    data_devices: Option<DataDeviceManagerState>,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    toplevel_info: Option<ToplevelInfoState>,
    pressure_supported: bool,
    qh: QueueHandle<Daemon>,
    handle: LoopHandle<'static, Daemon>,

    pointers: Vec<(wl_seat::WlSeat, wl_pointer::WlPointer, Option<ZwpRelativePointerV1>)>,
    drag_devices: Vec<(wl_seat::WlSeat, DataDevice)>,
    edges: Vec<HotEdge>,
    ripples: Vec<Ripple>,

    hover: Option<Hover>,
    pressure: pressure::Pressure,
    /// Triggered, and the pointer hasn't left the edge since: GNOME's
    /// `_isTriggered`.
    latched: bool,
    triggered_at: Option<Instant>,
    /// Mouse buttons held on an edge.
    buttons: u32,
    /// The timeout of the fallback, and of drags.
    timer: Option<RegistrationToken>,
    restack: Option<RegistrationToken>,
    /// The edges must be made again: outputs or settings changed.
    dirty: bool,
}

impl Daemon {
    fn reload(&mut self) {
        let Some(settings) = &self.settings else {
            return;
        };
        let config = config::load(settings);
        if config != self.config {
            self.config = config;
            self.dirty = true;
        }
    }

    fn output_rect(info: &OutputInfo) -> (i32, i32, i32, i32) {
        let (x, y) = info.logical_position.unwrap_or(info.location);
        let (w, h) = info.logical_size.unwrap_or_else(|| {
            let mode = info.modes.iter().find(|mode| mode.current);
            let (w, h) = mode.map(|mode| mode.dimensions).unwrap_or((0, 0));
            let scale = info.scale_factor.max(1);
            (w / scale, h / scale)
        });
        (x, y, w, h)
    }

    /// Makes the edges again, for the outputs and settings there are now.
    fn rebuild(&mut self) {
        self.edges.clear();
        self.leave();
        if !self.config.enabled {
            return;
        }

        let outputs: Vec<(wl_output::WlOutput, OutputInfo)> = self
            .output_state
            .outputs()
            .filter_map(|output| {
                let info = self.output_state.info(&output)?;
                Some((output, info))
            })
            .collect();
        let rects: Vec<_> = outputs.iter().map(|(_, info)| Self::output_rect(info)).collect();

        // Wayland has no primary output; COSMIC's Displays sets one for X11
        // apps, which cosmic-randr tells.
        let primary = self.config.primary_monitor_only.then(|| {
            let name = primary_output_name();
            outputs
                .iter()
                .position(|(_, info)| name.is_some() && info.name == name)
                .or_else(|| rects.iter().position(|r| (r.0, r.1) == (0, 0)))
                .unwrap_or(0)
        });

        let edge = self.config.edge;
        for (index, ((output, _), &(x, y, w, h))) in outputs.iter().zip(&rects).enumerate() {
            if primary.is_some_and(|primary| primary != index) {
                continue;
            }
            // As Hot Edge: no edge where another monitor lies beyond it, so
            // that the pointer can go across.
            let blocked = rects.iter().enumerate().any(|(other, &(ox, oy, ow, oh))| {
                other != index
                    && match edge {
                        Edge::Bottom => oy >= y + h && ox < x + w && ox + ow > x,
                        Edge::Top => oy + oh <= y && ox < x + w && ox + ow > x,
                        Edge::Left => ox + ow <= x && oy < y + h && oy + oh > y,
                        Edge::Right => ox >= x + w && oy < y + h && oy + oh > y,
                    }
            });
            if blocked || w <= 0 || h <= 0 {
                continue;
            }
            let output_length = match edge {
                Edge::Bottom | Edge::Top => w,
                Edge::Left | Edge::Right => h,
            } as u32;
            let percent = self.config.edge_size.clamp(1, 100);
            let length = (output_length * percent / 100).max(1);
            debug!("edge on output {index} at {x},{y} {w}x{h}: {length} of {output_length}");
            self.edges.push(self.make_edge(output, edge, output_length, length, percent == 100));
        }
    }

    fn make_edge(
        &self,
        output: &wl_output::WlOutput,
        edge: Edge,
        output_length: u32,
        length: u32,
        full: bool,
    ) -> HotEdge {
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("hot-edge"),
            Some(output),
        );
        // Without its ends anchored, a shorter strip sits in the middle.
        let (anchor, ends, size) = match edge {
            Edge::Bottom => (Anchor::BOTTOM, Anchor::LEFT | Anchor::RIGHT, (length, 1)),
            Edge::Top => (Anchor::TOP, Anchor::LEFT | Anchor::RIGHT, (length, 1)),
            Edge::Left => (Anchor::LEFT, Anchor::TOP | Anchor::BOTTOM, (1, length)),
            Edge::Right => (Anchor::RIGHT, Anchor::TOP | Anchor::BOTTOM, (1, length)),
        };
        let size = match (full, edge) {
            (true, Edge::Bottom | Edge::Top) => (0, size.1),
            (true, Edge::Left | Edge::Right) => (size.0, 0),
            _ => size,
        };
        layer.set_anchor(if full { anchor | ends } else { anchor });
        layer.set_size(size.0, size.1);
        // At the very edge, even under a panel that reserves space there.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        HotEdge {
            output: output.clone(),
            layer,
            edge,
            output_length,
            length,
            buffer: None,
        }
    }

    fn edge_of(&self, surface: &wl_surface::WlSurface) -> Option<&HotEdge> {
        self.edges.iter().find(|edge| edge.layer.wl_surface() == surface)
    }

    /// Where along the output a point on an edge is.
    fn along(&self, surface: &wl_surface::WlSurface, (x, y): (f64, f64)) -> f64 {
        self.edge_of(surface).map_or(0.0, |edge| {
            let offset = f64::from(edge.output_length - edge.length) / 2.0;
            offset
                + match edge.edge {
                    Edge::Bottom | Edge::Top => x,
                    Edge::Left | Edge::Right => y,
                }
        })
    }

    fn enter(&mut self, surface: &wl_surface::WlSurface, position: (f64, f64), dragging: bool) {
        if self.edge_of(surface).is_none() {
            return;
        }
        self.cancel_timer();
        self.pressure.reset();
        self.hover = Some(Hover {
            surface: surface.clone(),
            along: self.along(surface, position),
            dragging,
        });
        // An edge made again after triggering, with the pointer still on it.
        if self.latched
            && self
                .triggered_at
                .is_some_and(|at| at.elapsed() < RESTACK_DELAY + STILL_THERE)
        {
            return;
        }
        self.latched = false;
        debug!("enter {position:?}{}", if dragging { " dragging" } else { "" });
        if dragging || !self.pressure_supported {
            let timeout = Duration::from_millis(u64::from(self.config.fallback_timeout));
            let token = self
                .handle
                .insert_source(Timer::from_duration(timeout), |_, _, daemon| {
                    daemon.timer = None;
                    daemon.trigger();
                    TimeoutAction::Drop
                })
                .ok();
            self.timer = token;
        } else if self.config.pressure_threshold == 0 {
            self.trigger();
        }
    }

    fn leave(&mut self) {
        if self.hover.is_some() {
            debug!("leave");
        }
        self.cancel_timer();
        self.pressure.reset();
        self.hover = None;
        self.latched = false;
        self.buttons = 0;
    }

    fn cancel_timer(&mut self) {
        if let Some(token) = self.timer.take() {
            self.handle.remove(token);
        }
    }

    /// The pointer pushed far enough, or stayed long enough.
    fn trigger(&mut self) {
        self.latched = true;
        self.pressure.reset();
        self.cancel_timer();
        let Some(hover) = &self.hover else {
            return;
        };
        let Some(edge) = self.edge_of(&hover.surface) else {
            return;
        };
        if self.config.suppress_activation_when_button_held && self.buttons > 0 {
            debug!("suppressed: a button is held");
            return;
        }
        let output = edge.output.clone();
        let (kind, along) = (edge.edge, hover.along);
        if self.config.suppress_activation_when_fullscreen && self.fullscreen_on(&output) {
            debug!("suppressed: fullscreen");
            return;
        }
        debug!("trigger: {}", self.config.action.command());

        spawn(self.config.action.command());
        self.triggered_at = Some(Instant::now());
        if self.config.show_animation {
            self.ripple(&output, kind, along);
        }

        if let Some(token) = self.restack.take() {
            self.handle.remove(token);
        }
        self.restack = self
            .handle
            .insert_source(Timer::from_duration(RESTACK_DELAY), |_, _, daemon| {
                daemon.restack = None;
                let latched = daemon.latched;
                daemon.rebuild();
                daemon.latched = latched;
                TimeoutAction::Drop
            })
            .ok();
    }

    /// Whether the focused window on this output is fullscreen.
    fn fullscreen_on(&self, output: &wl_output::WlOutput) -> bool {
        use zcosmic_toplevel_handle_v1::State;
        self.toplevel_info.as_ref().is_some_and(|info| {
            info.toplevels().any(|toplevel| {
                toplevel.state.contains(&State::Fullscreen)
                    && toplevel.state.contains(&State::Activated)
                    && toplevel.output.contains(output)
            })
        })
    }

    fn ripple(&mut self, output: &wl_output::WlOutput, edge: Edge, along: f64) {
        let Some(info) = self.output_state.info(output) else {
            return;
        };
        let (_, _, w, h) = Self::output_rect(&info);
        let extent = ripple::EXTENT;
        let span = 2 * extent;
        let output_length = match edge {
            Edge::Bottom | Edge::Top => w,
            Edge::Left | Edge::Right => h,
        };
        // The surface, kept on the screen; the ripple's center in it.
        let start = (along.round() as i32 - extent as i32).clamp(0, (output_length - span as i32).max(0));
        let on = along - f64::from(start);
        let extent_f = f64::from(extent);
        let (anchor, size, margin, center) = match edge {
            Edge::Bottom => (Anchor::BOTTOM | Anchor::LEFT, (span, extent), (0, 0, 0, start), (on, extent_f)),
            Edge::Top => (Anchor::TOP | Anchor::LEFT, (span, extent), (0, 0, 0, start), (on, 0.0)),
            Edge::Left => (Anchor::LEFT | Anchor::TOP, (extent, span), (start, 0, 0, 0), (0.0, on)),
            Edge::Right => (Anchor::RIGHT | Anchor::TOP, (extent, span), (start, 0, 0, 0), (extent_f, on)),
        };

        let surface = self.compositor.create_surface(&self.qh);
        let scale = info.scale_factor.max(1);
        surface.set_buffer_scale(scale);
        // It mustn't catch the pointer.
        if let Ok(region) = Region::new(&self.compositor) {
            surface.set_input_region(Some(region.wl_region()));
        }
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("hot-edge-ripple"),
            Some(output),
        );
        layer.set_anchor(anchor);
        layer.set_size(size.0, size.1);
        layer.set_margin(margin.0, margin.1, margin.2, margin.3);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.ripples.push(Ripple {
            output: output.clone(),
            layer,
            size,
            scale,
            center,
            start: None,
            buffer: None,
        });
    }

    fn draw_ripple(&mut self, index: usize) {
        let ripple = &mut self.ripples[index];
        let start = *ripple.start.get_or_insert_with(Instant::now);
        let t = start.elapsed().as_secs_f64() * 1000.0;
        if t > ripple::DURATION_MS {
            self.ripples.remove(index);
            return;
        }
        let scale = ripple.scale as u32;
        let (width, height) = (ripple.size.0 * scale, ripple.size.1 * scale);
        let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            width as i32 * 4,
            wl_shm::Format::Argb8888,
        ) else {
            self.ripples.remove(index);
            return;
        };
        ripple::draw(canvas, width, height, f64::from(scale), ripple.center, t);
        let surface = ripple.layer.wl_surface();
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.frame(&self.qh, surface.clone());
        if buffer.attach_to(surface).is_ok() {
            ripple.layer.commit();
        }
        ripple.buffer = Some(buffer);
    }

    fn add_seat_devices(&mut self, seat: &wl_seat::WlSeat) {
        if let Some(manager) = &self.data_devices
            && !self.drag_devices.iter().any(|(s, _)| s == seat)
        {
            let device = manager.get_data_device(&self.qh, seat);
            self.drag_devices.push((seat.clone(), device));
        }
    }
}

/// The output COSMIC takes as primary (Displays › "Primary display" for X11
/// apps), by its connector name.
fn primary_output_name() -> Option<String> {
    let out = Command::new("cosmic-randr")
        .args(["list", "--kdl"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let mut current = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("output \"") {
            current = rest.split('"').next().map(str::to_owned);
        } else if line == "xwayland_primary #true" {
            return current;
        }
    }
    None
}

/// Runs a command apart from the daemon, as the keyboard shortcuts do.
fn spawn(command: &str) {
    if command.trim().is_empty() {
        return;
    }
    match Command::new("sh")
        .args(["-c", command])
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(err) => eprintln!("{}: can't run {command}: {err}", config::DAEMON),
    }
}

impl CompositorHandler for Daemon {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}

    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _: u32) {
        if let Some(index) = self.ripples.iter().position(|r| r.layer.wl_surface() == surface) {
            self.draw_ripple(index);
        }
    }

    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}

    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Daemon {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.dirty = true;
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.dirty = true;
    }

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.ripples.retain(|ripple| ripple.output != output);
        self.dirty = true;
    }
}

impl LayerShellHandler for Daemon {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.ripples.retain(|ripple| &ripple.layer != layer);
        if self.edges.iter().any(|edge| &edge.layer == layer) {
            self.dirty = true;
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(index) = self.ripples.iter().position(|r| &r.layer == layer) {
            if self.ripples[index].start.is_none() {
                self.draw_ripple(index);
            }
            return;
        }
        let Some(edge) = self.edges.iter_mut().find(|edge| &edge.layer == layer) else {
            return;
        };
        // Transparent, as wide as it was given.
        let (width, height) = configure.new_size;
        let (width, height) = (width.max(1) as i32, height.max(1) as i32);
        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
        else {
            return;
        };
        canvas.fill(0);
        let surface = edge.layer.wl_surface();
        surface.damage_buffer(0, 0, width, height);
        if buffer.attach_to(surface).is_ok() {
            edge.layer.commit();
        }
        edge.buffer = Some(buffer);
    }
}

impl SeatHandler for Daemon {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.add_seat_devices(&seat);
    }

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability != Capability::Pointer || self.pointers.iter().any(|(s, _, _)| s == &seat) {
            return;
        }
        let Ok(pointer) = self.seat_state.get_pointer(qh, &seat) else {
            return;
        };
        let relative = self.relative_pointer.get_relative_pointer(&pointer, qh).ok();
        self.add_seat_devices(&seat);
        self.pointers.push((seat, pointer, relative));
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability != Capability::Pointer {
            return;
        }
        self.pointers.retain(|(s, pointer, relative)| {
            if s != &seat {
                return true;
            }
            if let Some(relative) = relative {
                relative.destroy();
            }
            pointer.release();
            false
        });
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.drag_devices.retain(|(s, _)| s != &seat);
    }
}

impl PointerHandler for Daemon {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            let on_hover = self.hover.as_ref().is_some_and(|hover| hover.surface == event.surface);
            match event.kind {
                PointerEventKind::Enter { .. } => self.enter(&event.surface, event.position, false),
                // Only the edges there are now: one made again was left
                // when it went away, not by the pointer.
                PointerEventKind::Leave { .. } if on_hover => {
                    // Covered by what was just opened, not left by the
                    // pointer: still triggered until the edge is made again.
                    let latched = self.latched && self.restack.is_some();
                    self.leave();
                    self.latched = latched;
                }
                PointerEventKind::Motion { .. } if on_hover => {
                    let along = self.along(&event.surface, event.position);
                    if let Some(hover) = &mut self.hover {
                        hover.along = along;
                    }
                }
                PointerEventKind::Press { .. } if on_hover => self.buttons += 1,
                PointerEventKind::Release { .. } if on_hover => {
                    self.buttons = self.buttons.saturating_sub(1)
                }
                _ => {}
            }
        }
    }
}

impl RelativePointerHandler for Daemon {
    fn relative_pointer_motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpRelativePointerV1,
        _: &wl_pointer::WlPointer,
        event: RelativeMotionEvent,
    ) {
        let Some(hover) = &self.hover else {
            return;
        };
        if self.latched || hover.dragging {
            return;
        }
        let Some(edge) = self.edge_of(&hover.surface) else {
            return;
        };
        let (dx, dy) = event.delta;
        // Into the edge, and along it.
        let (across, along) = match edge.edge {
            Edge::Bottom => (dy, dx),
            Edge::Top => (-dy, dx),
            Edge::Left => (-dx, dy),
            Edge::Right => (dx, dy),
        };
        let threshold = self.config.pressure_threshold;
        if self.pressure.push(threshold, event.utime / 1000, across, along) {
            self.trigger();
        }
    }
}

/// Dragging something onto an edge: there is no pressure then, only the
/// timeout, and only if the settings let a held button through.
impl DataDeviceHandler for Daemon {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, x: f64, y: f64, surface: &wl_surface::WlSurface) {
        if !self.config.suppress_activation_when_button_held {
            Daemon::enter(self, surface, (x, y), true);
        }
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        if self.hover.as_ref().is_some_and(|hover| hover.dragging) {
            Daemon::leave(self);
        }
    }

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, x: f64, y: f64) {
        if let Some(surface) = self.hover.as_ref().filter(|h| h.dragging).map(|h| h.surface.clone()) {
            let along = self.along(&surface, (x, y));
            if let Some(hover) = &mut self.hover {
                hover.along = along;
            }
        }
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        if self.hover.as_ref().is_some_and(|hover| hover.dragging) {
            Daemon::leave(self);
        }
    }
}

impl DataOfferHandler for Daemon {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}

    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
}

impl DataSourceHandler for Daemon {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: Option<String>) {}

    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: String, _: WritePipe) {}

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl ToplevelInfoHandler for Daemon {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        self.toplevel_info.as_mut().expect("toplevel info bound")
    }

    fn new_toplevel(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ExtForeignToplevelHandleV1) {}

    fn update_toplevel(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ExtForeignToplevelHandleV1) {}

    fn toplevel_closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ExtForeignToplevelHandleV1) {}
}

impl ShmHandler for Daemon {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Daemon {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

sctk::delegate_compositor!(Daemon);
sctk::delegate_output!(Daemon);
sctk::delegate_shm!(Daemon);
sctk::delegate_seat!(Daemon);
sctk::delegate_pointer!(Daemon);
sctk::delegate_relative_pointer!(Daemon);
sctk::delegate_data_device!(Daemon);
sctk::delegate_layer!(Daemon);
sctk::delegate_registry!(Daemon);
cctk::delegate_toplevel_info!(Daemon);
