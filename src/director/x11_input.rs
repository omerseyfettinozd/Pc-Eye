//! XWayland istemcileri için X11 (XTest) klavye yolu.
//!
//! Wayland sanal klavyesi ve dinamik XKB keymap'i XWayland'e düzgün çevrilmediği için
//! (yanlış karakterler, Ctrl modifier'ının düşmesi) XWayland hedeflerine tuşlar X sunucusunun
//! kendi klavye eşlemesi üzerinden (`GetKeyboardMapping`) bulunup `XTestFakeKeyEvent` ile
//! modifier'lar tuşun etrafında basılıp bırakılarak gönderilir.

use anyhow::{anyhow, Context, Result};
use std::thread;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, KEY_PRESS_EVENT, KEY_RELEASE_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const KS_SHIFT_L: u32 = 0xffe1;
const KS_CONTROL_L: u32 = 0xffe3;
const KS_ALT_L: u32 = 0xffe9;
const KS_SUPER_L: u32 = 0xffeb;
const KS_ISO_LEVEL3_SHIFT: u32 = 0xfe03;
const KS_MODE_SWITCH: u32 = 0xff7e;

/// Tek bir keysym'i üretmek için gereken tuş ve modifier'lar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPlan {
    pub keycode: u8,
    pub shift: bool,
    pub level3: bool,
}

/// X klavye eşlemesinin anlık görüntüsü (saf veri; birim test edilebilir).
#[derive(Debug, Clone)]
pub struct KeyMap {
    pub min_keycode: u8,
    pub per: usize,
    pub syms: Vec<u32>,
}

impl KeyMap {
    fn row(&self, keycode: u8) -> &[u32] {
        let i = (keycode - self.min_keycode) as usize * self.per;
        &self.syms[i..i + self.per]
    }

    fn keycodes(&self) -> impl DoubleEndedIterator<Item = u8> + '_ {
        let n = self.syms.len() / self.per.max(1);
        (0..n).map(move |i| self.min_keycode + i as u8)
    }

    /// Keysym'i birinci grup sütunlarında arar: 0 = düz, 1 = Shift, 4 = Level3, 5 = Shift+Level3.
    pub fn find(&self, keysym: u32) -> Option<KeyPlan> {
        for (col, shift, level3) in [(0, false, false), (1, true, false), (4, false, true), (5, true, true)] {
            if col >= self.per {
                continue;
            }
            for kc in self.keycodes() {
                if self.row(kc)[col] == keysym {
                    return Some(KeyPlan { keycode: kc, shift, level3 });
                }
            }
        }
        None
    }

    /// Hiç keysym'i olmayan (geçici yeniden eşleme için kullanılabilir) bir tuş kodu.
    pub fn spare_keycode(&self) -> Option<u8> {
        self.keycodes().rev().find(|&kc| self.row(kc).iter().all(|&s| s == 0))
    }

    /// Düz (Shift'siz, birinci düzey) tuş kodu — modifier tuşları için.
    pub fn plain_keycode(&self, keysym: u32) -> Option<u8> {
        self.keycodes().find(|&kc| self.row(kc).contains(&keysym))
    }
}

/// Karakter için denenecek keysym adayları (Unicode keysym + eski Latin-3/5 adları).
pub fn char_keysym_candidates(ch: char) -> Vec<u32> {
    let cp = ch as u32;
    let mut v = Vec::new();
    match ch {
        '\n' | '\r' => v.push(0xff0d),
        '\t' => v.push(0xff09),
        '\u{8}' => v.push(0xff08),
        _ => {
            if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
                v.push(cp);
            }
            match ch {
                'Ş' => v.push(0x1aa),
                'ş' => v.push(0x1ba),
                'Ğ' => v.push(0x2ab),
                'ğ' => v.push(0x2bb),
                'İ' => v.extend([0x2a9, 0x1a9]),
                'ı' => v.extend([0x2b9, 0x1b9]),
                _ => {}
            }
            v.push(0x0100_0000 | cp);
        }
    }
    v
}

