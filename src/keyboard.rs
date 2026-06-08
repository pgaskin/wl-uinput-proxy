//! uinput-backed implementation of `zwp_virtual_keyboard_manager_v1`.
//! 
//! If the client keymap is available and can map the key to a keysym which can
//! be mapped back to evdev codes for the compositor's keymap, if also
//! available, those evdev codes are used. Otherwise, evdev codes are used as-is
//! and modifiers are simulated as modifer keypresses.

use std::{
    collections::HashMap, os::fd::{AsFd, OwnedFd}, panic, rc::Rc, sync::atomic::{AtomicU64, Ordering}
};

use wl_proxy::{
    object::{Object, ObjectCoreApi},
    protocols::{
        wayland::wl_seat::WlSeat,
        virtual_keyboard_unstable_v1::{
            zwp_virtual_keyboard_manager_v1::{
                ZwpVirtualKeyboardManagerV1, ZwpVirtualKeyboardManagerV1Handler,
            },
            zwp_virtual_keyboard_v1::{ZwpVirtualKeyboardV1, ZwpVirtualKeyboardV1Handler},
        },
    },
};

use xkbcommon_rs::{
    Keymap, State,
    xkb_state::{KeyDirection, StateComponent},
};

use crate::{
    seat::{SharedKeymap, compile_keymap},
    uinput::{
        Device, UinputBuilder, UinputDevice,
        EV_KEY, KEY_CAPSLOCK, KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_NUMLOCK, KEY_RIGHTALT,
    },
};

/// `wl_keyboard.keymap_format.xkb_v1`.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// Fallback mapping from modifier bits to evdev key codes. Matches most XKB maps.
const MOD_MAP: &[(u32, u16)] = &[
    (1 << 0, KEY_LEFTSHIFT),
    (1 << 1, KEY_CAPSLOCK),
    (1 << 2, KEY_LEFTCTRL),
    (1 << 3, KEY_LEFTALT),
    (1 << 4, KEY_NUMLOCK),
    (1 << 6, KEY_LEFTMETA),
    (1 << 7, KEY_RIGHTALT),
];

/// Modifiers that change the keysym level, and so must be set to exactly what a
/// translated key needs (unlike the passthrough modifiers below, which are held
/// across keys purely so keybindings see them).
const LEVEL_MODS: &[u16] = &[KEY_LEFTSHIFT, KEY_RIGHTALT];

/// Level modifier keys which only affect the xkb state and change the value of keysyms.
pub fn is_modifier_keysym(sym: u32) -> bool {
    matches!(sym,
        0xffe1..=0xffee // Shift_L..Hyper_R
        | 0xff7e        // Mode_switch (ISO_Group_Shift / AltGr alias)
        | 0xff7f        // Num_Lock
        | 0xfe03        // ISO_Level3_Shift
        | 0xfe11        // ISO_Level5_Shift
    )
}

/// Modifier keys to pass through so keybindings work correctly.
///
/// Shift is included even though it's a level modifier since bindings and
/// modified clicks (e.g. Alt+Shift+Click) need it to be actually held, so it's
/// held here and temporarily released per-key in [`Keyboard::translate_key`]
/// when a translated key needs a different shift state than the one held for
/// the binding.
pub fn passthrough_mod_keys(state: &State) -> Vec<u16> {
    let mut keys = Vec::new();
    let active = |name| {
        state
            .mod_name_is_active(name, StateComponent::MODS_EFFECTIVE)
            .unwrap_or(false)
    };
    if active("Shift") {
        keys.push(KEY_LEFTSHIFT);
    }
    if active("Control") {
        keys.push(KEY_LEFTCTRL);
    }
    if active("Mod1") {
        keys.push(KEY_LEFTALT);
    }
    if active("Mod4") {
        keys.push(KEY_LEFTMETA);
    }
    keys
}

pub struct ReverseMap {
    syms: HashMap<u32, (u16, Vec<u16>)>,
}

impl ReverseMap {
    pub fn build(keymap: &Keymap) -> Self {
        let mut syms = HashMap::new();
        let mut state = State::new(keymap.clone());

        let mod_mask = |name| keymap.mod_get_index(name).map_or(0u32, |i| 1 << i);
        let shift = mod_mask("Shift");
        let level3 = mod_mask("Mod5"); // AltGr

        // try simpler combinations first
        let mut combos: Vec<(u32, Vec<u16>)> = vec![(0, vec![])];
        if shift != 0 {
            combos.push((shift, vec![KEY_LEFTSHIFT]));
        }
        if level3 != 0 {
            combos.push((level3, vec![KEY_RIGHTALT]));
            if shift != 0 {
                combos.push((shift | level3, vec![KEY_LEFTSHIFT, KEY_RIGHTALT]));
            }
        }

        // brute-force the keysyms
        let min = u32::from(keymap.min_keycode());
        let max = u32::from(keymap.max_keycode());
        for (mask, modkeys) in &combos {
            state.update_mask(*mask, 0, 0, 0, 0, 0);
            for kc in min..=max {
                let Some(sym) = state.key_get_one_sym(kc) else {
                    continue;
                };
                let evdev = kc.saturating_sub(8) as u16;
                syms.entry(sym.raw()).or_insert_with(|| (evdev, modkeys.clone()));
            }
        }
        // reset the keymap state
        state.update_mask(0, 0, 0, 0, 0, 0);

        Self { syms }
    }

