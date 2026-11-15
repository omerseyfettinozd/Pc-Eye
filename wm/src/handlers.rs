use std::time::Instant;

use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, PopupKind, PopupManager, Space, Window,
    },
    input::{Seat, SeatHandler, SeatState},
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
            shell::server::xdg_toplevel,
        },
        wayland_server::{
            protocol::{wl_buffer, wl_seat, wl_shm, wl_surface::WlSurface},
            Client, Resource,
        },
    },
    utils::{Logical, Point, Serial, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, get_role, is_sync_subsurface, with_states, BufferAssignment,
            CompositorClientState, CompositorHandler, CompositorState, SubsurfaceCachedState,
            SurfaceAttributes, SUBSURFACE_ROLE,
        },
        output::{OutputHandler, OutputManagerState},
        selection::{
            data_device::{
                set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
                ServerDndGrabHandler,
            },
            SelectionHandler,
        },
        shell::xdg::{
            decoration::{XdgDecorationHandler, XdgDecorationState},
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{with_buffer_contents, ShmHandler, ShmState},
    },
};

use crate::state::{ClientState, PcEyeWm, SurfaceImage, WindowMeta};

impl CompositorHandler for PcEyeWm {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        let mut extracted_image: Option<Option<SurfaceImage>> = None;
        let mut subsurface_loc: Option<Point<i32, Logical>> = None;
        let mut buffer_to_release: Option<wl_buffer::WlBuffer> = None;

        let is_subsurface = get_role(surface) == Some(SUBSURFACE_ROLE);

        with_states(surface, |states| {
            if is_subsurface {
                let loc = states
                    .cached_state
                    .get::<SubsurfaceCachedState>()
                    .current()
                    .location;
                subsurface_loc = Some(loc);
            }

            let mut attrs = states.cached_state.get::<SurfaceAttributes>();
            let current = attrs.current();
            match &current.buffer {
                Some(BufferAssignment::NewBuffer(buffer)) => {
                    match extract_shm_image(buffer) {
                        Ok(Some(img)) => {
                            extracted_image = Some(Some(img));
                        }
                        Ok(None) => {}
                        Err(err) => {
                            tracing::debug!(?err, "skipping non-shm or invalid buffer");
                        }
                    }
                    buffer_to_release = Some(buffer.clone());
                }
                Some(BufferAssignment::Removed) => {
                    extracted_image = Some(None);
                }
                None => {}
            }
        });

        if let Some(loc) = subsurface_loc {
            self.subsurface_locations.insert(surface.id(), loc);
        }
        if let Some(maybe_img) = extracted_image {
            match maybe_img {
                Some(img) => {
                    self.surface_images.insert(surface.id(), img);
                }
                None => {
                    self.surface_images.remove(&surface.id());
                }
            }
        }
        if let Some(buffer) = buffer_to_release {
            buffer.release();
        }

        on_commit_buffer_handler::<Self>(surface);

        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        if let Some(id) = self.window_id_for_surface(&root) {
            if let Some(meta) = self.windows.get_mut(&id) {
                if !is_sync_subsurface(surface) {
                    meta.window.on_commit();
                }
                meta.commits += 1;
                meta.last_commit = Instant::now();
            }
        }

        handle_xdg_commit(&mut self.popups, &self.space, surface);
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        let id = surface.id();
        self.surface_images.remove(&id);
        self.subsurface_locations.remove(&id);
    }
}

fn extract_shm_image(buffer: &wl_buffer::WlBuffer) -> Result<Option<SurfaceImage>, String> {
    with_buffer_contents(buffer, |ptr, len, data| {
        if data.width <= 0 || data.height <= 0 || data.offset < 0 {
            return None;
        }
        let width = data.width as usize;
        let height = data.height as usize;
        let stride = data.stride as usize;
        let offset = data.offset as usize;
        let row_bytes = width.checked_mul(4)?;
        if data.stride < 0 || stride < row_bytes {
            return None;
        }
        let needed = offset
            .checked_add(height.checked_sub(1)?.checked_mul(stride)?)?
            .checked_add(row_bytes)?;
        if needed > len {
            return None;
        }

        let has_alpha = match data.format {
            wl_shm::Format::Argb8888 => true,
            wl_shm::Format::Xrgb8888 => false,
            _ => return None,
        };

        // Safety: `with_buffer_contents` guarantees `ptr` is valid for `len` bytes during the closure.
        let pool = unsafe { std::slice::from_raw_parts(ptr, len) };
        let pixel_count = width.checked_mul(height)?;
        let mut rgb = Vec::with_capacity(pixel_count * 3);
        let mut alpha = if has_alpha {
            Some(Vec::with_capacity(pixel_count))
        } else {
            None
        };

        for y in 0..height {
            let row_start = offset + y * stride;
            let row = &pool[row_start..row_start + row_bytes];
            for px in row.chunks_exact(4) {
                // Little-endian ARGB8888/XRGB8888 is [B, G, R, A/X] in memory.
                rgb.push(px[2]);
                rgb.push(px[1]);
                rgb.push(px[0]);
                if let Some(ref mut alpha_buf) = alpha {
                    alpha_buf.push(px[3]);
                }
            }
        }

        Some(SurfaceImage {
            width: data.width as u32,
            height: data.height as u32,
            rgb,
            alpha,
        })
    })
    .map_err(|e| e.to_string())
}

