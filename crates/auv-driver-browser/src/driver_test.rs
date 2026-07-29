use auv_driver_common::{Driver, PlatformKind};

use super::{BrowserDriver, parse_devtools_active_port};

#[test]
fn driver_descriptor_uses_browser_platform() {
  let driver = BrowserDriver::new();
  let descriptor = driver.descriptor();

  assert_eq!(descriptor.id, "browser.cdp");
  assert_eq!(descriptor.platform, PlatformKind::Browser);
  assert!(driver.options().headless);
  assert!(driver.options().sandbox);
  assert!(!driver.options().ignore_certificate_errors);
}

#[test]
fn devtools_active_port_parser_requires_browser_endpoint() {
  assert_eq!(
    parse_devtools_active_port("9222\n/devtools/browser/test-id\n").unwrap().as_deref(),
    Some("ws://127.0.0.1:9222/devtools/browser/test-id")
  );
  assert!(parse_devtools_active_port("9222\n").unwrap().is_none());
  assert!(parse_devtools_active_port("invalid\n/devtools/browser/test-id\n").is_err());
  assert!(parse_devtools_active_port("9222\n/devtools/page/test-id\n").is_err());
}
