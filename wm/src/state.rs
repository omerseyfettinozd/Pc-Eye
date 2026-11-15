use std::{
    borrow::Cow,
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::{fs::MetadataExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result};
use smithay::{
    backend::{
        allocator::gbm::GbmDevice,
        egl::{EGLContext, EGLDevice, EGLDisplay},
        input::KeyState,
        renderer::{
            gles::{GlesRenderbuffer, GlesRenderer},
            ImportDma,
        },
    },
    desktop::{PopupManager, Space, Window},
    input::{
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, MotionEvent,
            PointerTarget, RelativeMotionEvent,
        },
        touch::{
            DownEvent as TouchDownEvent, MotionEvent as TouchMotionEvent, OrientationEvent,
            ShapeEvent, TouchTarget, UpEvent as TouchUpEvent,
        },
        Seat, SeatState,
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{generic::Generic, EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction},
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason, GlobalId, ObjectId},
            protocol::{wl_shm, wl_surface::WlSurface},
            Display, DisplayHandle, Resource,
        },
    },
    utils::{IsAlive, Logical, Point, Serial, Size, Transform},
    wayland::{
        compositor::{
            with_surface_tree_upward, CompositorClientState, CompositorState,
            SubsurfaceCachedState, TraversalAction, SUBSURFACE_ROLE,
        },
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState},
        output::OutputManagerState,
        seat::WaylandFocus,
        selection::data_device::DataDeviceState,
        shell::xdg::{decoration::XdgDecorationState, XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
        xwayland_keyboard_grab::XWaylandKeyboardGrabState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent},
};

#[derive(Debug, Clone, PartialEq)]
pub enum FocusTarget {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl IsAlive for FocusTarget {
    #[inline]
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(s) => s.alive(),
            Self::X11(s) => s.alive(),
        }
    }
}

impl WaylandFocus for FocusTarget {
    #[inline]
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(s) => Some(Cow::Borrowed(s)),
            Self::X11(s) => s.wl_surface().map(Cow::Owned),
        }
    }

    #[inline]
    fn same_client_as(&self, object_id: &ObjectId) -> bool {
        match self {
            Self::Wayland(s) => s.same_client_as(object_id),
            Self::X11(s) => s.same_client_as(object_id),
        }
    }
}

impl KeyboardTarget<PcEyeWm> for FocusTarget {
    fn enter(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, keys: Vec<KeysymHandle<'_>>, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            Self::X11(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::leave(s, seat, data, serial),
            Self::X11(s) => KeyboardTarget::leave(s, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<PcEyeWm>,
        data: &mut PcEyeWm,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            Self::X11(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, modifiers: ModifiersState, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            Self::X11(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
        }
    }
}

impl PointerTarget<PcEyeWm> for FocusTarget {
    fn enter(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &MotionEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::enter(s, seat, data, event),
            Self::X11(s) => PointerTarget::enter(s, seat, data, event),
        }
    }

    fn motion(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &MotionEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::motion(s, seat, data, event),
            Self::X11(s) => PointerTarget::motion(s, seat, data, event),
        }
    }