/// Tuş adını keysym'e çevirir (`Return`, `ctrl` dışı tuşlar, `F5`, tek karakter).
pub fn name_keysym(name: &str) -> Option<u32> {
    let l = name.to_ascii_lowercase();
    let named = match l.as_str() {
        "return" | "enter" => Some(0xff0d),
        "escape" | "esc" => Some(0xff1b),
        "tab" => Some(0xff09),
        "space" => Some(0x20),
        "backspace" | "bs" => Some(0xff08),
        "delete" | "del" => Some(0xffff),
        "insert" | "ins" => Some(0xff63),
        "home" => Some(0xff50),
        "left" => Some(0xff51),
        "up" => Some(0xff52),
        "right" => Some(0xff53),
        "down" => Some(0xff54),
        "pageup" | "pgup" | "prior" => Some(0xff55),
        "pagedown" | "pgdn" | "next" => Some(0xff56),
        "end" => Some(0xff57),
        "menu" => Some(0xff67),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if let Some(n) = l.strip_prefix('f').and_then(|r| r.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0xffbe + n - 1);
        }
    }
    let mut it = name.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => char_keysym_candidates(c).into_iter().next(),
        _ => None,
    }
}

pub struct X11Keyboard {
    conn: RustConnection,
    map: KeyMap,
}

impl X11Keyboard {
    pub fn connect() -> Result<Self> {
        let (conn, _) = x11rb::connect(None).context("X11 (XWayland) sunucusuna bağlanılamadı")?;
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let m = conn
            .get_keyboard_mapping(min, max - min + 1)?
            .reply()
            .context("GetKeyboardMapping başarısız")?;
        let map = KeyMap { min_keycode: min, per: m.keysyms_per_keycode as usize, syms: m.keysyms };
        conn.xtest_get_version(2, 2)?.reply().context("XTEST eklentisi yok")?;
        Ok(Self { conn, map })
    }

    fn sync(&self) -> Result<()> {
        self.conn.flush()?;
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }

    fn fake(&self, press: bool, keycode: u8) -> Result<()> {
        let t = if press { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT };
        self.conn.xtest_fake_input(t, keycode, 0, x11rb::NONE, 0, 0, 0)?;
        Ok(())
    }

    fn modifier_keycode(&self, keysym: u32) -> Result<u8> {
        self.map
            .plain_keycode(keysym)
            .ok_or_else(|| anyhow!("X klavye eşlemesinde modifier keysym 0x{:x} yok", keysym))
    }

    /// Verilen tuş planını (modifier'lar basılı) bas-bırak yapar.
    fn tap(&self, plan: KeyPlan, extra_mods: &[u32]) -> Result<()> {
        let mut mods: Vec<u8> = Vec::new();
        for &m in extra_mods {
            mods.push(self.modifier_keycode(m)?);
        }
        if plan.shift && !extra_mods.contains(&KS_SHIFT_L) {
            mods.push(self.modifier_keycode(KS_SHIFT_L)?);
        }
        if plan.level3 {
            let kc = self
                .modifier_keycode(KS_ISO_LEVEL3_SHIFT)
                .or_else(|_| self.modifier_keycode(KS_MODE_SWITCH))?;
            mods.push(kc);
        }
        for &m in &mods {
            self.fake(true, m)?;
        }
        self.fake(true, plan.keycode)?;
        self.fake(false, plan.keycode)?;
        for &m in mods.iter().rev() {
            self.fake(false, m)?;
        }
        self.sync()?;
        thread::sleep(Duration::from_millis(8));
        Ok(())
    }

    /// Eşlemede olmayan keysym'i boş bir tuş koduna geçici olarak atayıp basar.
    fn tap_remapped(&self, keysym: u32, extra_mods: &[u32]) -> Result<()> {
        let kc = self
            .map
            .spare_keycode()
            .ok_or_else(|| anyhow!("Geçici eşleme için boş tuş kodu yok"))?;
        let per = self.map.per as u8;
        let mut syms = vec![0u32; per as usize];
        syms[0] = keysym;
        if per > 1 {
            syms[1] = keysym;
        }
        self.conn.change_keyboard_mapping(1, kc, per, &syms)?;
        self.sync()?;
        thread::sleep(Duration::from_millis(25));
        let res = self.tap(KeyPlan { keycode: kc, shift: false, level3: false }, extra_mods);
        thread::sleep(Duration::from_millis(15));
        self.conn.change_keyboard_mapping(1, kc, per, &vec![0u32; per as usize])?;
        self.sync()?;
        thread::sleep(Duration::from_millis(15));
        res
    }

    fn tap_keysym_candidates(&self, cands: &[u32], extra_mods: &[u32]) -> Result<()> {
        for &ks in cands {
            if let Some(plan) = self.map.find(ks) {
                return self.tap(plan, extra_mods);
            }
        }
        let last = *cands.last().ok_or_else(|| anyhow!("keysym adayı yok"))?;
        self.tap_remapped(last, extra_mods)
    }

