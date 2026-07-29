use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use auv_driver_common::{Driver, DriverDescriptor, DriverError, DriverResult, DriverSession};

use crate::cdp::{CdpBackend, OwnedBrowserProcess};
use crate::descriptor::{BrowserDriverDescriptor, browser_driver_descriptor};
use crate::model::{BrowserConnectOptions, BrowserLaunchOptions};

/// Configurable Chromium/CDP driver.
///
/// The value only stores launch configuration; it does not hold a browser or
/// network connection. Call [`Driver::open_local`] to start a driver-owned
/// Chromium process, or use [`BrowserDriver::connect`] to attach to an existing
/// CDP endpoint.
#[derive(Clone, Debug, Default)]
pub struct BrowserDriver {
  options: BrowserLaunchOptions,
}

impl BrowserDriver {
  /// Creates a driver with [`BrowserLaunchOptions::default`].
  pub fn new() -> Self {
    Self::default()
  }

  /// Creates a driver that will use `options` for local launches.
  ///
  /// Options are validated when [`Driver::open_local`] is called.
  pub fn with_options(options: BrowserLaunchOptions) -> Self {
    Self { options }
  }

  /// Returns the local-launch configuration stored by this driver.
  pub fn options(&self) -> &BrowserLaunchOptions {
    &self.options
  }

  /// Returns the browser-specific static driver descriptor.
  pub fn browser_descriptor(&self) -> BrowserDriverDescriptor {
    browser_driver_descriptor()
  }

  /// Attaches to an existing browser-level CDP WebSocket endpoint.
  ///
  /// The returned session owns the CDP connection but does **not** own or close
  /// the remote browser process. Dropping its last clone releases the
  /// connection. [`BrowserConnectOptions::connect_timeout`] is one absolute
  /// deadline for endpoint resolution and the TCP, TLS, and WebSocket
  /// handshakes. [`BrowserConnectOptions::command_timeout`] bounds each
  /// subsequent protocol command; a higher-level API call can issue several
  /// commands.
  ///
  /// Plain `ws://` is accepted only for a loopback endpoint. Remote connections
  /// must use certificate-validated `wss://`.
  pub fn connect(options: BrowserConnectOptions) -> DriverResult<BrowserDriverSession> {
    let backend = CdpBackend::connect(&options, None, None)?;
    Ok(BrowserDriverSession {
      backend: Arc::new(backend),
    })
  }
}

/// An active Chromium/CDP session.
///
/// Clones share one serialized protocol connection. If the session came from
/// [`Driver::open_local`], the shared backend also owns the Chromium child
/// process and any temporary profile. Dropping the final clone asks Chromium to
/// close, terminates the child if necessary, and removes a driver-created
/// temporary profile. A caller-supplied profile directory is never removed.
///
/// If the session came from [`BrowserDriver::connect`], dropping the final
/// clone only releases the connection; the attached browser remains owned by
/// its launcher.
#[derive(Clone)]
pub struct BrowserDriverSession {
  pub(crate) backend: Arc<CdpBackend>,
}

impl BrowserDriverSession {
  /// Returns the browser-specific static driver descriptor.
  pub fn browser_descriptor(&self) -> BrowserDriverDescriptor {
    browser_driver_descriptor()
  }
}

impl fmt::Debug for BrowserDriverSession {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("BrowserDriverSession").field("descriptor", &self.browser_descriptor()).finish_non_exhaustive()
  }
}

impl Driver for BrowserDriver {
  type Session = BrowserDriverSession;

  /// Returns this driver's descriptor in the shared driver format.
  fn descriptor(&self) -> DriverDescriptor {
    self.browser_descriptor().as_driver_descriptor()
  }