    fn relative_motion(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &RelativeMotionEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::relative_motion(s, seat, data, event),
            Self::X11(s) => PointerTarget::relative_motion(s, seat, data, event),
        }
    }

    fn button(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &ButtonEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::button(s, seat, data, event),
            Self::X11(s) => PointerTarget::button(s, seat, data, event),
        }
    }

    fn axis(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, frame: AxisFrame) {
        match self {
            Self::Wayland(s) => PointerTarget::axis(s, seat, data, frame),
            Self::X11(s) => PointerTarget::axis(s, seat, data, frame),
        }
    }

    fn frame(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm) {
        match self {
            Self::Wayland(s) => PointerTarget::frame(s, seat, data),
            Self::X11(s) => PointerTarget::frame(s, seat, data),
        }
    }

    fn gesture_swipe_begin(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &GestureSwipeBeginEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::gesture_swipe_begin(s, seat, data, event),
            Self::X11(s) => PointerTarget::gesture_swipe_begin(s, seat, data, event),
        }
    }

    fn gesture_swipe_update(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &GestureSwipeUpdateEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::gesture_swipe_update(s, seat, data, event),
            Self::X11(s) => PointerTarget::gesture_swipe_update(s, seat, data, event),
        }
    }

    fn gesture_swipe_end(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &GestureSwipeEndEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::gesture_swipe_end(s, seat, data, event),
            Self::X11(s) => PointerTarget::gesture_swipe_end(s, seat, data, event),
        }
    }

    fn gesture_pinch_begin(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &GesturePinchBeginEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::gesture_pinch_begin(s, seat, data, event),
            Self::X11(s) => PointerTarget::gesture_pinch_begin(s, seat, data, event),
        }
    }

    fn gesture_pinch_update(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &GesturePinchUpdateEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::gesture_pinch_update(s, seat, data, event),
            Self::X11(s) => PointerTarget::gesture_pinch_update(s, seat, data, event),
        }
    }

    fn gesture_pinch_end(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &GesturePinchEndEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::gesture_pinch_end(s, seat, data, event),
            Self::X11(s) => PointerTarget::gesture_pinch_end(s, seat, data, event),
        }
    }

    fn gesture_hold_begin(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &GestureHoldBeginEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::gesture_hold_begin(s, seat, data, event),
            Self::X11(s) => PointerTarget::gesture_hold_begin(s, seat, data, event),
        }
    }

    fn gesture_hold_end(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &GestureHoldEndEvent) {
        match self {
            Self::Wayland(s) => PointerTarget::gesture_hold_end(s, seat, data, event),
            Self::X11(s) => PointerTarget::gesture_hold_end(s, seat, data, event),
        }
    }

    fn leave(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, serial: Serial, time: u32) {
        match self {
            Self::Wayland(s) => PointerTarget::leave(s, seat, data, serial, time),
            Self::X11(s) => PointerTarget::leave(s, seat, data, serial, time),
        }
    }
}

impl TouchTarget<PcEyeWm> for FocusTarget {
    fn down(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &TouchDownEvent, seq: Serial) {
        match self {
            Self::Wayland(s) => TouchTarget::down(s, seat, data, event, seq),
            Self::X11(s) => TouchTarget::down(s, seat, data, event, seq),
        }
    }

    fn up(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &TouchUpEvent, seq: Serial) {
        match self {
            Self::Wayland(s) => TouchTarget::up(s, seat, data, event, seq),
            Self::X11(s) => TouchTarget::up(s, seat, data, event, seq),
        }
    }

    fn motion(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &TouchMotionEvent, seq: Serial) {
        match self {
            Self::Wayland(s) => TouchTarget::motion(s, seat, data, event, seq),
            Self::X11(s) => TouchTarget::motion(s, seat, data, event, seq),
        }
    }

    fn frame(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, seq: Serial) {
        match self {
            Self::Wayland(s) => TouchTarget::frame(s, seat, data, seq),
            Self::X11(s) => TouchTarget::frame(s, seat, data, seq),
        }
    }

    fn cancel(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, seq: Serial) {
        match self {
            Self::Wayland(s) => TouchTarget::cancel(s, seat, data, seq),
            Self::X11(s) => TouchTarget::cancel(s, seat, data, seq),
        }
    }

    fn shape(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &ShapeEvent, seq: Serial) {
        match self {
            Self::Wayland(s) => TouchTarget::shape(s, seat, data, event, seq),
            Self::X11(s) => TouchTarget::shape(s, seat, data, event, seq),
        }
    }

    fn orientation(&self, seat: &Seat<PcEyeWm>, data: &mut PcEyeWm, event: &OrientationEvent, seq: Serial) {
        match self {
            Self::Wayland(s) => TouchTarget::orientation(s, seat, data, event, seq),
            Self::X11(s) => TouchTarget::orientation(s, seat, data, event, seq),
        }
    }
}

pub struct CalloopData {
    pub state: PcEyeWm,
    pub display_handle: DisplayHandle,
}

#[derive(Debug, Clone)]
pub struct SurfaceImage {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    pub alpha: Option<Vec<u8>>,
}

