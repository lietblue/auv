use auv_driver_common::PlatformKind;

use super::{BROWSER_CDP_CAPABILITIES, browser_driver_descriptor};

#[test]
fn descriptor_uses_browser_cdp_namespace() {
  let descriptor = browser_driver_descriptor();

  assert_eq!(descriptor.id, "browser.cdp");
  assert_eq!(descriptor.platform, PlatformKind::Browser);
  assert!(BROWSER_CDP_CAPABILITIES.contains(&"browser.capture-page"));
  assert!(BROWSER_CDP_CAPABILITIES.contains(&"browser.click-element"));
  assert!(BROWSER_CDP_CAPABILITIES.contains(&"browser.set-file-input-files"));
}
