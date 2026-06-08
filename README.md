# wl-uinput-proxy

Proxies an existing Wayland connection, implementing [`zwp_virtual_keyboard_v1`](https://wayland.app/protocols/virtual-keyboard-unstable-v1) and [`zwlr_virtual_pointer_v1`](https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1) using uinput.

This is intended for making remote desktop implementions like wayvnc and RealVNC work correctly with compositors with broken/incomplete virtual input implementations, especially Smithay-based ones.

It has been tested to work on niri 26.04, with wayvnc 0.9.1 and RealVNC 7.17.0. With it, scrolling, compositor hotkeys, and keymaps work correctly.

There are a few limitations:

- You can't type characters which can't be typed using the current keymap.
- Initialization is slightly racy since the compositor may take some time to pick up the uinput devices. Also, getting the keymap is async.
- The keymap logic isn't perfect, and may fail to map complex key combinations. It is also theoretically racy if the compositor switches keymaps rapidly.
- Some applications may misinterpret keyboard shortcuts if they include a level-shifting key (e.g., alt, shift) but the letter (or other non-modifier) requires different level-shifting keys to produce in the active keymap. This is rare, though (see below).
- You need access to `/dev/uinput`.
- Virtual devices cannot be mapped to a specific seat, output, or compositor instance.

To use this with wayvnc, just prefix wayvnc with `wl-uinput-proxy`.

To use this with RealVNC, see the instructions in [vncagent-wlr-fixes](https://github.com/pgaskin/vncagent-wlr-fixes).

To use this with other applications, prefix them with `wl-uinput-proxy`. Note that only protocols supported by [wl-proxy](https://github.com/mahkoh/wl-proxy/blob/master/wl-proxy/Cargo.toml) will be proxied.

## Keymap translation

This was the trickiest part to get right, mostly due to unclear documentation. I'll attempt to explain what I've done here.

The term `keycode` refers to a raw evdev keycode as defined in [`linux/input-event-codes.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/input-event-codes.h).

The term `keysym` refers to an XKB keycode as defined in [`xkbcommon/xkbcommon-keysyms.h`](https://github.com/xkbcommon/libxkbcommon/blob/master/include/xkbcommon/xkbcommon-keysyms.h).

Applications typically work with keysyms rather than raw keycodes, which are translated according to the keymap.

#### The virtual keyboard protocol

The [`zwp_virtual_keyboard_v1`](https://wayland.app/protocols/virtual-keyboard-unstable-v1) protocol has a method for setting the keymap, which required to be called before sending keys. Each virtual keyboard must also bind to a specific `wl_seat`, which sense given my previous explanation of input handling.

However, since we're emulating a physical input device using [`uinput`](https://docs.kernel.org/input/uinput.html), we can't send a keymap since the kernel has no concept of that for evdev devices.

If we just sent raw keycodes ignoring the keymap requested for the virtual keyboard, only keys where both the virtual keyboard and the keymap the compositor selected for us by default would work. Furthermore, we wouldn't even be able to type anything which didn't have a raw keycode. For example, if the virtual keyboard client requested the standard `us` layout and the compositor was configured to use the `dvorak` variant by default, `wtype test` would type `y.oy` if the compositor keymap was `us` with the `dvorak` variant.

This means we need to translate between the two keymaps, while also being careful about modifiers (they need to be sent as key up/down events, and be set at the correct index in the modifier bitmask). And, it also means we're limited to keycodes that are included in the active keymap.

Due the nature of keymaps, it's hard and inefficient do build a reverse keymap perfectly since there could be any number of shifted levels (see below).

For modifiers, we can just pass through non-level-shifting ones like ctrl and super after translating the keycodes, but level-shifting ones are more complicated. Keyboard shortcuts may depend on the modifiers, but so may the letter itself. Consider the scenario where one keymap has the symbol `:` shifted using shift, and another with alt, and the application/compositor has a keybind for `ctrl+shift+:`. Which modifiers do we send, and how would we produce the `:` while keeping the shortcut intact.

I took an approach which I think balances the tradeoffs well.

- I assume the input device will be bound to the first seat (most compositors only implement one anyways), and detecting the active keymap from it. This means the keymap will be incorrect if there are a window is focused by a keyboard from a secondary seat with a different keymap.

- I build a reverse mapping of keycodes to keysyms for the active keymap, taking into account a few predetermined combinations of level-shifting keys (for efficiency), then brute-forcing the remaining ones. If the keymap is unknown for any reason, I fall back to the standard 8 "real" modifiers and sending the keysyms as-is. This will result in incorrect characters being type if the keymap does not match, like when you type on a US keyboard set to dvorak without realising it.

- I map the raw keycode from the virtual keyboard client to the keysym, then look up that keysym in the reverse mapping to produce the raw key events which would reproduce that keysym when interpreted under the active keymap.

- I track the keysyms and "real" modifiers consistently across keymaps so keymap changes (which could happen frequently if there are multiple keyboards with different layouts defined) don't leave modifiers stuck.

- I pass through non-level-shifting modifiers as-is, emitting key up/down events for them, and updating the modifier mask with the mapped bitmask index if required.

- For level-shifting modifiers, I still update the modifier mask with the mapped indexes, but when typing a letter which requires different modifiers, I temporarily send a key-up for that modifer, but still keep it in the modifier mask. This isn't perfect, but works fine for most clients since they usually interpret keybindings including non-modifiers based on the modifer mask on the final letter rather than watching the key up/down events on each individual modifier key. See c197764fce0e1860c0825b2bf2ffd3aeed50b975.

Note the limitations of this approach are similar those of [`XTEST`](https://xorg.freedesktop.org/archive/X11R7.7/doc/xextproto/xtest.html) on X11, which works at a lower level than the Wayland virtual keyboard protocol (it uses raw keycodes and leaves it to the sending client to map those to the active keymap, like what I do here).

I should probably take a closer look at [`XkbKeysymToModifiers`](https://xorg.freedesktop.org/archive/current/doc/man/man3/XkbKeysymToModifiers.3.xhtml) and [`XKeysymToKeycode`](https://xorg.freedesktop.org/archive/current/doc/man/man3/XStringToKeysym.3.xhtml) and see if there's a better way to build the reverse mapping.

#### How keyboard input works in Wayland

To accept input, a client is supposed to bind to all [`wl_seat`](https://wayland-book.com/seat.html)s. Note that most compositors only implement a single seat (note that the only reason you'd want more seats is if you wanted different groups of devices to be able to be used independently with their own cursor, focus, etc).

Each seat may have the `keyboard` capability, meaning you can get a `wl_keyboard` for the seat. There is only one `wl_keyboard` for each seat, even if multiple physical keyboards are attached to it. When a `wl_surface` (i.e., window) receives keyboard input, the `enter` event is sent for whichever `wl_keyboard` (remember, there's one per seat) is focusing the surface.

At the same time, each seat with the `keyboard` capability will send `keymap` events for its `wl_keyboard` object whenever the active keymap changes. In other words, it will send the keymap of the physical keyboard which is going to be sending events to it. If you have use multiple keyboards with different keymaps, clients will get `keymap` events as you switch which one you're typing on.

After receiving a `keymap` event, clients will receive keycodes from the keyboard, which must then be translated to the keysym using the active keymap. This is generally handled by a library like [xkbcommon](https://xkbcommon.org/).

As a user, you typically only care about the keycode (i.e., the character which gets typed) rather than the keysym (an internal detail), though for the `us` layout, they tend to be the same, just [shifted by 8](https://unix.stackexchange.com/questions/537982/why-do-evdev-keycodes-and-x11-keycodes-differ-by-8).

You may then wonder how modifiers like ctrl and shift work. An xkb keymap can contain levels which change the keycodes for each key, where pressing certain modifiers (e.g., alt, shift) would change the level. All modifiers, whether they shift levels or not, are sent as raw `key` events (which contain the serial, time, and up/down state) (this is needed for certain things to work, e.g., pressing just alt to show a menu bar) in addition to the `modifiers` event which contains a bitmask the state of all modifiers supported by the keymap.

All xkb keymaps [define](https://xkbcommon.org/doc/current/keymap-text-format-v1-v2.html#modifiers-declaration-and-binding) 8 "real" modifiers, which are always the same: `shift=0`, `caps=1`, `control=2`, and `mod1..mod5=3..7`. A keymap may then also define additional "virtual" modifiers.

To clarify things further, the after a application translates the key using the keymap, the XKB keysym will have any level-shifting modifiers applied. This is important since it means that applications can handle combinations like ctrl+shift+a unambiguously. Applications do not need to re-apply modifiers to the characters; that's the purpose of xkbcommon. They only need to look at modifiers if they want to, and can implement keyboard shortcuts with either the modifier bitmask or by tracking the key up/down events of the modifier keys themselves (side note: this is also one reason why broken virtual keyboard implementations may cause issues in some applications, but not others).

You can confirm how this works by experimenting with the [`wev`](https://git.sr.ht/~sircmpwn/wev) tool. I also found [this post](https://drewdevault.com/blog/Input-handling-in-wlroots/) helpful for starting to understand keyboard handling in compositors.