pub struct WindowMeta {
    pub id: u64,
    pub window: Window,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub commits: u64,
    pub last_commit: Instant,
    pub announced: bool,
}

pub struct PendingWaitIdle {
    pub client_id: u64,
    pub window_id: Option<u64>,
    pub quiet_ms: u64,
    pub grace_ms: u64,
    pub started: Instant,
    pub deadline: Instant,
}

pub struct PcEyeWm {
    pub start_time: Instant,
    pub socket_name: String,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, CalloopData>,
    pub loop_signal: LoopSignal,

    pub space: Space<Window>,
    pub output: Output,
    pub output_size: Size<i32, Logical>,
    pub _output_global: GlobalId,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,
    pub renderer: Option<GlesRenderer>,
    pub dmabuf_renderbuffer: Option<((i32, i32), GlesRenderbuffer)>,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<PcEyeWm>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,
    pub seat: Seat<Self>,
    pub xwayland_shell_state: XWaylandShellState,
    pub xwm: Option<X11Wm>,
    pub x_display: Option<u32>,

    pub cursor_pos: Point<f64, Logical>,
    pub focus_history: Vec<u64>,

    pub next_window_id: u64,
    pub windows: HashMap<u64, WindowMeta>,
    pub surface_images: HashMap<ObjectId, SurfaceImage>,
    pub subsurface_locations: HashMap<ObjectId, Point<i32, Logical>>,

    pub next_ipc_client_id: u64,
    pub ipc_clients: HashMap<u64, UnixStream>,
    pub ipc_buffers: HashMap<u64, Vec<u8>>,
    pub event_subscribers: Vec<UnixStream>,
    pub waiters: Vec<PendingWaitIdle>,
    pub children: Vec<Child>,
}

impl PcEyeWm {
    pub fn new(
        event_loop: &mut EventLoop<'static, CalloopData>,
        display: Display<Self>,
        socket_name: &str,
        output_dims: (i32, i32),
    ) -> Result<Self> {
        let start_time = Instant::now();
        let dh = display.handle();
        let loop_handle = event_loop.handle();
        let loop_signal = event_loop.get_signal();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(
            &dh,
            vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
        );
        let mut dmabuf_state = DmabufState::new();
        let (renderer, dmabuf_global) = match try_init_dmabuf_renderer() {
            Some((renderer, file_dev_id)) => {
                let global = create_dmabuf_global(&mut dmabuf_state, &dh, &renderer, file_dev_id);
                (Some(renderer), Some(global))
            }
            None => (None, None),
        };
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let popups = PopupManager::default();
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        XWaylandKeyboardGrabState::new::<Self>(&dh);

        let cursor_pos = Point::from((output_dims.0 as f64 / 2.0, output_dims.1 as f64 / 2.0));
        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        seat.add_keyboard(Default::default(), 200, 25)
            .context("failed to initialize seat0 keyboard")?;
        let pointer = seat.add_pointer();
        pointer.set_location(cursor_pos);

        let mut space = Space::default();
        let output_size: Size<i32, Logical> = output_dims.into();
        let mode = OutputMode {
            size: output_dims.into(),
            refresh: 60_000,
        };
        let output = Output::new(
            "headless-1".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "pc-eye".into(),
                model: "headless".into(),
            },
        );
        let output_global = output.create_global::<Self>(&dh);
        output.set_preferred(mode);
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        space.map_output(&output, (0, 0));

        Self::init_wayland_listener(display, &loop_handle, socket_name)?;
        Self::start_xwayland(&dh, &loop_handle);

