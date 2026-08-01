//! Text clipboard snapshot/restore/set for Linux Wayland desktops.
//!
//! GNOME and other portal-backed desktops use the clipboard attached to an XDG
//! RemoteDesktop session. Niri does not implement that portal, so it prefers
//! the standard `wl-copy` / `wl-paste` command pair and falls back to the
//! portal when those commands are unavailable.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use auv_driver_common::error::DriverResult;

use crate::driver::LinuxDriverSessionState;
use crate::error::backend;
use crate::native::input::prefers_native_wayland;
use crate::native::portal::{ClipboardSession as PortalClipboardSession, PortalClipboard};

#[derive(Debug)]
pub(crate) enum ClipboardSession {
  Portal(PortalClipboardSession),
  WaylandCommands(WaylandClipboard),
}

#[derive(Debug, Default)]
pub(crate) struct WaylandClipboard {
  owner: Option<Child>,
}

impl Drop for WaylandClipboard {
  fn drop(&mut self) {
    self.stop_owner();
  }
}

impl WaylandClipboard {
  fn open() -> DriverResult<Self> {
    probe_wayland_commands()?;
    Ok(Self::default())
  }

  fn snapshot(&self) -> DriverResult<String> {
    wayland_snapshot()
  }

  fn set_text(&mut self, text: &str) -> DriverResult<()> {
    self.stop_owner();

    let mut child = Command::new("wl-copy")
      .args(["--foreground", "--type", "text/plain;charset=utf-8"])
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .spawn()
      .map_err(|error| backend(format!("failed to start wl-copy: {error}")))?;
    let write_result = child.stdin.take().expect("wl-copy stdin was piped").write_all(text.as_bytes());
    if let Err(error) = write_result {
      let _ = child.kill();
      let _ = child.wait();
      return Err(backend(format!("failed to write text to wl-copy: {error}")));
    }

    // wl-copy must remain alive while it owns the selection. Give it a short
    // window to fail (for example, when WAYLAND_DISPLAY is invalid), then keep
    // the live owner in the session instead of waiting forever for it to exit.
    std::thread::sleep(Duration::from_millis(25));
    match child.try_wait() {
      Ok(None) => {
        self.owner = Some(child);
        Ok(())
      }
      Ok(Some(status)) => {
        let output = child.wait_with_output().map_err(|error| backend(format!("failed to collect wl-copy output: {error}")))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(backend(format!("wl-copy exited with {status}; {}", stderr.trim())))
      }
      Err(error) => {
        let _ = child.kill();
        let _ = child.wait();
        Err(backend(format!("failed to inspect wl-copy: {error}")))
      }
    }
  }

  fn stop_owner(&mut self) {
    if let Some(mut owner) = self.owner.take() {
      let _ = owner.kill();
      let _ = owner.wait();
    }
  }
}

impl ClipboardSession {
  fn open() -> DriverResult<Self> {
    if prefers_native_wayland() {
      open_with_fallback(
        ("wl-clipboard", || WaylandClipboard::open().map(Self::WaylandCommands)),
        ("RemoteDesktop portal", || PortalClipboard::open().map(Self::Portal)),
      )
    } else {
      open_with_fallback(
        ("RemoteDesktop portal", || PortalClipboard::open().map(Self::Portal)),
        ("wl-clipboard", || WaylandClipboard::open().map(Self::WaylandCommands)),
      )
    }
  }

  fn snapshot(&mut self) -> DriverResult<String> {
    match self {
      Self::Portal(session) => session.snapshot(),
      Self::WaylandCommands(session) => session.snapshot(),
    }
  }

  fn set_text(&mut self, text: &str) -> DriverResult<()> {
    match self {
      Self::Portal(session) => session.set_text(text),
      Self::WaylandCommands(session) => session.set_text(text),
    }
  }
}

/// Reads the current clipboard text. Returns an empty string when the active
/// clipboard owner has no `text/plain;charset=utf-8` payload.
pub fn snapshot(state: &Arc<Mutex<LinuxDriverSessionState>>) -> DriverResult<String> {
  with_clipboard_session(state, |session| session.snapshot())
}

/// Writes `snapshot` back to the clipboard as UTF-8 text.
pub fn restore(state: &Arc<Mutex<LinuxDriverSessionState>>, snapshot: &str) -> DriverResult<()> {
  write_text(state, snapshot)
}

/// Installs `text` as the clipboard's UTF-8 text payload.
pub fn set_text(state: &Arc<Mutex<LinuxDriverSessionState>>, text: &str) -> DriverResult<()> {
  write_text(state, text)
}

fn write_text(state: &Arc<Mutex<LinuxDriverSessionState>>, text: &str) -> DriverResult<()> {
  with_clipboard_session(state, |session| session.set_text(text))
}

fn with_clipboard_session<T>(
  state: &Arc<Mutex<LinuxDriverSessionState>>,
  operation: impl FnOnce(&mut ClipboardSession) -> DriverResult<T>,
) -> DriverResult<T> {
  let mut state = state.lock().expect("linux driver session state poisoned");
  if state.clipboard_session.is_none() {
    state.clipboard_session = Some(ClipboardSession::open()?);
  }
  operation(state.clipboard_session.as_mut().expect("clipboard session was just initialized"))
}

fn probe_wayland_commands() -> DriverResult<()> {
  for program in ["wl-copy", "wl-paste"] {
    let status = Command::new(program)
      .arg("--version")
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .map_err(|error| backend(format!("failed to start {program}: {error}")))?;
    if !status.success() {
      return Err(backend(format!("{program} --version exited with {status}")));
    }
  }
  Ok(())
}

fn wayland_snapshot() -> DriverResult<String> {
  let output = Command::new("wl-paste")
    .arg("--no-newline")
    .stdin(Stdio::null())
    .output()
    .map_err(|error| backend(format!("failed to start wl-paste: {error}")))?;
  if output.status.success() {
    return String::from_utf8(output.stdout).map_err(|error| backend(format!("wl-paste returned non-UTF-8 text: {error}")));
  }

  let stderr = String::from_utf8_lossy(&output.stderr);
  let diagnostic = stderr.to_ascii_lowercase();
  if diagnostic.contains("nothing is copied") || diagnostic.contains("no selection") {
    // No selection or no matching text MIME is the clipboard API's documented
    // empty-string case.
    return Ok(String::new());
  }
  Err(backend(format!("wl-paste exited with {}; {}", output.status, stderr.trim())))
}

fn open_with_fallback(
  first: (&str, impl FnOnce() -> DriverResult<ClipboardSession>),
  second: (&str, impl FnOnce() -> DriverResult<ClipboardSession>),
) -> DriverResult<ClipboardSession> {
  match (first.1)() {
    Ok(session) => Ok(session),
    Err(first_error) => match (second.1)() {
      Ok(session) => Ok(session),
      Err(second_error) => Err(backend(format!(
        "failed to open Linux clipboard session; {} failed: {}; {} failed: {}",
        first.0, first_error, second.0, second_error
      ))),
    },
  }
}
