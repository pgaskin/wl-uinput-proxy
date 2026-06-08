//! Simple uinput wrapper.

use crate::wlog;

use std::{
    collections::HashMap,
    io, mem,
    os::fd::{AsRawFd, OwnedFd},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use nix::{
    fcntl::{OFlag, open},
    ioctl_none, ioctl_write_int, ioctl_write_ptr,
    libc::{c_char, input_absinfo, input_event, input_id, timeval, uinput_abs_setup, uinput_setup},
    sys::stat::Mode,
    unistd::write,
};

// linux/uinput.h
ioctl_none!(ui_dev_create, b'U', 1);
ioctl_none!(ui_dev_destroy, b'U', 2);
ioctl_write_ptr!(ui_dev_setup, b'U', 3, nix::libc::uinput_setup);
ioctl_write_ptr!(ui_abs_setup, b'U', 4, nix::libc::uinput_abs_setup);
ioctl_write_int!(ui_set_evbit, b'U', 100);
ioctl_write_int!(ui_set_keybit, b'U', 101);
ioctl_write_int!(ui_set_relbit, b'U', 102);
ioctl_write_int!(ui_set_absbit, b'U', 103);

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
        let bytes =
            unsafe { std::slice::from_raw_parts((&ev as *const input_event).cast::<u8>(), size) };
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

/// Unique index for tracking devices we create.
static DEVICE_IDX: AtomicU64 = AtomicU64::new(0);

/// Time to wait for the compositor to discover a device before emitting events.
pub const CREATE_DELAY: Duration = Duration::from_millis(150);

/// A handle to an optional uinput device. The order of events is preserved
/// across all devices, and events are only emitted after waiting a bit for them
/// to be discovered. If None, all events are ignored immediately.
pub struct Device {
    id: Option<u64>,
}

enum Cmd {
    Add(u64, UinputDevice, Instant),
    Emit(u64, u16, u16, i32),
    Sync(u64),
    Remove(u64),
    Drain(mpsc::SyncSender<()>),
}

impl Device {
    pub fn spawn(dev: Option<UinputDevice>) -> Self {
        let Some(dev) = dev else {
            return Self { id: None };
        };
        let id = DEVICE_IDX.fetch_add(1, Ordering::Relaxed);
        let _ = emitter().send(Cmd::Add(id, dev, Instant::now() + CREATE_DELAY));
        Self { id: Some(id) }
    }

    pub fn emit(&self, ty: u16, code: u16, value: i32) {
        if let Some(id) = self.id {
            let _ = emitter().send(Cmd::Emit(id, ty, code, value));
        }
    }

    pub fn sync(&self) {
        if let Some(id) = self.id {
            let _ = emitter().send(Cmd::Sync(id));
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            // queued after any release events emitted during teardown, so they
            // are applied before the device is destroyed.
            let _ = emitter().send(Cmd::Remove(id));
        }
    }
}

fn emitter() -> &'static Sender<Cmd> {
    static TX: OnceLock<Sender<Cmd>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Cmd>();
        thread::Builder::new()
            .name("uinput-emitter".into())
            .spawn(move || emitter_loop(rx))
            .expect("spawn uinput emitter thread");
        tx
    })
}

fn emitter_loop(rx: Receiver<Cmd>) {
    let mut devices: HashMap<u64, (UinputDevice, Instant)> = HashMap::new();
    for cmd in rx {
        match cmd {
            Cmd::Add(id, dev, start) => {
                devices.insert(id, (dev, start));
            }
            Cmd::Remove(id) => {
                devices.remove(&id); // drops the device, destroying it
            }
            Cmd::Emit(id, ty, code, value) => {
                if let Some((dev, ready_at)) = devices.get(&id) {
                    wait_until(*ready_at);
                    if let Err(e) = dev.emit(ty, code, value) {
                        wlog!("failed to emit uinput event: {e}");
                    }
                }
            }
            Cmd::Sync(id) => {
                if let Some((dev, ready_at)) = devices.get(&id) {
                    wait_until(*ready_at);
                    if let Err(e) = dev.sync() {
                        wlog!("failed to sync uinput device: {e}");
                    }
                }
            }
            Cmd::Drain(done) => {
                let _ = done.send(());
            }
        }
    }
}

pub fn drain() {
    let (tx, rx) = mpsc::sync_channel(0);
    let _ = emitter().send(Cmd::Drain(tx));
    let _ = rx.recv();
}

fn wait_until(t: Instant) {
    let now = Instant::now();
    if t > now {
        // only ever sleeps right after a device is created (startup); in steady
        // state every device is long past its deadline and this is a no-op.
        thread::sleep(t - now);
    }
}
