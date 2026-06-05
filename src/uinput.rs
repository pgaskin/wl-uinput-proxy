//! Simple uinput wrapper.

use std::{
    io, mem,
    os::fd::{AsRawFd, OwnedFd},
    sync::mpsc::{self, Sender},
    thread,
    time::Duration,
};

use nix::{
    fcntl::{OFlag, open},
    libc::{c_char, input_absinfo, input_event, input_id, timeval, uinput_abs_setup, uinput_setup},
    sys::stat::Mode,
    unistd::write,
};
use uinput_ioctls::{
    ui_abs_setup, ui_dev_create, ui_dev_destroy, ui_dev_setup, ui_set_absbit, ui_set_evbit,
    ui_set_keybit, ui_set_relbit,
};

// linux/input-event-codes.h
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const SYN_REPORT: u16 = 0;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_WHEEL_HI_RES: u16 = 0x0b;
pub const REL_HWHEEL_HI_RES: u16 = 0x0c;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_TASK: u16 = 0x117;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_NUMLOCK: u16 = 69;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_LEFTMETA: u16 = 125;

// linux/input.h
const BUS_VIRTUAL: u16 = 0x06;

// raw device input range (this will get scaled when actually used)
pub const ABS_MAX_VAL: i32 = 65535;

pub struct UinputBuilder {
    fd: OwnedFd,
    /// (code, min, max)
    abs: Vec<(u16, i32, i32)>,
}

impl UinputBuilder {
    pub fn new() -> io::Result<Self> {
        let fd = open(
            "/dev/uinput",
            OFlag::O_WRONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self {
            fd,
            abs: Vec::new(),
        })
    }

    pub fn enable_ev(&self, ev: u16) -> io::Result<()> {
        unsafe { ui_set_evbit(self.fd.as_raw_fd(), ev as _) }?;
        Ok(())
    }

    pub fn enable_key(&self, code: u16) -> io::Result<()> {
        unsafe { ui_set_keybit(self.fd.as_raw_fd(), code as _) }?;
        Ok(())
    }

    pub fn enable_rel(&self, code: u16) -> io::Result<()> {
        unsafe { ui_set_relbit(self.fd.as_raw_fd(), code as _) }?;
        Ok(())
    }

    pub fn enable_abs(&mut self, code: u16, min: i32, max: i32) -> io::Result<()> {
        unsafe { ui_set_absbit(self.fd.as_raw_fd(), code as _) }?;
        self.abs.push((code, min, max));
        Ok(())
    }

    pub fn build(self, name: &str) -> io::Result<UinputDevice> {
        let fd = self.fd.as_raw_fd();

        for &(code, min, max) in &self.abs {
            let abs = uinput_abs_setup {
                code,
                absinfo: input_absinfo {
                    value: 0,
                    minimum: min,
                    maximum: max,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
            };
            unsafe { ui_abs_setup(fd, &abs) }?;
        }

        let mut setup: uinput_setup = unsafe { mem::zeroed() };
        setup.id = input_id {
            bustype: BUS_VIRTUAL,
            vendor: 0x1234,
            product: 0x5678,
            version: 1,
        };
        let last = setup.name.len() - 1;
        for (dst, &src) in setup.name[..last].iter_mut().zip(name.as_bytes()) {
            *dst = src as c_char;
        }

        unsafe { ui_dev_setup(fd, &setup) }?;
        unsafe { ui_dev_create(fd) }?;

        Ok(UinputDevice { fd: self.fd })
    }
}

pub struct UinputDevice {
    fd: OwnedFd,
}

impl UinputDevice {
    pub fn emit(&self, ty: u16, code: u16, value: i32) -> io::Result<()> {
        let ev = input_event {
            time: timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_: ty,
            code,
            value,
        };
        let size = mem::size_of::<input_event>();
        let bytes = unsafe { std::slice::from_raw_parts((&ev as *const input_event).cast::<u8>(), size) };
        let written = write(&self.fd, bytes)?;
        if written != size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short write ({written}/{size}"),
            ));
        }
        Ok(())
    }

    pub fn sync(&self) -> io::Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }
}

impl Drop for UinputDevice {
    fn drop(&mut self) {
        let _ = unsafe { ui_dev_destroy(self.fd.as_raw_fd()) };
    }
}

/// Time to wait for the compositor to discover a device before emitting events.
pub const CREATE_DELAY: Duration = Duration::from_millis(150);

/// A wrapper around an optional uinput device which emits eventa on a dedicated
/// thread after `CREATE_DELAY` if the device is not `None`.
pub struct Device {
    tx: Option<Sender<Cmd>>,
}

enum Cmd {
    Emit(u16, u16, i32),
    Sync,
}

impl Device {
    pub fn spawn(dev: Option<UinputDevice>) -> Self {
        let Some(dev) = dev else {
            return Self { tx: None };
        };
        let (tx, rx) = mpsc::channel::<Cmd>();
        thread::spawn(move || {
            thread::sleep(CREATE_DELAY);
            for cmd in rx {
                let r = match cmd {
                    Cmd::Emit(ty, code, value) => dev.emit(ty, code, value),
                    Cmd::Sync => dev.sync(),
                };
                if let Err(e) = r {
                    eprintln!("wl-uinput-proxy: failed to emit uinput event: {e}");
                }
            }
            // channel closed, queue drained, dev will be destroyed
        });
        Self { tx: Some(tx) }
    }

    pub fn emit(&self, ty: u16, code: u16, value: i32) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Emit(ty, code, value));
        }
    }

    pub fn sync(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Sync);
        }
    }
}
