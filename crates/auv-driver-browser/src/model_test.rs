use std::path::PathBuf;
use std::time::Duration;

use auv_driver_common::DriverError;

use super::{BrowserConnectOptions, BrowserLaunchOptions, BrowserViewport, CssSelector, PageRef};

#[test]
fn launch_options_reject_zero_sized_viewport() {
  let options = BrowserLaunchOptions {
    viewport: BrowserViewport {
      width: 0,
      height: 720,
    },
    ..BrowserLaunchOptions::default()
  };

  assert!(matches!(options.validate(), Err(DriverError::InvalidInput { .. })));
}

#[test]
fn launch_options_reject_missing_explicit_executable() {
  let options = BrowserLaunchOptions {
    executable: Some(PathBuf::from("/definitely/missing/auv-browser")),
    ..BrowserLaunchOptions::default()
  };

  assert!(matches!(options.validate(), Err(DriverError::InvalidInput { .. })));
}

#[test]
fn connect_options_require_websocket_endpoint_and_timeout() {
  let mut options = BrowserConnectOptions::new("http://127.0.0.1:9222");
  assert!(matches!(options.validate(), Err(DriverError::InvalidInput { .. })));

  options.websocket_url = "ws://127.0.0.1:9222/devtools/browser/test".to_string();
  options.command_timeout = Duration::ZERO;
  assert!(matches!(options.validate(), Err(DriverError::InvalidInput { .. })));

  options.command_timeout = Duration::from_secs(1);
  options.connect_timeout = Duration::ZERO;
  assert!(matches!(options.validate(), Err(DriverError::InvalidInput { .. })));
}

#[test]
fn launch_options_reject_driver_owned_extra_arguments() {
  for argument in [
    "--remote-debugging-port=9222",
    "--remote-debugging-address",
    "--user-data-dir=/tmp/shared-profile",
    "--no-sandbox",
    "--no-sandbox=true",
    "--disable-namespace-sandbox",
    "--disable-seccomp-filter-sandbox",
    "--no-zygote-sandbox",
    "--disable-landlock-sandbox",
    "--disable-webnn-sandbox",
    "--ignore-certificate-errors",
    "--ignore-certificate-errors=true",
    "--remote-debugging-pipe",
    "--headless=old",
    "--window-size=800,600",
  ] {
    let options = BrowserLaunchOptions {
      extra_args: vec![argument.into()],
      ..BrowserLaunchOptions::default()
    };
    assert!(matches!(options.validate(), Err(DriverError::InvalidInput { .. })), "{argument}");
  }
}

#[test]
fn unencrypted_remote_cdp_endpoint_is_rejected() {
  for endpoint in [
    "ws://example.com/devtools/browser/test",
    "ws://192.0.2.1:9222/devtools/browser/test",
  ] {
    let options = BrowserConnectOptions::new(endpoint);
    assert!(matches!(options.validate(), Err(DriverError::InvalidInput { .. })), "{endpoint}");
  }

  for endpoint in [
    "ws://localhost:9222/devtools/browser/test",
    "ws://127.0.0.1:9222/devtools/browser/test",
    "ws://[::1]:9222/devtools/browser/test",
    "wss://example.com/devtools/browser/test",
  ] {
    BrowserConnectOptions::new(endpoint).validate().unwrap();
  }
}

#[test]
fn connect_options_redact_endpoint_debug_output() {
  let options = BrowserConnectOptions::new("wss://user:secret@example.com/devtools/browser/id?token=secret");
  let debug = format!("{options:?}");

  assert!(!debug.contains("secret"));
  assert!(!debug.contains("example.com"));
  assert!(debug.contains("redacted CDP endpoint"));
}

#[test]
fn selectors_and_page_refs_reject_empty_values() {
  assert!(matches!(CssSelector::new("  "), Err(DriverError::InvalidInput { .. })));
  assert!(matches!(PageRef::new(""), Err(DriverError::InvalidInput { .. })));
}

#[test]
fn page_refs_validate_during_deserialization() {
  let page = PageRef::new("page-1").unwrap();
  let encoded = serde_json::to_string(&page).unwrap();

  assert_eq!(serde_json::from_str::<PageRef>(&encoded).unwrap(), page);
  assert!(serde_json::from_str::<PageRef>("\"\"").is_err());
  assert!(serde_json::from_str::<PageRef>("\"  \"").is_err());
}
