use std::{os::unix::io::OwnedFd, time::Instant};

use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, format::has_alpha, Buffer as _, Fourcc},
        renderer::{
            gles::{GlesRenderbuffer, GlesRenderer},
            utils::on_commit_buffer_handler,
            Bind, Color32F, ExportMem, Frame, ImportDma, Offscreen, Renderer, Texture,
            TextureMapping,
        },
    },
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat,
    delegate_shm, delegate_xdg_decoration, delegate_xdg_shell, delegate_xwayland_keyboard_grab,
    delegate_xwayland_shell,
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, space::RenderZindex, PopupKind,
        PopupManager, Space, Window,
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
    utils::{Logical, Point, Rectangle, Serial, Transform, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, get_role, is_sync_subsurface, with_states, BufferAssignment,
            CompositorClientState, CompositorHandler, CompositorState, SubsurfaceCachedState,
            SurfaceAttributes, SUBSURFACE_ROLE,
        },
        dmabuf::{get_dmabuf, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        output::{OutputHandler, OutputManagerState},
        seat::WaylandFocus,
        selection::{
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_focus,
                set_data_device_selection, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
                ServerDndGrabHandler,
            },
            SelectionHandler, SelectionTarget,
        },
        shell::xdg::{
            decoration::{XdgDecorationHandler, XdgDecorationState},
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{with_buffer_contents, ShmHandler, ShmState},
        xwayland_keyboard_grab::XWaylandKeyboardGrabHandler,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge as X11ResizeEdge, WmWindowProperty, X11Window, XwmId},
        X11Surface, X11Wm, XWaylandClientData, XwmHandler,
    },
};

use crate::state::{CalloopData, ClientState, FocusTarget, PcEyeWm, SurfaceImage, WindowMeta};

impl CompositorHandler for PcEyeWm {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            return &state.compositor_state;
        }
        if let Some(state) = client.get_data::<ClientState>() {
            return &state.compositor_state;
        }
        panic!("Unknown client data type")
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.commit_inner(surface);
        // Kare gelir gelmez bekleyenler yanıtlanır (yalnız 16 ms'lik zamanlayıcıyı beklemek eylem
        // doğrulamasına kare başına ~8 ms ortalama gecikme ekliyordu). Yanıtlar istemciye hemen gider.
        if !self.waiters.is_empty() {
            crate::ipc::resolve_waiters(self);
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        let id = surface.id();
        self.surface_images.remove(&id);
        self.subsurface_locations.remove(&id);
    }
}

impl PcEyeWm {
    fn commit_inner(&mut self, surface: &WlSurface) {
        let mut extracted_image: Option<Option<SurfaceImage>> = None;
        let mut subsurface_loc: Option<Point<i32, Logical>> = None;
        let mut new_buffer: Option<wl_buffer::WlBuffer> = None;

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
                    new_buffer = Some(buffer.clone());
                }
                Some(BufferAssignment::Removed) => {
                    extracted_image = Some(None);
                }
                None => {}
            }
        });

        let mut buffer_to_release: Option<wl_buffer::WlBuffer> = None;
        if let Some(buffer) = new_buffer {
            if let Ok(dmabuf) = get_dmabuf(&buffer) {
                let dmabuf = dmabuf.clone();
                if let Some(renderer) = self.renderer.as_mut() {
                    match extract_dmabuf_image(renderer, &mut self.dmabuf_renderbuffer, &dmabuf) {
                        Ok(Some(img)) => {
                            extracted_image = Some(Some(img));
                        }
                        Ok(None) => {}
                        Err(err) => {
                            tracing::debug!(?err, "skipping dmabuf frame pixel update");
                        }
                    }
                }
            } else {
                match extract_shm_image(&buffer) {
                    Ok(Some(img)) => {
                        extracted_image = Some(Some(img));
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::debug!(?err, "skipping non-shm or invalid buffer");
                    }
                }
            }
            buffer_to_release = Some(buffer);
        }

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

        let mut announce_event: Option<(u64, String, String)> = None;
        if let Some(id) = self.window_id_for_surface(&root) {
            let has_pixels = self.surface_images.contains_key(&root.id());
            if let Some(meta) = self.windows.get_mut(&id) {
                if !is_sync_subsurface(surface) {
                    meta.window.on_commit();
                }
                meta.commits += 1;
                meta.last_commit = Instant::now();
                if has_pixels && !meta.announced {
                    meta.announced = true;
                    announce_event = Some((
                        id,
                        meta.app_id.clone().unwrap_or_default(),
                        meta.title.clone().unwrap_or_default(),
                    ));
                }
            }
        }

        handle_xdg_commit(&mut self.popups, &self.space, surface);

        if let Some((id, app_id, title)) = announce_event {
            self.emit_openwindow(id, &app_id, &title);
            let is_focused = self
                .seat
                .get_keyboard()
                .and_then(|k| k.current_focus())
                .is_some_and(|f| f.wl_surface().as_deref() == Some(&root));
            if is_focused {
                self.emit_activewindow(Some((id, &app_id, &title)));
            }
        }
    }

}

