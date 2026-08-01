use auv_driver_common::{DriverDescriptor, PlatformKind};

/// Capabilities exposed by the Linux Wayland desktop driver slice.
///
/// Input is foreground delivery through either RemoteDesktop or compositor
/// virtual-input protocols. Coordinate-targeted pointer clicks report a
/// fallback reason because Wayland does not provide background window input.
pub const LINUX_DESKTOP_CAPABILITIES: &[&str] = &[
  "desktop.list-displays",
  "desktop.capture-display",
  "desktop.capture-region",
  "desktop.list-windows",
  "desktop.capture-window",
  "desktop.capture-ax-tree",
  "desktop.recognize-image-text",
  "desktop.find-window-text",
  "desktop.wait-window-text",
  "desktop.probe-permissions",
  "clipboard.snapshot",
  "clipboard.restore",
  "clipboard.set-text",
  "control.click-point",
  "control.click-window-point",
  "control.scroll-point",
  "control.scroll-window-point",
  "control.type-text",
  "control.paste-text-preserve-clipboard",
  "control.press-key",
  "control.copy",
  "control.paste",
  "control.focus-ax-node",
  "control.select-ax-node",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxDriverDescriptor {
  pub id: &'static str,
  pub platform: PlatformKind,
  pub description: &'static str,
}

impl LinuxDriverDescriptor {
  pub fn as_driver_descriptor(&self) -> DriverDescriptor {
    DriverDescriptor {
      id: self.id,
      platform: self.platform,
      description: self.description,
    }
  }
}

pub fn linux_driver_descriptor() -> LinuxDriverDescriptor {
  LinuxDriverDescriptor {
    id: "linux.desktop",
    platform: PlatformKind::Linux,
    description: "Linux Wayland desktop driver: portal capture, AT-SPI window/accessibility observation, Tesseract OCR, text clipboard, RemoteDesktop input, niri virtual-input fallback, and portal readiness probes.",
  }
}