    pub fn lookup(&self, sym: u32) -> Option<(u16, &[u16])> {
        match panic::catch_unwind(|| self.syms.get(&sym).map(|(kc, mods)| (*kc, mods.as_slice()))) {
            Ok(Some(v)) => Some(v),
            Ok(None) => {
                eprintln!("wl-uinput-proxy: failed to look up keysym {sym:#010x}");
                None
            }
            Err(e) => {
                let msg = e.downcast_ref::<&str>().copied()
                    .or_else(|| e.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("unknown panic");
                eprintln!("wl-uinput-proxy: panic looking up keysym {sym:#010x}: {msg}");
                None
            }
        }
    }
}

pub struct KeyboardManager {
    keymap: SharedKeymap,
}

impl KeyboardManager {
    pub fn new(keymap: SharedKeymap) -> Self {
        Self { keymap }
    }
}

static DEVICE_IDX: AtomicU64 = AtomicU64::new(0);

impl ZwpVirtualKeyboardManagerV1Handler for KeyboardManager {
    fn handle_create_virtual_keyboard(
        &mut self,
        _slf: &Rc<ZwpVirtualKeyboardManagerV1>,
        _seat: &Rc<WlSeat>,
        id: &Rc<ZwpVirtualKeyboardV1>,
    ) {
        id.set_forward_to_server(false);
        let name = format!("wl-uinput-proxy virtual keyboard {}", DEVICE_IDX.fetch_add(1, Ordering::Relaxed));
        let dev = match create_keyboard_device(&name) {
            Ok(dev) => Some(dev),
            Err(e) => {
                eprintln!("wl-uinput-proxy: failed to create uinput keyboard device: {e}");
                None
            }
        };
        id.set_handler(Keyboard::new(Device::spawn(dev), self.keymap.clone()));
    }
}

struct Pressed {
    out_kc: u16,
    /// Level mods we pressed on key-down and release on key-up.
    pressed_mods: Vec<u16>,
    /// Passthrough level mods we temporarily released on key-down and to press
    /// again on key-up.
    released_mods: Vec<u16>,
}

pub struct Keyboard {
    dev: Device,
    keymap: SharedKeymap,
    reverse: Option<Rc<ReverseMap>>,
    client: Option<State>,
    passthrough_held: Vec<u16>,
    pressed: HashMap<u32, Pressed>,
    raw_held_mods: u32,
}

fn create_keyboard_device(name: &str) -> std::io::Result<UinputDevice> {
    let b = UinputBuilder::new()?;
    b.enable_ev(EV_KEY)?;
    // every key and modifier, but below BTN_* so it gets classified as a
    // keyboard by the kernel
    for code in 1..=255u16 {
        b.enable_key(code)?;
    }
    b.build(name)
}

impl Keyboard {
    fn new(dev: Device, keymap: SharedKeymap) -> Self {
        Self {
            dev,
            keymap,
            reverse: None,
            client: None,
            passthrough_held: Vec::new(),
            pressed: HashMap::new(),
            raw_held_mods: 0,
        }
    }

    fn ensure_reverse(&mut self) {
        if self.reverse.is_none()
            && let Some(km) = self.keymap.borrow().as_ref()
        {
            self.reverse = Some(Rc::new(ReverseMap::build(km)));
        }
    }

    fn translating(&self) -> bool {
        self.reverse.is_some() && self.client.is_some()
    }

    fn sync_passthrough(&mut self) {
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let desired = passthrough_mod_keys(client);
        let mut changed = false;
        for &m in &desired {
            if !self.passthrough_held.contains(&m) {
                self.dev.emit(EV_KEY, m, 1);
                self.passthrough_held.push(m);
                changed = true;
            }
        }
        let dev = &self.dev;
        self.passthrough_held.retain(|&m| {
            let keep = desired.contains(&m);
            if !keep {
                dev.emit(EV_KEY, m, 0);
                changed = true;
            }
            keep
        });
        if changed {
            self.dev.sync();
        }
    }

    fn release_raw_mods(&mut self) {
        if self.raw_held_mods == 0 {
            return;
        }
        for &(bit, code) in MOD_MAP {
            if self.raw_held_mods & bit != 0 {
                self.dev.emit(EV_KEY, code, 0);
            }
        }
        self.raw_held_mods = 0;
        self.dev.sync();
    }