  /// Launches and owns a local Chromium process, then connects over CDP.
  ///
  /// Chromium is launched with an ephemeral debugging port bound to loopback.
  /// When no [`BrowserLaunchOptions::user_data_dir`] is supplied, the driver
  /// creates an isolated temporary profile and keeps it alive for the session.
  /// The returned session owns the child process and profile as described by
  /// [`BrowserDriverSession`].
  ///
  /// [`BrowserLaunchOptions::startup_timeout`] bounds waiting for Chromium to
  /// publish its endpoint. [`BrowserLaunchOptions::command_timeout`] bounds the
  /// initial CDP connection and each later protocol command. A higher-level API
  /// call can issue several commands. Failure during launch or connection
  /// cleans up the child process.
  fn open_local(&self) -> DriverResult<Self::Session> {
    self.options.validate()?;
    let executable = resolve_executable(self.options.executable.as_deref())?;
    let (profile, profile_path) = prepare_profile(self.options.user_data_dir.as_deref())?;
    let mut profile_argument = OsString::from("--user-data-dir=");
    profile_argument.push(profile_path.as_os_str());
    let mut command = Command::new(executable);
    command
      .arg("--remote-debugging-port=0")
      .arg("--remote-debugging-address=127.0.0.1")
      .arg(profile_argument)
      .arg(format!("--window-size={},{}", self.options.viewport.width, self.options.viewport.height))
      .arg("--no-first-run")
      .arg("--no-default-browser-check")
      .arg("--disable-background-networking")
      .arg("--disable-component-update")
      .arg("--disable-default-apps");
    if self.options.headless {
      command.arg("--headless=new");
    }
    if !self.options.sandbox {
      command.arg("--no-sandbox");
    }
    if self.options.ignore_certificate_errors {
      command.arg("--ignore-certificate-errors");
    }
    command.args(&self.options.extra_args);
    command.arg("about:blank").stdout(Stdio::null()).stderr(Stdio::null());

    let endpoint_path = profile_path.join("DevToolsActivePort");
    let previous_endpoint = match fs::read(&endpoint_path) {
      Ok(contents) => Some(contents),
      Err(error) if error.kind() == ErrorKind::NotFound => None,
      Err(error) => {
        return Err(backend(format!("failed to inspect existing Chromium CDP endpoint file {}: {error}", endpoint_path.display())));
      }
    };
    let mut child = command.spawn().map_err(|error| backend(format!("failed to launch Chromium: {error}")))?;
    let websocket_url = match wait_for_websocket_url(&mut child, &endpoint_path, previous_endpoint.as_deref(), self.options.startup_timeout)
    {
      Ok(url) => url,
      Err(error) => {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
      }
    };
    let process = OwnedBrowserProcess::new(child, profile);
    let connect = BrowserConnectOptions {
      websocket_url,
      connect_timeout: self.options.command_timeout,
      command_timeout: self.options.command_timeout,
    };
    let backend = CdpBackend::connect(&connect, Some(process), Some(self.options.viewport))?;
    Ok(BrowserDriverSession {
      backend: Arc::new(backend),
    })
  }
}

impl DriverSession for BrowserDriverSession {
  /// Returns this session's descriptor in the shared driver format.
  fn descriptor(&self) -> DriverDescriptor {
    self.browser_descriptor().as_driver_descriptor()
  }
}

fn prepare_profile(user_data_dir: Option<&Path>) -> DriverResult<(Option<tempfile::TempDir>, PathBuf)> {
  if let Some(directory) = user_data_dir {
    std::fs::create_dir_all(directory)
      .map_err(|error| backend(format!("failed to create browser user-data directory {}: {error}", directory.display())))?;
    return Ok((None, directory.to_path_buf()));
  }
  let directory = tempfile::Builder::new()
    .prefix("auv-browser-profile-")
    .tempdir()
    .map_err(|error| backend(format!("failed to create temporary browser profile: {error}")))?;
  let path = directory.path().to_path_buf();
  Ok((Some(directory), path))
}

fn resolve_executable(explicit: Option<&Path>) -> DriverResult<PathBuf> {
  if let Some(explicit) = explicit {
    return Ok(explicit.to_path_buf());
  }
  if let Some(explicit) = env::var_os("CHROME").map(PathBuf::from)
    && explicit.is_file()
  {
    return Ok(explicit);
  }

  let mut candidates = platform_candidates();
  candidates.extend(path_candidates(&[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "chrome",
  ]));
  candidates
    .into_iter()
    .find(|candidate| candidate.is_file())
    .ok_or_else(|| backend("could not find Chrome or Chromium; set BrowserLaunchOptions.executable or the CHROME environment variable"))
}

