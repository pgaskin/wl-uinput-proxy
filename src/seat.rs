//! Gets the current keymap from the compositor by getting a private
//! `wl_seat`/`wl_keyboard` to receive the keymap for.

use crate::wlog;

use std::{
    cell::RefCell,
    fs::File,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    os::unix::fs::FileExt,
    rc::Rc,
};

use wl_proxy::{
    object::ObjectCoreApi,
    protocols::{
        ObjectInterface,
        wayland::{
            wl_keyboard::{WlKeyboard, WlKeyboardHandler, WlKeyboardKeymapFormat},
            wl_registry::{WlRegistry, WlRegistryHandler},
            wl_seat::{WlSeat, WlSeatCapability, WlSeatHandler},
        },
    },
};

use xkbcommon_rs::{Context, Keymap, KeymapFormat};

pub type SharedKeymap = Rc<RefCell<Option<Rc<Keymap>>>>;

pub struct ServerRegistry {
    keymap: SharedKeymap,
    bound_seat: bool,
}

impl ServerRegistry {
    pub fn new(keymap: SharedKeymap) -> Self {
        Self {
            keymap,
            bound_seat: false,
        }
    }
}

impl WlRegistryHandler for ServerRegistry {
    fn handle_global(
        &mut self,
        slf: &Rc<WlRegistry>,
        name: u32,
        interface: ObjectInterface,
        _version: u32,
    ) {
        // just get the first seat, it's not like we can choose which on we
        // attach uinput devices to one specifically
        if interface == ObjectInterface::WlSeat && !self.bound_seat {
            self.bound_seat = true;
            let seat = slf.state().create_object::<WlSeat>(_version);
            seat.set_forward_to_client(false);
            slf.send_bind(name, seat.clone());
            seat.set_handler(ServerSeat::new(self.keymap.clone()));
        }
    }
}

pub struct ServerSeat {
    keymap: SharedKeymap,
    got_keyboard: bool,
}

impl ServerSeat {
    fn new(keymap: SharedKeymap) -> Self {
        Self {
            keymap,
            got_keyboard: false,
        }
    }
}

impl WlSeatHandler for ServerSeat {
    fn handle_capabilities(&mut self, slf: &Rc<WlSeat>, capabilities: WlSeatCapability) {
        if !self.got_keyboard && capabilities.0 & WlSeatCapability::KEYBOARD.0 != 0 {
            self.got_keyboard = true;
            let kbd = slf.new_send_get_keyboard();
            kbd.set_forward_to_client(false);
            kbd.set_handler(ServerKeyboard::new(self.keymap.clone()));
        }
    }
}

pub struct ServerKeyboard {
    keymap: SharedKeymap,
}

impl ServerKeyboard {
    fn new(keymap: SharedKeymap) -> Self {
        Self { keymap }
    }
}

impl WlKeyboardHandler for ServerKeyboard {
    fn handle_keymap(
        &mut self,
        _slf: &Rc<WlKeyboard>,
        format: WlKeyboardKeymapFormat,
        fd: &Rc<OwnedFd>,
        size: u32,
    ) {
        if format != WlKeyboardKeymapFormat::XKB_V1 {
            wlog!("compositor keymap has unsupported format {format:?}");
            return;
        }
        match compile_keymap(fd.as_fd(), size as usize) {
            Some(km) => {
                let layout = km.layout_get_name(0).unwrap_or("?");
                wlog!(
                    "got compositor keymap (layout '{layout}', keycodes {}..={})",
                    u32::from(km.min_keycode()),
                    u32::from(km.max_keycode()),
                );
                *self.keymap.borrow_mut() = Some(Rc::new(km));
            }
            None => wlog!("failed to compile compositor keymap"),
        }
    }
}

pub(crate) fn compile_keymap(fd: BorrowedFd, size: usize) -> Option<Keymap> {
    if size == 0 {
        return None;
    }
    let file = File::from(fd.try_clone_to_owned().ok()?); // dup
    let mut buf = vec![0u8; size];
    file.read_exact_at(&mut buf, 0).ok()?; // pread so we don't change the offset
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len()); // null-terminated
    let text = std::str::from_utf8(&buf[..end]).ok()?;
    let ctx = Context::new(0).ok()?;
    Keymap::new_from_string(ctx, text, KeymapFormat::TextV1, 0).ok()
}