    pub fn type_text(&self, text: &str) -> Result<()> {
        for ch in text.chars() {
            self.tap_keysym_candidates(&char_keysym_candidates(ch), &[])?;
        }
        Ok(())
    }

    pub fn send_combo(&self, combo: &str) -> Result<()> {
        let mut mods: Vec<u32> = Vec::new();
        let mut keys: Vec<(Vec<u32>, bool)> = Vec::new();
        for part in combo.split('+').map(str::trim).filter(|s| !s.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "shift" => mods.push(KS_SHIFT_L),
                "ctrl" | "control" => mods.push(KS_CONTROL_L),
                "alt" | "mod1" => mods.push(KS_ALT_L),
                "super" | "win" | "logo" | "mod4" => mods.push(KS_SUPER_L),
                _ => {
                    let mut chars = part.chars();
                    let single = match (chars.next(), chars.next()) {
                        (Some(c), None) => Some(c),
                        _ => None,
                    };
                    // Büyük harf kısayolu = Shift + küçük harf (ctrl+S gibi)
                    let (cands, shift) = match single {
                        Some(c) if c.is_uppercase() && c.is_ascii() => {
                            (char_keysym_candidates(c.to_ascii_lowercase()), true)
                        }
                        Some(c) => (char_keysym_candidates(c), false),
                        None => (
                            vec![name_keysym(part).ok_or_else(|| anyhow!("Bilinmeyen tuş: `{}`", part))?],
                            false,
                        ),
                    };
                    keys.push((cands, shift));
                }
            }
        }
        if keys.is_empty() {
            return Err(anyhow!("Kombinasyonda tuş yok: `{}`", combo));
        }
        for (cands, shift) in keys {
            let mut m = mods.clone();
            if shift && !m.contains(&KS_SHIFT_L) {
                m.push(KS_SHIFT_L);
            }
            self.tap_keysym_candidates(&cands, &m)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> KeyMap {
        // keycode 9: a/A ; 10: s/S ; 11: ş/Ş + level3 'x'/'X' ; 12: Control_L ; 13..: boş
        let per = 6;
        let mut syms = vec![0u32; per * 6];
        let set = |s: &mut Vec<u32>, row: usize, v: [u32; 6]| s[row * per..row * per + per].copy_from_slice(&v);
        set(&mut syms, 0, [0x61, 0x41, 0x61, 0x41, 0, 0]);
        set(&mut syms, 1, [0x73, 0x53, 0x73, 0x53, 0, 0]);
        set(&mut syms, 2, [0x1ba, 0x1aa, 0x1ba, 0x1aa, 0x78, 0x58]);
        set(&mut syms, 3, [KS_CONTROL_L, 0, KS_CONTROL_L, 0, 0, 0]);
        KeyMap { min_keycode: 9, per, syms }
    }

    #[test]
    fn finds_plain_shift_and_level3() {
        let m = map();
        assert_eq!(m.find(0x61), Some(KeyPlan { keycode: 9, shift: false, level3: false }));
        assert_eq!(m.find(0x53), Some(KeyPlan { keycode: 10, shift: true, level3: false }));
        assert_eq!(m.find(0x58), Some(KeyPlan { keycode: 11, shift: true, level3: true }));
        assert_eq!(m.find(0x1ba).unwrap().keycode, 11);
        assert_eq!(m.plain_keycode(KS_CONTROL_L), Some(12));
        assert_eq!(m.spare_keycode(), Some(14));
        assert_eq!(m.find(0x1234), None);
    }

    #[test]
    fn turkish_candidates() {
        assert_eq!(char_keysym_candidates('ş')[0], 0x1ba);
        assert_eq!(char_keysym_candidates('ç')[0], 0xe7);
        assert!(char_keysym_candidates('İ').contains(&0x2a9));
        assert_eq!(*char_keysym_candidates('😀').last().unwrap(), 0x0100_0000 | 0x1f600);
    }

    #[test]
    fn named_keys() {
        assert_eq!(name_keysym("Return"), Some(0xff0d));
        assert_eq!(name_keysym("f5"), Some(0xffc2));
        assert_eq!(name_keysym("s"), Some(0x73));
        assert_eq!(name_keysym("bogus"), None);
    }
}