fn extract_dmabuf_image(
    renderer: &mut GlesRenderer,
    cached_rbo: &mut Option<((i32, i32), GlesRenderbuffer)>,
    dmabuf: &Dmabuf,
) -> Result<Option<SurfaceImage>, String> {
    let texture = renderer
        .import_dmabuf(dmabuf, None)
        .map_err(|err| format!("import_dmabuf failed: {err:?}"))?;
    let size = texture.size();
    if size.w <= 0 || size.h <= 0 {
        return Ok(None);
    }

    let width = size.w as usize;
    let height = size.h as usize;
    let pixel_count = width
        .checked_mul(height)
        .ok_or_else(|| "dmabuf pixel count overflow".to_string())?;
    let expected_bytes = pixel_count
        .checked_mul(4)
        .ok_or_else(|| "dmabuf byte size overflow".to_string())?;

    let mut offscreen = match cached_rbo.take() {
        Some((dims, rbo)) if dims == (size.w, size.h) => rbo,
        _ => Offscreen::<GlesRenderbuffer>::create_buffer(renderer, Fourcc::Abgr8888, size)
            .map_err(|err| format!("create offscreen buffer failed: {err:?}"))?,
    };

    let render_result: Result<_, String> = (|| {
        let mut framebuffer = renderer
            .bind(&mut offscreen)
            .map_err(|err| format!("bind offscreen buffer failed: {err:?}"))?;
        let damage_rect = Rectangle::from_size((size.w, size.h).into());

        let sync = {
            let mut frame = renderer
                .render(&mut framebuffer, (size.w, size.h).into(), Transform::Normal)
                .map_err(|err| format!("renderer.render failed: {err:?}"))?;
            frame
                .clear(Color32F::TRANSPARENT, &[damage_rect])
                .map_err(|err| format!("frame.clear failed: {err:?}"))?;
            frame
                .render_texture_at(
                    &texture,
                    (0, 0).into(),
                    1,
                    1.0,
                    Transform::Normal,
                    &[damage_rect],
                    &[],
                    1.0,
                )
                .map_err(|err| format!("render_texture_at failed: {err:?}"))?;
            frame
                .finish()
                .map_err(|err| format!("frame.finish failed: {err:?}"))?
        };
        if let Err(err) = sync.wait() {
            tracing::debug!(?err, "sync wait interrupted before dmabuf readback");
        }

        renderer
            .copy_framebuffer(
                &framebuffer,
                Rectangle::from_size((size.w, size.h).into()),
                Fourcc::Abgr8888,
            )
            .map_err(|err| format!("copy_framebuffer failed: {err:?}"))
    })();

    *cached_rbo = Some(((size.w, size.h), offscreen));
    let mapping = render_result?;

    let flipped = mapping.flipped();
    let pixels = renderer
        .map_texture(&mapping)
        .map_err(|err| format!("map_texture failed: {err:?}"))?;
    if pixels.len() < expected_bytes {
        return Err(format!(
            "mapped dmabuf texture too small: {} < {}",
            pixels.len(),
            expected_bytes
        ));
    }

    let has_alpha_chan = has_alpha(dmabuf.format().code);
    let mut rgb = Vec::with_capacity(pixel_count * 3);
    let mut alpha = if has_alpha_chan {
        Some(Vec::with_capacity(pixel_count))
    } else {
        None
    };

    let row_bytes = width * 4;
    let push_row = |y: usize, rgb: &mut Vec<u8>, alpha: &mut Option<Vec<u8>>| {
        let row_start = y * row_bytes;
        let row = &pixels[row_start..row_start + row_bytes];
        for px in row.chunks_exact(4) {
            // `Fourcc::Abgr8888` in GL (`GL_RGBA`/`GL_UNSIGNED_BYTE`) is [R, G, B, A] in memory.
            rgb.push(px[0]);
            rgb.push(px[1]);
            rgb.push(px[2]);
            if let Some(ref mut alpha_buf) = alpha {
                alpha_buf.push(px[3]);
            }
        }
    };

    // `GlesMapping::flipped() == true` means top-down row order from `Transform::Normal` FBO render.
    if flipped {
        for y in 0..height {
            push_row(y, &mut rgb, &mut alpha);
        }
    } else {
        for y in (0..height).rev() {
            push_row(y, &mut rgb, &mut alpha);
        }
    }

    Ok(Some(SurfaceImage {
        width: size.w as u32,
        height: size.h as u32,
        rgb,
        alpha,
    }))
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

impl DmabufHandler for PcEyeWm {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let Some(renderer) = self.renderer.as_mut() else {
            notifier.failed();
            return;
        };
        match renderer.import_dmabuf(&dmabuf, None) {
            Ok(_) => {
                let _ = notifier.successful::<Self>();
            }
            Err(err) => {
                tracing::debug!(?err, "dmabuf import failed");
                notifier.failed();
            }
        }
    }
}