        Ok(Self {
            start_time,
            socket_name: socket_name.to_string(),
            display_handle: dh,
            loop_handle,
            loop_signal,
            space,
            output,
            output_size,
            _output_global: output_global,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            dmabuf_state,
            dmabuf_global,
            renderer,
            dmabuf_renderbuffer: None,
            output_manager_state,
            seat_state,
            data_device_state,
            popups,
            seat,
            xwayland_shell_state,
            xwm: None,
            x_display: None,
            cursor_pos,
            focus_history: Vec::new(),
            next_window_id: 1,
            windows: HashMap::new(),
            surface_images: HashMap::new(),
            subsurface_locations: HashMap::new(),
            next_ipc_client_id: 1,
            ipc_clients: HashMap::new(),
            ipc_buffers: HashMap::new(),
            event_subscribers: Vec::new(),
            waiters: Vec::new(),
            children: Vec::new(),
        })
    }

    pub fn has_dmabuf(&self) -> bool {
        self.renderer.is_some() && self.dmabuf_global.is_some()
    }

    fn start_xwayland(dh: &DisplayHandle, loop_handle: &LoopHandle<'static, CalloopData>) {
        match XWayland::spawn(
            dh,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            Stdio::null(),
            Stdio::null(),
            |_| (),
        ) {
            Ok((xwayland, client)) => {
                let wm_loop_handle = loop_handle.clone();
                if let Err(err) = loop_handle.insert_source(xwayland, move |event, _, data| {
                    match event {
                        XWaylandEvent::Ready {
                            x11_socket,
                            display_number,
                        } => {
                            match X11Wm::start_wm(wm_loop_handle.clone(), x11_socket, client.clone()) {
                                Ok(wm) => {
                                    data.state.xwm = Some(wm);
                                    data.state.x_display = Some(display_number);
                                    tracing::info!(display = display_number, "Xwayland ready");
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        ?err,
                                        "failed to start X11 window manager; continuing as Wayland-only"
                                    );
                                }
                            }
                        }
                        XWaylandEvent::Error => {
                            tracing::warn!("Xwayland failed to start; continuing as Wayland-only");
                        }
                    }
                }) {
                    tracing::warn!(
                        ?err,
                        "failed to insert Xwayland event source; continuing as Wayland-only"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(?err, "failed to spawn Xwayland; continuing as Wayland-only");
            }
        }
    }

    fn init_wayland_listener(
        display: Display<PcEyeWm>,
        loop_handle: &LoopHandle<'static, CalloopData>,
        socket_name: &str,
    ) -> Result<()> {
        let listening_socket = ListeningSocketSource::with_name(socket_name)
            .with_context(|| format!("failed to bind Wayland socket '{socket_name}'"))?;

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, data| {
                if let Err(err) = data
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                {
                    tracing::warn!(?err, "failed to insert Wayland client");
                }
            })
            .map_err(|e| anyhow::anyhow!("failed to insert Wayland listening socket: {e:?}"))?;

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, data| {
                    // Safety: we do not drop the display inside dispatch_clients.
                    unsafe {
                        if let Err(err) = display.get_mut().dispatch_clients(&mut data.state) {
                            tracing::error!(?err, "error dispatching Wayland clients");
                        }
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| anyhow::anyhow!("failed to insert Wayland display source: {e:?}"))?;

        Ok(())
    }

    pub fn x_display_str(&self) -> Option<String> {
        self.x_display.map(|n| format!(":{n}"))
    }

    pub fn window_id_for_surface(&self, surface: &WlSurface) -> Option<u64> {
        self.windows.iter().find_map(|(&id, meta)| {
            meta.window
                .wl_surface()
                .filter(|s| s.as_ref() == surface)
                .map(|_| id)
        })
    }

    pub fn window_id_for_x11(&self, x11: &X11Surface) -> Option<u64> {
        let target_id = x11.window_id();
        self.windows.iter().find_map(|(&id, meta)| {
            meta.window
                .x11_surface()
                .filter(|s| s.window_id() == target_id)
                .map(|_| id)
        })
    }

    pub fn window_id_for_focus(&self, focus: &FocusTarget) -> Option<u64> {
        match focus {
            FocusTarget::Wayland(surface) => self.window_id_for_surface(surface),
            FocusTarget::X11(x11) => self.window_id_for_x11(x11),
        }
    }

    pub fn window_id_for_window(&self, window: &Window) -> Option<u64> {
        self.windows
            .iter()
            .find_map(|(&id, meta)| (&meta.window == window).then_some(id))
    }

    pub fn record_focus_history(&mut self, id: u64) {
        self.focus_history.retain(|&fid| fid != id);
        self.focus_history.insert(0, id);
    }

    pub fn drain_event_subscribers(&mut self) {
        if self.event_subscribers.is_empty() {
            return;
        }
        self.event_subscribers.retain_mut(drain_stream);
    }

    fn emit_event_payload(&mut self, payload: &str) {
        if self.event_subscribers.is_empty() {
            return;
        }
        let bytes = payload.as_bytes();
        self.event_subscribers
            .retain_mut(|stream| drain_stream(stream) && stream.write_all(bytes).is_ok());
    }

    pub fn emit_openwindow(&mut self, id: u64, app_id: &str, title: &str) {
        let app_id = sanitize_event_field(app_id);
        let title = sanitize_event_field(title);
        let payload = format!("openwindow>>{id:x},1,{app_id},{title}\n");
        self.emit_event_payload(&payload);
    }

    pub fn emit_closewindow(&mut self, id: u64) {
        let payload = format!("closewindow>>{id:x}\n");
        self.emit_event_payload(&payload);
    }

    pub fn emit_activewindow(&mut self, info: Option<(u64, &str, &str)>) {
        match info {
            Some((id, app_id, title)) => {
                let app_id = sanitize_event_field(app_id);
                let title = sanitize_event_field(title);
                let payload = format!("activewindow>>{app_id},{title}\nactivewindowv2>>{id:x}\n");
                self.emit_event_payload(&payload);
            }
            None => {
                self.emit_event_payload("activewindow>>,\nactivewindowv2>>\n");
            }
        }
    }

    pub fn emit_windowtitle(&mut self, id: u64, title: &str) {
        let title = sanitize_event_field(title);
        let payload = format!("windowtitle>>{id:x}\nwindowtitlev2>>{id:x},{title}\n");
        self.emit_event_payload(&payload);
    }

    fn draw_window_into(
        &self,
        canvas: &mut [u8],
        canvas_w: u32,
        canvas_h: u32,
        root: &WlSurface,
        offset_x: i32,
        offset_y: i32,
    ) {
        with_surface_tree_upward(
            root,
            Point::<i32, Logical>::from((offset_x, offset_y)),
            |surf, states, parent_loc| {
                let offset = if surf == root || states.role != Some(SUBSURFACE_ROLE) {
                    Point::from((0, 0))
                } else {
                    self.subsurface_locations
                        .get(&surf.id())
                        .copied()
                        .unwrap_or_else(|| {
                            states
                                .cached_state
                                .get::<SubsurfaceCachedState>()
                                .current()
                                .location
                        })
                };
                TraversalAction::DoChildren(*parent_loc + offset)
            },
            |surf, states, parent_loc| {
                let offset = if surf == root || states.role != Some(SUBSURFACE_ROLE) {
                    Point::from((0, 0))
                } else {
                    self.subsurface_locations
                        .get(&surf.id())
                        .copied()
                        .unwrap_or_else(|| {
                            states
                                .cached_state
                                .get::<SubsurfaceCachedState>()
                                .current()
                                .location
                        })
                };
                let loc = *parent_loc + offset;
                if let Some(img) = self.surface_images.get(&surf.id()) {
                    blit_surface(canvas, canvas_w, canvas_h, img, loc.x, loc.y);
                }
            },
            |_, _, _| true,
        );
    }

    pub fn composite_window_rgb(&self, id: u64) -> Result<(u32, u32, Vec<u8>), String> {
        let meta = self
            .windows
            .get(&id)
            .ok_or_else(|| format!("unknown window id: {id}"))?;
        let root = meta
            .window
            .wl_surface()
            .map(|s| s.into_owned())
            .ok_or_else(|| format!("window {id} has no surface yet"))?;
        let root_img = self
            .surface_images
            .get(&root.id())
            .ok_or_else(|| format!("window {id} has no committed pixels yet"))?;

        let canvas_w = root_img.width;
        let canvas_h = root_img.height;
        if canvas_w == 0 || canvas_h == 0 {
            return Err(format!("window {id} has zero-sized surface"));
        }

        let mut canvas = vec![0u8; (canvas_w as usize) * (canvas_h as usize) * 3];
        self.draw_window_into(&mut canvas, canvas_w, canvas_h, &root, 0, 0);
        Ok((canvas_w, canvas_h, canvas))
    }

    pub fn composite_region_rgb(
        &self,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<i32>,
        h: Option<i32>,
    ) -> Result<(u32, u32, Vec<u8>), String> {
        let out_w = self.output_size.w;
        let out_h = self.output_size.h;
        if out_w <= 0 || out_h <= 0 {
            return Err("invalid output size".to_string());
        }

        let rx = x.unwrap_or(0);
        let ry = y.unwrap_or(0);
        let rw = w.unwrap_or(out_w);
        let rh = h.unwrap_or(out_h);

        let x0 = rx.clamp(0, out_w);
        let y0 = ry.clamp(0, out_h);
        let x1 = (rx as i64).saturating_add(rw as i64).clamp(0, out_w as i64) as i32;
        let y1 = (ry as i64).saturating_add(rh as i64).clamp(0, out_h as i64) as i32;

        let crop_w = (x1 - x0).max(0) as u32;
        let crop_h = (y1 - y0).max(0) as u32;
        if crop_w == 0 || crop_h == 0 {
            return Err("capture region is empty after clamping".to_string());
        }

        let buf_len = (crop_w as usize)
            .checked_mul(crop_h as usize)
            .and_then(|n| n.checked_mul(3))
            .ok_or_else(|| "capture region buffer size overflow".to_string())?;
        let mut canvas = vec![0u8; buf_len];

        for window in self.space.elements() {
            let Some(root) = window.wl_surface().map(|s| s.into_owned()) else {
                continue;
            };
            let win_loc = self
                .space
                .element_location(window)
                .unwrap_or_else(|| (0, 0).into());
            self.draw_window_into(
                &mut canvas,
                crop_w,
                crop_h,
                &root,
                win_loc.x - x0,
                win_loc.y - y0,
            );
        }

        Ok((crop_w, crop_h, canvas))
    }

    pub fn reap_children(&mut self) {
        self.children
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }
}