impl BufferHandler for PcEyeWm {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for PcEyeWm {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_compositor!(PcEyeWm);
delegate_shm!(PcEyeWm);

impl SeatHandler for PcEyeWm {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<PcEyeWm> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

delegate_seat!(PcEyeWm);

impl SelectionHandler for PcEyeWm {
    type SelectionUserData = ();
}

impl DataDeviceHandler for PcEyeWm {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for PcEyeWm {}
impl ServerDndGrabHandler for PcEyeWm {}

delegate_data_device!(PcEyeWm);

impl OutputHandler for PcEyeWm {}
delegate_output!(PcEyeWm);

impl XdgShellHandler for PcEyeWm {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.size = Some(self.output_size);
            state.states.set(xdg_toplevel::State::Maximized);
            state.states.set(xdg_toplevel::State::Activated);
        });

        let window = Window::new_wayland_window(surface.clone());
        self.space.map_element(window.clone(), (0, 0), true);

        for other in self.space.elements() {
            if other != &window {
                if let Some(toplevel) = other.toplevel() {
                    if toplevel.is_initial_configure_sent() {
                        toplevel.send_pending_configure();
                    }
                }
            }
        }

        let wl_surface = surface.wl_surface().clone();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(wl_surface), SERIAL_COUNTER.next_serial());
        }

        let id = self.next_window_id;
        self.next_window_id += 1;

        self.windows.insert(
            id,
            WindowMeta {
                id,
                window,
                app_id: None,
                title: None,
                commits: 0,
                last_commit: Instant::now(),
            },
        );
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.size = Some(self.output_size);
            state.states.set(xdg_toplevel::State::Maximized);
        });
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        let app_id = with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().app_id.clone())
        });
        if let Some(id) = self.window_id_for_surface(surface.wl_surface()) {
            if let Some(meta) = self.windows.get_mut(&id) {
                meta.app_id = app_id;
            }
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let title = with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().title.clone())
        });
        if let Some(id) = self.window_id_for_surface(surface.wl_surface()) {
            if let Some(meta) = self.windows.get_mut(&id) {
                meta.title = title;
            }
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.window_id_for_surface(surface.wl_surface()) {
            if let Some(meta) = self.windows.remove(&id) {
                self.space.unmap_elem(&meta.window);
            }
        }
        self.surface_images.remove(&surface.wl_surface().id());
        let next_focus = self
            .space
            .elements()
            .next_back()
            .and_then(|w| w.toplevel().map(|t| t.wl_surface().clone()));
        if let Some(keyboard) = self.seat.get_keyboard() {
            if keyboard
                .current_focus()
                .is_some_and(|f| &f == surface.wl_surface())
            {
                keyboard.set_focus(self, next_focus, SERIAL_COUNTER.next_serial());
            }
        }
    }
}

delegate_xdg_shell!(PcEyeWm);

impl XdgDecorationHandler for PcEyeWm {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

delegate_xdg_decoration!(PcEyeWm);

fn handle_xdg_commit(popups: &mut PopupManager, space: &Space<Window>, surface: &WlSurface) {
    if let Some(window) = space
        .elements()
        .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
    {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });
        if !initial_configure_sent {
            window.toplevel().unwrap().send_configure();
        }
    }

    popups.commit(surface);
    if let Some(PopupKind::Xdg(ref xdg)) = popups.find_popup(surface) {
        if !xdg.is_initial_configure_sent() {
            let _ = xdg.send_configure();
        }
    }
}

impl PcEyeWm {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
        else {
            return;
        };
        let Some(output) = self.space.outputs().next() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(window) else {
            return;
        };

        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