delegate_compositor!(PcEyeWm);
delegate_shm!(PcEyeWm);
delegate_dmabuf!(PcEyeWm);

impl SeatHandler for PcEyeWm {
    type KeyboardFocus = FocusTarget;
    type PointerFocus = FocusTarget;
    type TouchFocus = FocusTarget;

    fn seat_state(&mut self) -> &mut SeatState<PcEyeWm> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&FocusTarget>) {
        let dh = &self.display_handle;
        let client = focused
            .and_then(|f| f.wl_surface())
            .and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);

        let active_info = focused
            .and_then(|f| self.window_id_for_focus(f))
            .and_then(|id| {
                self.windows.get(&id).and_then(|meta| {
                    meta.announced.then(|| {
                        (
                            id,
                            meta.app_id.clone().unwrap_or_default(),
                            meta.title.clone().unwrap_or_default(),
                        )
                    })
                })
            });

        if focused.is_none() {
            self.emit_activewindow(None);
        } else if let Some((id, app_id, title)) = active_info {
            self.emit_activewindow(Some((id, &app_id, &title)));
        }
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
            // Tam ekran: istemci başlık çubuğu (CSD) çizilmez; ajan Hyprland oturumundaki gibi
            // yalnız uygulamanın içeriğini görür (başlık düğmeleri ağacı kirletiyordu).
            state.states.set(xdg_toplevel::State::Fullscreen);
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
                announced: false,
            },
        );
        self.record_focus_history(id);

        let wl_surface = surface.wl_surface().clone();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(
                self,
                Some(FocusTarget::Wayland(wl_surface)),
                SERIAL_COUNTER.next_serial(),
            );
        }
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
        let mut emit_title: Option<(u64, String)> = None;
        if let Some(id) = self.window_id_for_surface(surface.wl_surface()) {
            if let Some(meta) = self.windows.get_mut(&id) {
                meta.title = title;
                if meta.announced {
                    emit_title = Some((id, meta.title.clone().unwrap_or_default()));
                }
            }
        }
        if let Some((id, title)) = emit_title {
            self.emit_windowtitle(id, &title);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let mut closed_id: Option<u64> = None;
        if let Some(id) = self.window_id_for_surface(surface.wl_surface()) {
            if let Some(meta) = self.windows.remove(&id) {
                self.space.unmap_elem(&meta.window);
                if meta.announced {
                    closed_id = Some(id);
                }
            }
            self.focus_history.retain(|&fid| fid != id);
        }
        self.surface_images.remove(&surface.wl_surface().id());
        self.subsurface_locations.remove(&surface.wl_surface().id());

        if let Some(id) = closed_id {
            self.emit_closewindow(id);
        }

        let was_focused = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .is_some_and(|f| f.wl_surface().as_deref() == Some(surface.wl_surface()));

        if was_focused {
            let next_id = self
                .focus_history
                .iter()
                .copied()
                .find(|fid| self.windows.contains_key(fid));
            if let Some(next_id) = next_id {
                let _ = self.focus_window(next_id);
            } else if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
                self.emit_activewindow(None);
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

impl XWaylandKeyboardGrabHandler for PcEyeWm {
    fn keyboard_focus_for_xsurface(&self, surface: &WlSurface) -> Option<FocusTarget> {
        let elem = self
            .space
            .elements()
            .find(|elem| elem.wl_surface().as_deref() == Some(surface))?;
        elem.x11_surface().cloned().map(FocusTarget::X11)
    }
}

delegate_xwayland_keyboard_grab!(PcEyeWm);

impl XWaylandShellHandler for PcEyeWm {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, wl_surface: WlSurface, surface: X11Surface) {
        let mut announce_event: Option<(u64, String, String)> = None;
        let mut should_focus_id: Option<u64> = None;

        if let Some(id) = self.window_id_for_x11(&surface) {
            let has_pixels = self.surface_images.contains_key(&wl_surface.id());
            if let Some(meta) = self.windows.get_mut(&id) {
                if has_pixels {
                    meta.window.on_commit();
                    if !meta.announced {
                        meta.announced = true;
                        announce_event = Some((
                            id,
                            meta.app_id.clone().unwrap_or_default(),
                            meta.title.clone().unwrap_or_default(),
                        ));
                    }
                }
            }
            if self.focus_history.first().copied() == Some(id) {
                should_focus_id = Some(id);
            }
        }

        if let Some((id, app_id, title)) = announce_event {
            self.emit_openwindow(id, &app_id, &title);
        }
        if let Some(id) = should_focus_id {
            let _ = self.focus_window(id);
        }
    }
}

delegate_xwayland_shell!(PcEyeWm);

impl XwmHandler for PcEyeWm {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("X11Wm not initialized")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let rect = Rectangle::new((0, 0).into(), self.output_size);
        let _ = window.set_fullscreen(true);
        let _ = window.configure(rect);
        if let Err(err) = window.set_mapped(true) {
            tracing::warn!(?err, "failed to map X11 window");
            return;
        }

        if let Some(existing_id) = self.window_id_for_x11(&window) {
            let _ = self.focus_window(existing_id);
            return;
        }

        let desktop_window = Window::new_x11_window(window.clone());
        self.space.map_element(desktop_window.clone(), (0, 0), true);

        for other in self.space.elements() {
            if other != &desktop_window {
                if let Some(toplevel) = other.toplevel() {
                    if toplevel.is_initial_configure_sent() {
                        toplevel.send_pending_configure();
                    }
                }
            }
        }

        let id = self.next_window_id;
        self.next_window_id += 1;

        let class = window.class();
        let title = window.title();
        let app_id = (!class.is_empty()).then_some(class);
        let title_opt = (!title.is_empty()).then_some(title);

        let has_pixels = window
            .wl_surface()
            .is_some_and(|s| self.surface_images.contains_key(&s.id()));

        let mut meta = WindowMeta {
            id,
            window: desktop_window,
            app_id: app_id.clone(),
            title: title_opt.clone(),
            commits: 0,
            last_commit: Instant::now(),
            announced: false,
        };
        if has_pixels {
            meta.window.on_commit();
            meta.announced = true;
        }

        self.windows.insert(id, meta);

        if has_pixels {
            self.emit_openwindow(
                id,
                app_id.as_deref().unwrap_or_default(),
                title_opt.as_deref().unwrap_or_default(),
            );
        }

        let _ = self.focus_window(id);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let location = window.geometry().loc;
        let desktop_window = Window::new_x11_window(window);
        desktop_window.override_z_index(RenderZindex::Overlay as u8);
        self.space.map_element(desktop_window, location, false);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.remove_x11_window(&window);
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.remove_x11_window(&window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        _w: Option<u32>,
        _h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let rect = Rectangle::new((0, 0).into(), self.output_size);
        let _ = window.configure(rect);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<X11Window>,
    ) {
        if window.is_override_redirect() {
            let target_id = window.window_id();
            let found = self
                .space
                .elements()
                .find(|w| w.x11_surface().is_some_and(|x| x.window_id() == target_id))
                .cloned();
            if let Some(elem) = found {
                self.space.map_element(elem, geometry.loc, false);
            }
        }
    }

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        match property {
            WmWindowProperty::Title => {
                let title = window.title();
                let new_title = (!title.is_empty()).then_some(title);
                let mut emit_title: Option<(u64, String)> = None;
                if let Some(id) = self.window_id_for_x11(&window) {
                    if let Some(meta) = self.windows.get_mut(&id) {
                        meta.title = new_title;
                        if meta.announced {
                            emit_title = Some((id, meta.title.clone().unwrap_or_default()));
                        }
                    }
                }
                if let Some((id, title)) = emit_title {
                    self.emit_windowtitle(id, &title);
                }
            }
            WmWindowProperty::Class => {
                let class = window.class();
                let new_app_id = (!class.is_empty()).then_some(class);
                if let Some(id) = self.window_id_for_x11(&window) {
                    if let Some(meta) = self.windows.get_mut(&id) {
                        meta.app_id = new_app_id;
                    }
                }
            }
            _ => {}
        }
    }

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_maximized(true);
        let _ = window.configure(Rectangle::new((0, 0).into(), self.output_size));
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_maximized(false);
        let _ = window.configure(Rectangle::new((0, 0).into(), self.output_size));
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_fullscreen(true);
        let _ = window.configure(Rectangle::new((0, 0).into(), self.output_size));
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.configure(Rectangle::new((0, 0).into(), self.output_size));
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _resize_edge: X11ResizeEdge,
    ) {
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn allow_selection_access(&mut self, xwm: XwmId, selection: SelectionTarget) -> bool {
        if selection != SelectionTarget::Clipboard {
            return false;
        }
        self.seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .is_some_and(|f| matches!(f, FocusTarget::X11(s) if s.xwm_id() == Some(xwm)))
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        if selection == SelectionTarget::Clipboard {
            if let Err(err) = request_data_device_client_selection(&self.seat, mime_type, fd) {
                tracing::warn!(?err, "failed to request Wayland clipboard for Xwayland");
            }
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        if selection == SelectionTarget::Clipboard {
            set_data_device_selection(&self.display_handle, &self.seat, mime_types, ());
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        if selection == SelectionTarget::Clipboard
            && current_data_device_selection_userdata(&self.seat).is_some()
        {
            clear_data_device_selection(&self.display_handle, &self.seat);
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwm = None;
        self.x_display = None;
    }
}

impl XWaylandShellHandler for CalloopData {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        self.state.xwayland_shell_state()
    }

    fn surface_associated(&mut self, xwm: XwmId, wl_surface: WlSurface, surface: X11Surface) {
        self.state.surface_associated(xwm, wl_surface, surface);
    }
}

impl XwmHandler for CalloopData {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        self.state.xwm_state(xwm)
    }

    fn new_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_window(xwm, window);
    }

    fn new_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.new_override_redirect_window(xwm, window);
    }

    fn map_window_request(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.map_window_request(xwm, window);
    }

    fn map_window_notify(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.map_window_notify(xwm, window);
    }

    fn mapped_override_redirect_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.mapped_override_redirect_window(xwm, window);
    }

    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.unmapped_window(xwm, window);
    }

    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.state.destroyed_window(xwm, window);
    }

    fn configure_request(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        self.state.configure_request(xwm, window, x, y, w, h, reorder);
    }

    fn configure_notify(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        above: Option<X11Window>,
    ) {
        self.state.configure_notify(xwm, window, geometry, above);
    }

    fn property_notify(&mut self, xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        self.state.property_notify(xwm, window, property);
    }

    fn maximize_request(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::maximize_request(&mut self.state, xwm, window);
    }

    fn unmaximize_request(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::unmaximize_request(&mut self.state, xwm, window);
    }

    fn fullscreen_request(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::fullscreen_request(&mut self.state, xwm, window);
    }

    fn unfullscreen_request(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::unfullscreen_request(&mut self.state, xwm, window);
    }

    fn minimize_request(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::minimize_request(&mut self.state, xwm, window);
    }

    fn unminimize_request(&mut self, xwm: XwmId, window: X11Surface) {
        XwmHandler::unminimize_request(&mut self.state, xwm, window);
    }

    fn resize_request(
        &mut self,
        xwm: XwmId,
        window: X11Surface,
        button: u32,
        resize_edge: X11ResizeEdge,
    ) {
        XwmHandler::resize_request(&mut self.state, xwm, window, button, resize_edge);
    }

    fn move_request(&mut self, xwm: XwmId, window: X11Surface, button: u32) {
        XwmHandler::move_request(&mut self.state, xwm, window, button);
    }

    fn allow_selection_access(&mut self, xwm: XwmId, selection: SelectionTarget) -> bool {
        self.state.allow_selection_access(xwm, selection)
    }

    fn send_selection(
        &mut self,
        xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        XwmHandler::send_selection(&mut self.state, xwm, selection, mime_type, fd);
    }

    fn new_selection(&mut self, xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        XwmHandler::new_selection(&mut self.state, xwm, selection, mime_types);
    }

    fn cleared_selection(&mut self, xwm: XwmId, selection: SelectionTarget) {
        self.state.cleared_selection(xwm, selection);
    }

    fn randr_primary_output_change(&mut self, xwm: XwmId, output_name: Option<String>) {
        self.state.randr_primary_output_change(xwm, output_name);
    }

    fn disconnected(&mut self, xwm: XwmId) {
        self.state.disconnected(xwm);
    }
}

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
    fn remove_x11_window(&mut self, window: &X11Surface) {
        let target_id = window.window_id();
        let maybe_elem = self
            .space
            .elements()
            .find(|w| w.x11_surface().is_some_and(|x| x.window_id() == target_id))
            .cloned();
        if let Some(elem) = maybe_elem {
            self.space.unmap_elem(&elem);
        }

        let mut closed_id: Option<u64> = None;
        if let Some(id) = self.window_id_for_x11(window) {
            if let Some(meta) = self.windows.remove(&id) {
                self.space.unmap_elem(&meta.window);
                if meta.announced {
                    closed_id = Some(id);
                }
            }
            self.focus_history.retain(|&fid| fid != id);
        }

        if let Some(wl_surface) = window.wl_surface() {
            self.surface_images.remove(&wl_surface.id());
            self.subsurface_locations.remove(&wl_surface.id());
        }

        if let Some(id) = closed_id {
            self.emit_closewindow(id);
        }

        let was_focused = self
            .seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .is_some_and(|f| match &f {
                FocusTarget::X11(x) => x.window_id() == target_id,
                FocusTarget::Wayland(s) => window.wl_surface().as_ref() == Some(s),
            });

        if was_focused {
            let next_id = self
                .focus_history
                .iter()
                .copied()
                .find(|fid| self.windows.contains_key(fid));
            if let Some(next_id) = next_id {
                let _ = self.focus_window(next_id);
            } else if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
                self.emit_activewindow(None);
            }
        }
    }

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