fn open_render_node() -> Result<File> {
    let preferred = Path::new("/dev/dri/renderD128");
    if preferred.exists() {
        match OpenOptions::new().read(true).write(true).open(preferred) {
            Ok(file) => return Ok(file),
            Err(err) => {
                tracing::debug!(path = %preferred.display(), ?err, "failed to open /dev/dri/renderD128");
            }
        }
    }

    let mut candidates: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .context("failed to read /dev/dri")?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path != preferred
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("renderD"))
        })
        .collect();
    candidates.sort();

    for path in candidates {
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => return Ok(file),
            Err(err) => {
                tracing::debug!(path = %path.display(), ?err, "failed to open render node");
            }
        }
    }

    anyhow::bail!("no usable DRM render node found under /dev/dri")
}

fn try_init_dmabuf_renderer() -> Option<(GlesRenderer, Option<u64>)> {
    if std::env::var_os("PCEYE_WM_DMABUF").is_some_and(|v| v == "0") {
        tracing::info!("dmabuf disabled via PCEYE_WM_DMABUF=0");
        return None;
    }

    let init = || -> Result<(GlesRenderer, Option<u64>)> {
        let file = open_render_node()?;
        let file_dev_id = file.metadata().ok().map(|m| m.rdev());
        let gbm = GbmDevice::new(file).context("failed to create GbmDevice")?;
        // Safety: `gbm` is a valid initialized `GbmDevice` wrapping an open DRM render node.
        let display = unsafe { EGLDisplay::new(gbm) }.context("failed to create EGLDisplay from GBM")?;
        let context = EGLContext::new(&display).context("failed to create EGLContext")?;
        // Safety: `context` is a valid `EGLContext` created from the live `EGLDisplay`.
        let renderer = unsafe { GlesRenderer::new(context) }.context("failed to create GlesRenderer")?;
        Ok((renderer, file_dev_id))
    };

    match init() {
        Ok(res) => {
            tracing::info!("initialized EGL/GLES renderer for dmabuf support");
            Some(res)
        }
        Err(err) => {
            tracing::warn!(?err, "GPU dmabuf initialization failed; falling back to wl_shm only");
            None
        }
    }
}