    fn translate_key(&mut self, key: u32, press: bool) {
        let xkb_kc = key + 8;
        if press {
            if self.pressed.contains_key(&key) {
                // ignore a repeated press of an pressed key since re-emitting would
                // leak the previous release and leave the key stuck on release
                return;
            }
            let sym = self
                .client
                .as_ref()
                .unwrap()
                .key_get_one_sym(xkb_kc)
                .map_or(0, |s| s.raw());
            self.client.as_mut().unwrap().update_key(xkb_kc, KeyDirection::Down);

            self.sync_passthrough();
            if is_modifier_keysym(sym) {
                return;
            }

            let (out_kc, needed) = match self.reverse.as_ref().unwrap().lookup(sym) {
                Some((kc, mods)) => (kc, mods.to_vec()),
                None => (key as u16, Vec::new()), // fall back to the raw evdev code
            };
            let mut pressed_mods = Vec::new();
            let mut released_mods = Vec::new();
            for &m in LEVEL_MODS {
                match (needed.contains(&m), self.passthrough_held.contains(&m)) {
                    (true, false) => {
                        self.dev.emit(EV_KEY, m, 1);
                        pressed_mods.push(m);
                    }
                    (false, true) => {
                        self.dev.emit(EV_KEY, m, 0);
                        released_mods.push(m);
                    }
                    _ => {} // already in the right state
                }
            }
            self.dev.emit(EV_KEY, out_kc, 1);
            self.dev.sync();
            self.pressed.insert(key, Pressed { out_kc, pressed_mods, released_mods });
        } else {
            self.client.as_mut().unwrap().update_key(xkb_kc, KeyDirection::Up);
            if let Some(p) = self.pressed.remove(&key) {
                self.dev.emit(EV_KEY, p.out_kc, 0);
                for &m in p.pressed_mods.iter().rev() {
                    self.dev.emit(EV_KEY, m, 0);
                }
                for &m in &p.released_mods {
                    self.dev.emit(EV_KEY, m, 1); // restore the binding's level mods
                }
                self.dev.sync();
            }
            self.sync_passthrough();
        }
    }
}

impl ZwpVirtualKeyboardV1Handler for Keyboard {
    fn handle_keymap(
        &mut self,
        _slf: &Rc<ZwpVirtualKeyboardV1>,
        format: u32,
        fd: &Rc<OwnedFd>,
        size: u32,
    ) {
        if format != KEYMAP_FORMAT_XKB_V1 {
            return;
        }
        if let Some(km) = compile_keymap(fd.as_fd(), size as usize) {
            self.client = Some(State::new(km));
        }
    }

    fn handle_key(&mut self, _slf: &Rc<ZwpVirtualKeyboardV1>, _time: u32, key: u32, state: u32) {
        let press = state == 1;
        self.ensure_reverse();
        if self.translating() {
            self.release_raw_mods(); // if any were pressed while we got the keymap
            self.translate_key(key, press);
        } else {
            self.dev.emit(EV_KEY, key as u16, press as i32);
            self.dev.sync();
        }
    }

    fn handle_modifiers(
        &mut self,
        _slf: &Rc<ZwpVirtualKeyboardV1>,
        mods_depressed: u32,
        mods_latched: u32,
        mods_locked: u32,
        group: u32,
    ) {
        self.ensure_reverse();
        if self.translating() {
            self.release_raw_mods(); // if any were pressed while we got the keymap
            self.client.as_mut().unwrap().update_mask(
                mods_depressed,
                mods_latched,
                mods_locked,
                0,
                0,
                group as usize,
            );
            self.sync_passthrough();
            return;
        }

        // determine modifiers from the bitmask (fallback)
        let desired = mods_depressed | mods_latched | mods_locked;
        let mut changed = false;
        for &(bit, code) in MOD_MAP {
            let want = desired & bit != 0;
            let have = self.raw_held_mods & bit != 0;
            if want && !have {
                self.dev.emit(EV_KEY, code, 1);
                self.raw_held_mods |= bit;
                changed = true;
            } else if !want && have {
                self.dev.emit(EV_KEY, code, 0);
                self.raw_held_mods &= !bit;
                changed = true;
            }
        }
        if changed {
            self.dev.sync();
        }
    }

    fn handle_destroy(&mut self, slf: &Rc<ZwpVirtualKeyboardV1>) {
        // release keys so they don't get stuck
        for (_key, p) in self.pressed.drain() {
            self.dev.emit(EV_KEY, p.out_kc, 0);
            for &m in p.pressed_mods.iter().rev() {
                self.dev.emit(EV_KEY, m, 0);
            }
        }
        for &m in &self.passthrough_held {
            self.dev.emit(EV_KEY, m, 0);
        }
        self.passthrough_held.clear();
        for &(bit, code) in MOD_MAP {
            if self.raw_held_mods & bit != 0 {
                self.dev.emit(EV_KEY, code, 0);
            }
        }
        self.raw_held_mods = 0;
        self.dev.sync();

        slf.unset_handler();
        slf.delete_id();
    }
}
