use smithay::input::keyboard::XkbConfig;
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState},
    desktop::WindowSurfaceType,
    input::{
        keyboard::{xkb, FilterResult, Keycode},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::state::PcEyeWm;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

impl PcEyeWm {
    fn time_msec(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    pub fn focus_window(&mut self, id: u64) -> Result<(), String> {
        let window = self
            .windows
            .get(&id)
            .ok_or_else(|| format!("unknown window id: {id}"))?
            .window
            .clone();

        self.space.raise_element(&window, true);
        for other in self.space.elements() {
            if let Some(toplevel) = other.toplevel() {
                if toplevel.is_initial_configure_sent() {
                    toplevel.send_pending_configure();
                }
            }
        }

        let wl_surface = window
            .toplevel()
            .map(|t| t.wl_surface().clone())
            .ok_or_else(|| format!("window {id} has no toplevel surface"))?;

        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(wl_surface), SERIAL_COUNTER.next_serial());
        }
        self.record_focus_history(id);
        Ok(())
    }

    fn surface_under_at(
        &self,
        pos: Point<f64, Logical>,
    ) -> (Option<u64>, Option<(WlSurface, Point<f64, Logical>)>) {
        let Some((window, location)) = self.space.element_under(pos) else {
            return (None, None);
        };
        let win_id = self.window_id_for_window(window);
        let surface = window
            .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
            .map(|(s, p)| (s, (p + location).to_f64()));
        if surface.is_some() {
            (win_id, surface)
        } else {
            (None, None)
        }
    }

    fn sync_pointer_focus(&mut self, pointer: &PointerHandle<Self>, time: u32) {
        if pointer.is_grabbed() {
            return;
        }
        let pos = self.cursor_pos;
        let (_, under) = self.surface_under_at(pos);
        if under.as_ref().map(|(s, _)| s) != pointer.current_focus().as_ref() {
            pointer.motion(
                self,
                under,
                &MotionEvent {
                    location: pos,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
            pointer.frame(self);
        }
    }

    pub fn pointer_move(&mut self, x: f64, y: f64) -> Result<Option<u64>, String> {
        if !x.is_finite() || !y.is_finite() {
            return Err("invalid pointer coordinates".to_string());
        }
        let pos: Point<f64, Logical> = (x, y).into();
        self.cursor_pos = pos;

        let (win_id, under) = self.surface_under_at(pos);
        let pointer = self
            .seat
            .get_pointer()
            .ok_or_else(|| "seat has no pointer".to_string())?;
        let time = self.time_msec();

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        Ok(win_id)
    }

    pub fn pointer_button(&mut self, button: u32, pressed: bool) -> Result<(), String> {
        if pressed {
            let (win_id, _) = self.surface_under_at(self.cursor_pos);
            if let Some(id) = win_id {
                self.focus_window(id)?;
            }
        }

        let pointer = self
            .seat
            .get_pointer()
            .ok_or_else(|| "seat has no pointer".to_string())?;
        let time = self.time_msec();
        if pressed {
            self.sync_pointer_focus(&pointer, time);
        }

        let state = if pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        };
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        Ok(())
    }

    pub fn pointer_axis(&mut self, dx: f64, dy: f64) -> Result<(), String> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err("invalid pointer axis delta".to_string());
        }
        let pointer = self
            .seat
            .get_pointer()
            .ok_or_else(|| "seat has no pointer".to_string())?;
        let time = self.time_msec();
        self.sync_pointer_focus(&pointer, time);

        let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
        if dx != 0.0 {
            frame = frame
                .value(Axis::Horizontal, dx * 15.0)
                .v120(Axis::Horizontal, (dx * 120.0).round() as i32);
        }
        if dy != 0.0 {
            frame = frame
                .value(Axis::Vertical, dy * 15.0)
                .v120(Axis::Vertical, (dy * 120.0).round() as i32);
        }
        pointer.axis(self, frame);
        pointer.frame(self);
        Ok(())
    }

    fn pointer_focus_for_window(
        &self,
        id: u64,
        x: f64,
        y: f64,
    ) -> Result<(Point<f64, Logical>, Option<(WlSurface, Point<f64, Logical>)>), String> {
        if !x.is_finite() || !y.is_finite() {
            return Err("invalid pointer coordinates".to_string());
        }
        let window = self
            .windows
            .get(&id)
            .ok_or_else(|| format!("unknown window id: {id}"))?
            .window
            .clone();

        let win_loc = self
            .space
            .element_location(&window)
            .unwrap_or_else(|| (0, 0).into());
        let global_pos: Point<f64, Logical> = (win_loc.x as f64 + x, win_loc.y as f64 + y).into();

        let under = window
            .surface_under((x, y), WindowSurfaceType::ALL)
            .map(|(s, loc)| (s, (loc + win_loc).to_f64()))
            .or_else(|| {
                self.space.element_under(global_pos).and_then(|(w, loc)| {
                    w.surface_under(global_pos - loc.to_f64(), WindowSurfaceType::ALL)
                        .map(|(s, p)| (s, (p + loc).to_f64()))
                })
            });

        Ok((global_pos, under))
    }

    pub fn inject_move(&mut self, id: u64, x: f64, y: f64) -> Result<bool, String> {
        let (global_pos, under) = self.pointer_focus_for_window(id, x, y)?;
        self.cursor_pos = global_pos;
        let surface_found = under.is_some();
        let pointer = self
            .seat
            .get_pointer()
            .ok_or_else(|| "seat has no pointer".to_string())?;
        let time = self.time_msec();

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: global_pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        Ok(surface_found)
    }

    pub fn inject_click(
        &mut self,
        id: u64,
        x: f64,
        y: f64,
        button: &str,
        count: u32,
    ) -> Result<bool, String> {
        let btn_code = match button {
            "left" => BTN_LEFT,
            "right" => BTN_RIGHT,
            "middle" => BTN_MIDDLE,
            other => return Err(format!("unknown button '{other}', expected left|right|middle")),
        };

        self.focus_window(id)?;
        let (global_pos, under) = self.pointer_focus_for_window(id, x, y)?;
        self.cursor_pos = global_pos;
        let surface_found = under.is_some();
        let pointer = self
            .seat
            .get_pointer()
            .ok_or_else(|| "seat has no pointer".to_string())?;
        let time = self.time_msec();

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: global_pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);

        for _ in 0..count {
            pointer.button(
                self,
                &ButtonEvent {
                    button: btn_code,
                    state: ButtonState::Pressed,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
            pointer.frame(self);
            pointer.button(
                self,
                &ButtonEvent {
                    button: btn_code,
                    state: ButtonState::Released,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
            pointer.frame(self);
        }

        Ok(surface_found)
    }

    pub fn inject_scroll(&mut self, id: u64, x: f64, y: f64, dy: f64) -> Result<(), String> {
        if !dy.is_finite() {
            return Err("invalid scroll delta".to_string());
        }
        let (global_pos, under) = self.pointer_focus_for_window(id, x, y)?;
        self.cursor_pos = global_pos;
        let pointer = self
            .seat
            .get_pointer()
            .ok_or_else(|| "seat has no pointer".to_string())?;
        let time = self.time_msec();

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: global_pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);

        let frame = AxisFrame::new(time)
            .source(AxisSource::Wheel)
            .value(Axis::Vertical, dy * 15.0)
            .v120(Axis::Vertical, (dy * 120.0).round() as i32);
        pointer.axis(self, frame);
        pointer.frame(self);
        Ok(())
    }

    pub fn inject_keys(&mut self, id: Option<u64>, keys: &str) -> Result<(), String> {
        if let Some(win_id) = id {
            self.focus_window(win_id)?;
        }

        let parts: Vec<&str> = if keys.trim() == "+" {
            vec!["+"]
        } else {
            keys.split('+')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect()
        };
        if parts.is_empty() {
            return Err("empty keys specification".to_string());
        }

        let keyboard = self
            .seat
            .get_keyboard()
            .ok_or_else(|| "seat has no keyboard".to_string())?;

        let mut keycodes = Vec::with_capacity(parts.len());
        for part in parts {
            let sym = resolve_keysym(part)
                .ok_or_else(|| format!("unknown keysym name: '{part}'"))?;
            let kc = find_keycode_for_keysym(self, &keyboard, sym)
                .ok_or_else(|| format!("no keycode in keymap for key '{part}'"))?;
            keycodes.push(kc);
        }

        let (mods, main_key) = (&keycodes[..keycodes.len() - 1], keycodes[keycodes.len() - 1]);
        let time = self.time_msec();

        for &kc in mods {
            keyboard.input::<(), _>(
                self,
                kc,
                KeyState::Pressed,
                SERIAL_COUNTER.next_serial(),
                time,
                |_, _, _| FilterResult::Forward,
            );
        }
        keyboard.input::<(), _>(
            self,
            main_key,
            KeyState::Pressed,
            SERIAL_COUNTER.next_serial(),
            time,
            |_, _, _| FilterResult::Forward,
        );
        keyboard.input::<(), _>(
            self,
            main_key,
            KeyState::Released,
            SERIAL_COUNTER.next_serial(),
            time,
            |_, _, _| FilterResult::Forward,
        );
        for &kc in mods.iter().rev() {
            keyboard.input::<(), _>(
                self,
                kc,
                KeyState::Released,
                SERIAL_COUNTER.next_serial(),
                time,
                |_, _, _| FilterResult::Forward,
            );
        }

        Ok(())
    }

    pub fn inject_text(&mut self, id: Option<u64>, text: &str) -> Result<Vec<String>, String> {
        if let Some(win_id) = id {
            self.focus_window(win_id)?;
        }

        let keyboard = self
            .seat
            .get_keyboard()
            .ok_or_else(|| "seat has no keyboard".to_string())?;

        let shift_sym = xkb::keysym_from_name("Shift_L", xkb::KEYSYM_NO_FLAGS);
        let shift_kc = if shift_sym.raw() != xkb::keysyms::KEY_NoSymbol {
            find_keycode_for_keysym(self, &keyboard, shift_sym)
        } else {
            None
        };

        let time = self.time_msec();
        let mut skipped = Vec::new();
        let mut switched = false;

        for ch in text.chars() {
            let sym = match ch {
                '\n' | '\r' => xkb::Keysym::from(xkb::keysyms::KEY_Return),
                '\t' => xkb::Keysym::from(xkb::keysyms::KEY_Tab),
                _ => xkb::utf32_to_keysym(ch as u32),
            };
            if sym.raw() == xkb::keysyms::KEY_NoSymbol {
                skipped.push(ch.to_string());
                continue;
            }

            let mut found = find_keycode_and_level_for_keysym(self, &keyboard, sym);
            // ABD düzeninde olmayan Türkçe harfler (ğüşıöçİ): düzen geçici olarak "tr" yapılır
            // (istemciye yeni keymap gider), metin bitince "us"e dönülür.
            if found.is_none() && !switched {
                let cfg = XkbConfig { layout: "tr", ..Default::default() };
                if keyboard.set_xkb_config(self, cfg).is_ok() {
                    switched = true;
                    found = find_keycode_and_level_for_keysym(self, &keyboard, sym);
                }
            }
            let Some((kc, level)) = found else {
                skipped.push(ch.to_string());
                continue;
            };

            if level == 1 {
                let Some(skc) = shift_kc else {
                    skipped.push(ch.to_string());
                    continue;
                };
                keyboard.input::<(), _>(
                    self,
                    skc,
                    KeyState::Pressed,
                    SERIAL_COUNTER.next_serial(),
                    time,
                    |_, _, _| FilterResult::Forward,
                );
                keyboard.input::<(), _>(
                    self,
                    kc,
                    KeyState::Pressed,
                    SERIAL_COUNTER.next_serial(),
                    time,
                    |_, _, _| FilterResult::Forward,
                );
                keyboard.input::<(), _>(
                    self,
                    kc,
                    KeyState::Released,
                    SERIAL_COUNTER.next_serial(),
                    time,
                    |_, _, _| FilterResult::Forward,
                );
                keyboard.input::<(), _>(
                    self,
                    skc,
                    KeyState::Released,
                    SERIAL_COUNTER.next_serial(),
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            } else {
                keyboard.input::<(), _>(
                    self,
                    kc,
                    KeyState::Pressed,
                    SERIAL_COUNTER.next_serial(),
                    time,
                    |_, _, _| FilterResult::Forward,
                );
                keyboard.input::<(), _>(
                    self,
                    kc,
                    KeyState::Released,
                    SERIAL_COUNTER.next_serial(),
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            }
        }

        if switched {
            let _ = keyboard.set_xkb_config(self, XkbConfig::default());
        }
        Ok(skipped)
    }
}

fn resolve_keysym(name: &str) -> Option<xkb::Keysym> {
    let mapped = match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "ctrl_l" | "lctrl" => "Control_L",
        "ctrl_r" | "rctrl" => "Control_R",
        "shift" | "shift_l" | "lshift" => "Shift_L",
        "shift_r" | "rshift" => "Shift_R",
        "alt" | "alt_l" | "lalt" | "mod1" => "Alt_L",
        "alt_r" | "ralt" => "Alt_R",
        "super" | "super_l" | "lsuper" | "win" | "logo" | "meta" => "Super_L",
        "super_r" | "rsuper" => "Super_R",
        "enter" | "return" => "Return",
        "esc" | "escape" => "Escape",
        "backspace" | "bksp" => "BackSpace",
        "tab" => "Tab",
        "space" => "space",
        "del" | "delete" => "Delete",
        "ins" | "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" | "pgup" => "Page_Up",
        "pagedown" | "page_down" | "pgdn" => "Page_Down",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "+" => "plus",
        "-" => "minus",
        _ => name,
    };

    let sym = xkb::keysym_from_name(mapped, xkb::KEYSYM_NO_FLAGS);
    if sym.raw() != xkb::keysyms::KEY_NoSymbol {
        return Some(sym);
    }
    let sym_ci = xkb::keysym_from_name(mapped, xkb::KEYSYM_CASE_INSENSITIVE);
    if sym_ci.raw() != xkb::keysyms::KEY_NoSymbol {
        return Some(sym_ci);
    }
    None
}

fn find_keycode_for_keysym(
    state: &mut PcEyeWm,
    keyboard: &smithay::input::keyboard::KeyboardHandle<PcEyeWm>,
    sym: xkb::Keysym,
) -> Option<Keycode> {
    keyboard.with_xkb_state(state, |ctx| {
        let xkb = ctx.xkb().lock().unwrap();
        let layout = xkb.active_layout().0;
        // Safety: keymap reference does not outlive the Xkb lock guard.
        let keymap = unsafe { xkb.keymap() };
        let min = keymap.min_keycode().raw();
        let max = keymap.max_keycode().raw();

        for target_level in 0..4u32 {
            for code in min..=max {
                let kc = Keycode::from(code);
                let levels = keymap.num_levels_for_key(kc, layout);
                if target_level < levels {
                    let syms = keymap.key_get_syms_by_level(kc, layout, target_level);
                    if syms.contains(&sym) {
                        return Some(kc);
                    }
                }
            }
        }
        None
    })
}

fn find_keycode_and_level_for_keysym(
    state: &mut PcEyeWm,
    keyboard: &smithay::input::keyboard::KeyboardHandle<PcEyeWm>,
    sym: xkb::Keysym,
) -> Option<(Keycode, u32)> {
    keyboard.with_xkb_state(state, |ctx| {
        let xkb = ctx.xkb().lock().unwrap();
        let layout = xkb.active_layout().0;
        // Safety: keymap reference does not outlive the Xkb lock guard.
        let keymap = unsafe { xkb.keymap() };
        let min = keymap.min_keycode().raw();
        let max = keymap.max_keycode().raw();

        for target_level in 0..=1u32 {
            for code in min..=max {
                let kc = Keycode::from(code);
                let levels = keymap.num_levels_for_key(kc, layout);
                if target_level < levels {
                    let syms = keymap.key_get_syms_by_level(kc, layout, target_level);
                    if syms.contains(&sym) {
                        return Some((kc, target_level));
                    }
                }
            }
        }
        None
    })
}
