# wl-uinput-proxy

Proxies an existing Wayland connection, implementing `zwp_virtual_keyboard_manager_v1` and `zwlr_virtual_pointer_manager_v1` using uinput.

This is intended for making remote desktop implementions like wayvnc and RealVNC work correctly with compositors with broken/incomplete virtual input implementations, especially Smithay-based ones.

It has been tested to work on niri 26.04, with wayvnc 0.9.1 and RealVNC 7.17.0. With it, scrolling, compositor hotkeys, and keymaps work correctly.

There are a few limitations:

- You can't type characters which can't be typed using the current keymap.
- Initialization is slightly racy since the compositor may take some time to pick up the uinput devices. Also, getting the keymap is async.
- The keymap logic isn't perfect, and may fail to map complex key combinations.
- You need access to `/dev/uinput`.
- Virtual devices cannot be mapped to a specific seat, output, or compositor instance.

To use this with wayvnc, just prefix wayvnc with `wl-uinput-proxy`.

To use this with RealVNC, see the instructions in [vncagent-wlr-fixes](https://github.com/pgaskin/vncagent-wlr-fixes).

To use this with other applications, prefix them with `wl-uinput-proxy`. Note that only protocols supported by [wl-proxy](https://github.com/mahkoh/wl-proxy/blob/master/wl-proxy/Cargo.toml) will be proxied.
