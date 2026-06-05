//! uinput-backed implementation of `zwlr_virtual_pointer_manager_v1`.
//!
//! - position (`motion`, `motion_absolute`) -> `EV_ABS` `ABS_X`/`ABS_Y`
//! - scrolling (`axis`, `axis_discrete`) -> `EV_REL` low/high-res vertical/horizontal wheels
//! - buttons (`button`) -> `EV_KEY` `BTN_*`

use std::{
    thread,
    time::Duration,
    sync::atomic::{AtomicU64, Ordering},
};

use std::rc::Rc;

use wl_proxy::{
    fixed::Fixed,
    object::{Object, ObjectCoreApi},
    protocols::{
        wayland::{
            wl_output::WlOutput,
            wl_pointer::{WlPointerAxis, WlPointerAxisSource, WlPointerButtonState},
            wl_seat::WlSeat,
        },
        wlr_virtual_pointer_unstable_v1::{
            zwlr_virtual_pointer_manager_v1::{
                ZwlrVirtualPointerManagerV1, ZwlrVirtualPointerManagerV1Handler,
            },
            zwlr_virtual_pointer_v1::{ZwlrVirtualPointerV1, ZwlrVirtualPointerV1Handler},
        },
    },
};

use crate::uinput::{
    UinputBuilder, UinputDevice,
    ABS_MAX_VAL, ABS_X, ABS_Y, BTN_LEFT, BTN_TASK, Device, EV_ABS, EV_KEY, EV_REL, REL_HWHEEL,
    REL_HWHEEL_HI_RES, REL_WHEEL, REL_WHEEL_HI_RES,
};

/// Number of wayland axis units per wheel notch.
const WHEEL_NOTCH: f64 = 15.0; // same as wlroots/libinput

pub struct PointerManager;

static DEVICE_IDX: AtomicU64 = AtomicU64::new(0);

impl ZwlrVirtualPointerManagerV1Handler for PointerManager {
    fn handle_create_virtual_pointer(
        &mut self,
        _slf: &Rc<ZwlrVirtualPointerManagerV1>,
        _seat: Option<&Rc<WlSeat>>,
        id: &Rc<ZwlrVirtualPointerV1>,
    ) {
        id.set_forward_to_server(false);
        let name = format!("wl-uinput-proxy virtual pointer {}", DEVICE_IDX.fetch_add(1, Ordering::Relaxed));
        let dev = match create_pointer_device(&name) {
            Ok(dev) => Some(dev),
            Err(e) => {
                eprintln!("wl-uinput-proxy: failed to create uinput pointer device: {e}");
                None
            }
        };
        id.set_handler(Pointer::new(Device(dev)));
    }

    fn handle_create_virtual_pointer_with_output(
        &mut self,
        slf: &Rc<ZwlrVirtualPointerManagerV1>,
        seat: Option<&Rc<WlSeat>>,
        _output: Option<&Rc<WlOutput>>,
        id: &Rc<ZwlrVirtualPointerV1>,
    ) {
        eprintln!("wl-uinput-proxy: ignoring output for virtual pointer");
        self.handle_create_virtual_pointer(slf, seat, id);
    }

    fn handle_destroy(&mut self, slf: &Rc<ZwlrVirtualPointerManagerV1>) {
        slf.unset_handler();
        slf.delete_id();
    }
}

pub struct Pointer {
    dev: Device,
    abs_x: i32, // current device pos
    abs_y: i32, // current device pos
    x_extent: u32, // from last motion_absolute
    y_extent: u32, // from last motion_absolute
    v_cont: f64, // acc
    h_cont: f64, // acc
    v_disc: i32, // acc
    h_disc: i32, // acc
    has_v_disc: bool,
    has_h_disc: bool,
}

fn create_pointer_device(name: &str) -> std::io::Result<UinputDevice> {
    let mut b: UinputBuilder = UinputBuilder::new()?;
    b.enable_ev(EV_KEY)?;
    for btn in BTN_LEFT..=BTN_TASK {
        b.enable_key(btn)?;
    }
    b.enable_ev(EV_REL)?;
    for rel in [REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
        b.enable_rel(rel)?;
    }
    b.enable_ev(EV_ABS)?;
    b.enable_abs(ABS_X, 0, ABS_MAX_VAL)?;
    b.enable_abs(ABS_Y, 0, ABS_MAX_VAL)?;
    let dev = b.build(name)?;
    // still racy, but at least give it some time to get discovered by the
    // compositor before sending events
    thread::sleep(Duration::from_millis(150));
    Ok(dev)
}

impl Pointer {
    fn new(dev: Device) -> Self {
        Self {
            dev,
            abs_x: ABS_MAX_VAL / 2,
            abs_y: ABS_MAX_VAL / 2,
            x_extent: 0,
            y_extent: 0,
            v_cont: 0.0,
            h_cont: 0.0,
            v_disc: 0,
            h_disc: 0,
            has_v_disc: false,
            has_h_disc: false,
        }
    }

    fn emit_position(&self) {
        self.dev.emit(EV_ABS, ABS_X, self.abs_x);
        self.dev.emit(EV_ABS, ABS_Y, self.abs_y);
        self.dev.sync();
    }

