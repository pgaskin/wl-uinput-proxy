//! Wayland proxy implementing `zwlr_virtual_pointer_manager_v1` and `zwp_virtual_keyboard_manager_v1` using uinput.

mod keyboard;
mod pointer;
mod seat;
mod uinput;

use std::{cell::RefCell, process::Command, rc::Rc};

use wl_proxy::{
    baseline::Baseline,
    global_mapper::GlobalMapper,
    object::{ConcreteObject, Object, ObjectCoreApi, ObjectRcUtils},
    protocols::{
        ObjectInterface,
        virtual_keyboard_unstable_v1::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        wayland::{
            wl_callback::{WlCallback, WlCallbackHandler},
            wl_display::{WlDisplay, WlDisplayHandler},
            wl_registry::{WlRegistry, WlRegistryHandler},
        },
        wlr_virtual_pointer_unstable_v1::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    },
    simple::{SimpleCommandExt, SimpleProxy},
};

use crate::{
    keyboard::KeyboardManager,
    pointer::PointerManager,
    seat::{ServerRegistry, SharedKeymap},
};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(program) = args.next() else {
        println!("usage: wl-uinput-proxy <cmd> [args...]");
        std::process::exit(2);
    };
    let program_args: Vec<_> = args.collect();

    let proxy = match SimpleProxy::new(Baseline::ALL_OF_THEM) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("wl-uinput-proxy: failed to create proxy: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = Command::new(&program)
        .args(&program_args)
        .with_wayland_display(proxy.display())
        .spawn_and_forward_exit_code()
    {
        eprintln!("wl-uinput-proxy: failed to spawn {:?}: {e}", program);
        std::process::exit(1);
    }

    let err = proxy.run(Display::default);
    eprintln!("wl-uinput-proxy: proxy terminated: {err}");
    std::process::exit(1);
}

#[derive(Default)]
struct Display {
    init: bool,
    keymap: SharedKeymap,
}

impl WlDisplayHandler for Display {
    fn handle_get_registry(&mut self, slf: &Rc<WlDisplay>, registry: &Rc<WlRegistry>) {
        if !self.init {
            self.init = true;
            let server_registry = slf.create_child::<WlRegistry>();
            server_registry.set_forward_to_client(false);
            server_registry.set_handler(ServerRegistry::new(self.keymap.clone()));
            slf.send_get_registry(&server_registry);
        }

        let mapper = Rc::new(RefCell::new(GlobalMapper::default()));
        registry.set_handler(Registry::new(mapper.clone(), self.keymap.clone()));
        slf.send_get_registry(registry);

        // our globals must only be added after the existing globals have been
        // forwarded since some clients depend on the order (e.g., wayvnc
        // silently fails if the wl_seat and outputs aren't first)
        let announce = slf.create_child::<WlCallback>();
        announce.set_forward_to_client(false);
        announce.set_handler(AddSynthetic {
            registry: registry.clone(),
            mapper,
        });
        slf.send_sync(&announce); // run the callback after the wl_registry is done
    }
}

struct AddSynthetic {
    registry: Rc<WlRegistry>,
    mapper: Rc<RefCell<GlobalMapper>>,
}

impl WlCallbackHandler for AddSynthetic {
    fn handle_done(&mut self, slf: &Rc<WlCallback>, _callback_data: u32) {
        let mut mapper = self.mapper.borrow_mut();
        mapper.add_synthetic_global(
            &self.registry,
            ObjectInterface::ZwlrVirtualPointerManagerV1,
            ZwlrVirtualPointerManagerV1::XML_VERSION,
        );
        mapper.add_synthetic_global(
            &self.registry,
            ObjectInterface::ZwpVirtualKeyboardManagerV1,
            ZwpVirtualKeyboardManagerV1::XML_VERSION,
        );
        slf.unset_handler();
    }
}

struct Registry {
    mapper: Rc<RefCell<GlobalMapper>>,
    keymap: SharedKeymap,
}

impl Registry {
    fn new(mapper: Rc<RefCell<GlobalMapper>>, keymap: SharedKeymap) -> Self {
        Self { mapper, keymap }
    }
}

impl WlRegistryHandler for Registry {
    fn handle_global(
        &mut self,
        slf: &Rc<WlRegistry>,
        name: u32,
        interface: ObjectInterface,
        version: u32,
    ) {
        let mut mapper = self.mapper.borrow_mut();
        match interface {
            ObjectInterface::ZwlrVirtualPointerManagerV1 => {
                mapper.ignore_global(name);
            }
            ObjectInterface::ZwpVirtualKeyboardManagerV1 => {
                mapper.ignore_global(name);
            }
            _ => {
                mapper.forward_global(slf, name, interface, version);
            }
        }
    }

    fn handle_global_remove(&mut self, slf: &Rc<WlRegistry>, name: u32) {
        self.mapper.borrow_mut().forward_global_remove(slf, name);
    }

    fn handle_bind(&mut self, slf: &Rc<WlRegistry>, name: u32, id: Rc<dyn Object>) {
        match id.interface() {
            ZwlrVirtualPointerManagerV1::INTERFACE => {
                id.set_forward_to_server(false);
                id.downcast::<ZwlrVirtualPointerManagerV1>()
                    .set_handler(PointerManager);
                return;
            }
            ZwpVirtualKeyboardManagerV1::INTERFACE => {
                id.set_forward_to_server(false);
                id.downcast::<ZwpVirtualKeyboardManagerV1>()
                    .set_handler(KeyboardManager::new(self.keymap.clone()));
                return;
            }
            _ => {}
        }
        self.mapper.borrow_mut().forward_bind(slf, name, &id);
    }
}
