use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    process::Child,
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result};
use smithay::{
    desktop::{PopupManager, Space, Window},
    input::{Seat, SeatState},
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{generic::Generic, EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction},
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason, GlobalId, ObjectId},
            protocol::{wl_shm, wl_surface::WlSurface},
            Display, DisplayHandle, Resource,
        },
    },
    utils::{Logical, Point, Size, Transform},
    wayland::{
        compositor::{
            with_surface_tree_upward, CompositorClientState, CompositorState,
            SubsurfaceCachedState, TraversalAction, SUBSURFACE_ROLE,
        },
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::{decoration::XdgDecorationState, XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

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
}

pub struct PendingWaitIdle {
    pub client_id: u64,
    pub window_id: Option<u64>,
    pub quiet_ms: u64,
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
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<PcEyeWm>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,
    pub seat: Seat<Self>,

    pub next_window_id: u64,
    pub windows: HashMap<u64, WindowMeta>,
    pub surface_images: HashMap<ObjectId, SurfaceImage>,
    pub subsurface_locations: HashMap<ObjectId, Point<i32, Logical>>,

    pub next_ipc_client_id: u64,
    pub ipc_clients: HashMap<u64, UnixStream>,
    pub ipc_buffers: HashMap<u64, Vec<u8>>,
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
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let popups = PopupManager::default();

        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        seat.add_keyboard(Default::default(), 200, 25)
            .context("failed to initialize seat0 keyboard")?;
        seat.add_pointer();

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
            output_manager_state,
            seat_state,
            data_device_state,
            popups,
            seat,
            next_window_id: 1,
            windows: HashMap::new(),
            surface_images: HashMap::new(),
            subsurface_locations: HashMap::new(),
            next_ipc_client_id: 1,
            ipc_clients: HashMap::new(),
            ipc_buffers: HashMap::new(),
            waiters: Vec::new(),
            children: Vec::new(),
        })
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

    pub fn window_id_for_surface(&self, surface: &WlSurface) -> Option<u64> {
        self.windows.iter().find_map(|(&id, meta)| {
            meta.window
                .toplevel()
                .filter(|t| t.wl_surface() == surface)
                .map(|_| id)
        })
    }

    pub fn composite_window_rgb(&self, id: u64) -> Result<(u32, u32, Vec<u8>), String> {
        let meta = self
            .windows
            .get(&id)
            .ok_or_else(|| format!("unknown window id: {id}"))?;
        let root = meta
            .window
            .toplevel()
            .map(|t| t.wl_surface().clone())
            .ok_or_else(|| "window has no toplevel surface".to_string())?;
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

        with_surface_tree_upward(
            &root,
            Point::<i32, Logical>::from((0, 0)),
            |surf, states, parent_loc| {
                let offset = if surf == &root || states.role != Some(SUBSURFACE_ROLE) {
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
                let offset = if surf == &root || states.role != Some(SUBSURFACE_ROLE) {
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
                    blit_surface(&mut canvas, canvas_w, canvas_h, img, loc.x, loc.y);
                }
            },
            |_, _, _| true,
        );

        Ok((canvas_w, canvas_h, canvas))
    }

    pub fn reap_children(&mut self) {
        self.children
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
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