    fn flush_axis(
        &self,
        cont: f64,
        disc: i32,
        has_disc: bool,
        legacy_code: u16,
        hires_code: u16,
        negate: bool, // wayland uses the opposite direction for the vertical axis
    ) -> bool {
        let sign = if negate { -1 } else { 1 };
        let legacy = if has_disc {
            disc
        } else {
            (cont / WHEEL_NOTCH).round() as i32
        };
        let hires = if cont != 0.0 {
            (cont / WHEEL_NOTCH * 120.0).round() as i32
        } else {
            disc * 120
        };
        let mut emitted = false;
        if legacy != 0 {
            self.dev.emit(EV_REL,legacy_code, sign * legacy);
            emitted = true;
        }
        if hires != 0 {
            self.dev.emit(EV_REL,hires_code, sign * hires);
            emitted = true;
        }
        emitted
    }

    fn reset_axis(&mut self) {
        self.v_cont = 0.0;
        self.h_cont = 0.0;
        self.v_disc = 0;
        self.h_disc = 0;
        self.has_v_disc = false;
        self.has_h_disc = false;
    }
}

impl ZwlrVirtualPointerV1Handler for Pointer {
    fn handle_motion(&mut self, _slf: &Rc<ZwlrVirtualPointerV1>, _time: u32, dx: Fixed, dy: Fixed) {
        let sx = if self.x_extent > 0 {
            ABS_MAX_VAL as f64 / self.x_extent as f64
        } else {
            1.0
        };
        let sy = if self.y_extent > 0 {
            ABS_MAX_VAL as f64 / self.y_extent as f64
        } else {
            1.0
        };
        self.abs_x = (self.abs_x + (dx.to_f64() * sx).round() as i32).clamp(0, ABS_MAX_VAL);
        self.abs_y = (self.abs_y + (dy.to_f64() * sy).round() as i32).clamp(0, ABS_MAX_VAL);
        self.emit_position();
    }

    fn handle_motion_absolute(
        &mut self,
        _slf: &Rc<ZwlrVirtualPointerV1>,
        _time: u32,
        x: u32,
        y: u32,
        x_extent: u32,
        y_extent: u32,
    ) {
        self.x_extent = x_extent;
        self.y_extent = y_extent;
        self.abs_x = scale_abs(x, x_extent);
        self.abs_y = scale_abs(y, y_extent);
        self.emit_position();
    }

    fn handle_button(
        &mut self,
        _slf: &Rc<ZwlrVirtualPointerV1>,
        _time: u32,
        button: u32,
        state: WlPointerButtonState,
    ) {
        if !(u32::from(BTN_LEFT)..=u32::from(BTN_TASK)).contains(&button) {
            eprintln!("wl-uinput-proxy: ignoring unsupported pointer button {button:#x}");
            return;
        }
        let value = (state == WlPointerButtonState::PRESSED) as i32;
        self.dev.emit(EV_KEY, button as u16, value);
        self.dev.sync();
    }

    fn handle_axis(
        &mut self,
        _slf: &Rc<ZwlrVirtualPointerV1>,
        _time: u32,
        axis: WlPointerAxis,
        value: Fixed,
    ) {
        if axis == WlPointerAxis::VERTICAL_SCROLL {
            self.v_cont += value.to_f64();
        } else if axis == WlPointerAxis::HORIZONTAL_SCROLL {
            self.h_cont += value.to_f64();
        }
    }

    fn handle_axis_discrete(
        &mut self,
        _slf: &Rc<ZwlrVirtualPointerV1>,
        _time: u32,
        axis: WlPointerAxis,
        _value: Fixed,
        discrete: i32,
    ) {
        if axis == WlPointerAxis::VERTICAL_SCROLL {
            self.v_disc += discrete;
            self.has_v_disc = true;
        } else if axis == WlPointerAxis::HORIZONTAL_SCROLL {
            self.h_disc += discrete;
            self.has_h_disc = true;
        }
    }

    fn handle_frame(&mut self, _slf: &Rc<ZwlrVirtualPointerV1>) {
        let mut emitted = self.flush_axis(
            self.v_cont,
            self.v_disc,
            self.has_v_disc,
            REL_WHEEL,
            REL_WHEEL_HI_RES,
            true,
        );
        emitted |= self.flush_axis(
            self.h_cont,
            self.h_disc,
            self.has_h_disc,
            REL_HWHEEL,
            REL_HWHEEL_HI_RES,
            false,
        );
        if emitted {
            self.dev.sync();
        }
        self.reset_axis();
    }

    fn handle_axis_source(
        &mut self,
        _slf: &Rc<ZwlrVirtualPointerV1>,
        _axis_source: WlPointerAxisSource,
    ) {
    }

    fn handle_axis_stop(
        &mut self,
        _slf: &Rc<ZwlrVirtualPointerV1>,
        _time: u32,
        _axis: WlPointerAxis,
    ) {
    }

    fn handle_destroy(&mut self, slf: &Rc<ZwlrVirtualPointerV1>) {
        slf.unset_handler();
        slf.delete_id();
    }
}

fn scale_abs(value: u32, extent: u32) -> i32 {
    if extent == 0 {
        return (value as i32).clamp(0, ABS_MAX_VAL);
    }
    ((value as i64 * ABS_MAX_VAL as i64) / extent as i64).clamp(0, ABS_MAX_VAL as i64) as i32
}