fn platform_candidates() -> Vec<PathBuf> {
  let mut candidates = Vec::new();
  #[cfg(target_os = "macos")]
  {
    candidates.push(PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"));
    candidates.push(PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"));
    candidates.push(PathBuf::from("/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary"));
  }
  #[cfg(target_os = "windows")]
  {
    for root in [
      env::var_os("PROGRAMFILES"),
      env::var_os("PROGRAMFILES(X86)"),
      env::var_os("LOCALAPPDATA"),
    ]
    .into_iter()
    .flatten()
    {
      candidates.push(PathBuf::from(&root).join("Google").join("Chrome").join("Application").join("chrome.exe"));
      candidates.push(PathBuf::from(&root).join("Chromium").join("Application").join("chrome.exe"));
    }
  }
  #[cfg(target_os = "linux")]
  {
    candidates.push(PathBuf::from("/usr/bin/google-chrome"));
    candidates.push(PathBuf::from("/usr/bin/google-chrome-stable"));
    candidates.push(PathBuf::from("/usr/bin/chromium"));
    candidates.push(PathBuf::from("/usr/bin/chromium-browser"));
  }
  candidates
}

fn path_candidates(names: &[&str]) -> Vec<PathBuf> {
  let Some(search_path) = env::var_os("PATH") else {
    return Vec::new();
  };
  env::split_paths(&search_path)
    .flat_map(|directory| {
      names.iter().map(move |name| {
        let executable = if cfg!(target_os = "windows") {
          OsString::from(format!("{name}.exe"))
        } else {
          OsString::from(name)
        };
        directory.join(executable)
      })
    })
    .collect()
}

fn wait_for_websocket_url(
  child: &mut Child,
  endpoint_path: &Path,
  previous_endpoint: Option<&[u8]>,
  timeout: Duration,
) -> DriverResult<String> {
  let deadline = Instant::now().checked_add(timeout).ok_or_else(|| backend("Chromium startup deadline overflowed"))?;
  loop {
    match fs::read(endpoint_path) {
      Ok(contents) if previous_endpoint != Some(contents.as_slice()) => {
        let contents = std::str::from_utf8(&contents)
          .map_err(|error| backend(format!("Chromium CDP endpoint file {} was not UTF-8: {error}", endpoint_path.display())))?;
        if let Some(url) = parse_devtools_active_port(contents)? {
          return Ok(url);
        }
      }
      Ok(_) => {}
      Err(error) if error.kind() == ErrorKind::NotFound => {}
      Err(error) => {
        return Err(backend(format!("failed to read Chromium CDP endpoint file {}: {error}", endpoint_path.display())));
      }
    }
    if let Some(status) = child.try_wait().map_err(|error| backend(format!("failed to inspect Chromium process: {error}")))? {
      return Err(backend(format!("Chromium exited before publishing its CDP endpoint: {status}")));
    }
    let remaining = deadline
      .checked_duration_since(Instant::now())
      .filter(|duration| !duration.is_zero())
      .ok_or_else(|| backend("timed out waiting for Chromium to publish its CDP endpoint"))?;
    thread::sleep(Duration::from_millis(25).min(remaining));
  }
}

fn parse_devtools_active_port(contents: &str) -> DriverResult<Option<String>> {
  let mut lines = contents.lines();
  let Some(port) = lines.next() else {
    return Ok(None);
  };
  let Some(browser_path) = lines.next() else {
    return Ok(None);
  };
  let port = port.trim().parse::<u16>().map_err(|error| backend(format!("Chromium published an invalid CDP port: {error}")))?;
  if port == 0 {
    return Err(backend("Chromium published CDP port zero"));
  }
  let browser_path = browser_path.trim();
  if !browser_path.starts_with("/devtools/browser/") || browser_path.chars().any(char::is_whitespace) {
    return Err(backend("Chromium published an invalid browser CDP path"));
  }
  Ok(Some(format!("ws://127.0.0.1:{port}{browser_path}")))
}

fn backend(message: impl Into<String>) -> DriverError {
  DriverError::Backend {
    message: message.into(),
  }
}

#[cfg(test)]
#[path = "driver_test.rs"]
mod tests;