fn create_dmabuf_global(
    dmabuf_state: &mut DmabufState,
    dh: &DisplayHandle,
    renderer: &GlesRenderer,
    file_dev_id: Option<u64>,
) -> DmabufGlobal {
    let formats = renderer.dmabuf_formats();
    let egl_dev_id = EGLDevice::device_for_display(renderer.egl_context().display())
        .ok()
        .and_then(|dev| dev.try_get_render_node().ok().flatten())
        .map(|node| node.dev_id());

    if let Some(dev_id) = egl_dev_id.or_else(|| file_dev_id.and_then(|r| r.try_into().ok())) {
        match DmabufFeedbackBuilder::new(dev_id, formats.clone()).build() {
            Ok(feedback) => {
                return dmabuf_state.create_global_with_default_feedback::<PcEyeWm>(dh, &feedback);
            }
            Err(err) => {
                tracing::warn!(?err, "failed to build dmabuf default feedback; using v3 global");
            }
        }
    }

    dmabuf_state.create_global::<PcEyeWm>(dh, formats)
}

fn drain_stream(stream: &mut UnixStream) -> bool {
    let mut tmp = [0u8; 1024];
    for _ in 0..64 {
        match stream.read(&mut tmp) {
            Ok(0) => return true,
            Ok(_) => continue,
            Err(err) if err.kind() == ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
    }
    true
}

fn sanitize_event_field(s: &str) -> String {
    if s.contains(['\n', '\r']) {
        s.replace(['\r', '\n'], " ")
    } else {
        s.to_string()
    }
}

fn blit_surface(
    canvas: &mut [u8],
    canvas_w: u32,
    canvas_h: u32,
    img: &SurfaceImage,
    ox: i32,
    oy: i32,
) {
    let cw = canvas_w as i32;
    let ch = canvas_h as i32;
    let sw = img.width as i32;
    let sh = img.height as i32;

    let start_x = (-ox).max(0);
    let end_x = (cw - ox).min(sw);
    let start_y = (-oy).max(0);
    let end_y = (ch - oy).min(sh);

    if start_x >= end_x || start_y >= end_y {
        return;
    }

    match &img.alpha {
        None => {
            for sy in start_y..end_y {
                let dy = (oy + sy) as usize;
                let src_row_start = ((sy as usize) * (img.width as usize) + (start_x as usize)) * 3;
                let dst_row_start = (dy * (canvas_w as usize) + ((ox + start_x) as usize)) * 3;
                let copy_len = ((end_x - start_x) as usize) * 3;
                canvas[dst_row_start..dst_row_start + copy_len]
                    .copy_from_slice(&img.rgb[src_row_start..src_row_start + copy_len]);
            }
        }
        Some(alpha) => {
            for sy in start_y..end_y {
                let dy = (oy + sy) as usize;
                for sx in start_x..end_x {
                    let dx = (ox + sx) as usize;
                    let sidx = (sy as usize) * (img.width as usize) + (sx as usize);
                    let a = alpha[sidx] as u16;
                    let didx3 = (dy * (canvas_w as usize) + dx) * 3;
                    let sidx3 = sidx * 3;
                    if a == 255 {
                        canvas[didx3] = img.rgb[sidx3];
                        canvas[didx3 + 1] = img.rgb[sidx3 + 1];
                        canvas[didx3 + 2] = img.rgb[sidx3 + 2];
                    } else if a > 0 {
                        // Wayland ARGB8888 uses premultiplied alpha.
                        let inv_a = 255 - a;
                        for c in 0..3 {
                            let src = img.rgb[sidx3 + c] as u16;
                            let dst = canvas[didx3 + c] as u16;
                            canvas[didx3 + c] = (src + (dst * inv_a + 127) / 255).min(255) as u8;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
