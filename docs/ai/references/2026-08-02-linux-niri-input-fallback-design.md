# Linux niri input fallback design

Date: 2026-08-02

## Owner-approved scope

The repository owner requested a Linux/niri desktop-control path for the NixOS
desktop agent. Work is isolated on `feat/linux-niri-remote-desktop`; no pull
request is to be opened.

## Problem

The Linux driver currently obtains keyboard and pointer access solely through
`org.freedesktop.portal.RemoteDesktop`. Niri exposes ScreenCast compatibility
but does not implement Mutter's RemoteDesktop session interface, which the
GNOME portal backend expects. Capture can therefore work while input-session
creation fails.

Niri exposes the standard virtual-keyboard protocol and the wlroots
virtual-pointer protocol. Those protocols provide a compositor-native input
path for the active Wayland session.

Niri also cannot attach clipboard transfer to a RemoteDesktop portal session.
Its fallback uses `wl-copy` and `wl-paste`; the packaged desktop must make those
commands available.

## Contract

Keep the existing typed input operations and results:

```text
typed operation
  -> Linux input-session selection
  -> portal RemoteDesktop or native Wayland
  -> InputActionResult and trace
  -> independent observation/verification
```

The portal remains the first choice on general Linux desktops. A niri session
may select the native backend first to avoid a known-incompatible portal
round-trip. If the preferred backend fails, the other backend is attempted and
both failures are preserved in the returned diagnostic.

The selected backend must be observable in debug output. The public delivery
path remains `ForegroundSystemEvents`; a backend is an implementation detail,
not a new input policy.

## Native backend

- Bind `zwp_virtual_keyboard_manager_v1`.
- Send an XKB keymap once per virtual keyboard.
- Convert the driver's existing X11 keysym representation to Linux evdev key
  codes before sending key state.
- Bind `zwlr_virtual_pointer_manager_v1`.
- Use the compositor's output layout to convert desktop coordinates to an
  absolute pointer position.
- Preserve click count, click interval, scroll, and chord-release semantics.
- Keep the Wayland connection and event queue alive for the driver session.

## Deferred boundaries

- NOTICE: portal capture and native input are deliberately separate sessions.
- NOTICE: direct Unicode typing remains limited by the existing keysym API;
  arbitrary Unicode should use the clipboard-paste path.
- NOTICE: niri clipboard operations require `wl-clipboard`; other desktops
  continue to prefer the portal session and use the commands only as fallback.
- TODO: live-validate non-niri wlroots compositors before claiming support for
  them in the support matrix.

## Validation

Static:

- `cargo fmt --check`
- `cargo check -p auv-driver-linux`
- `cargo test -p auv-driver-linux`
- `git diff --check`

Live in the NixOS/niri VM:

- portal capture succeeds
- the native backend is selected without a RemoteDesktop permission loop
- pointer motion and click reach the expected target
- key chords and ASCII typing reach the focused app
- Unicode clipboard paste works and restores the prior clipboard
- a post-action capture/AT-SPI observation verifies the visible result
