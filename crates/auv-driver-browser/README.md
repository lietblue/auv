# auv-driver-browser

`auv-driver-browser` is AUV's library-only Chromium driver. It launches a local
Chrome/Chromium process or connects to an existing browser-level Chrome DevTools
Protocol (CDP) WebSocket endpoint, then exposes typed page and DOM APIs.

Status: **v0, tested with a hermetic CDP transport**. This is not a
`live-validated` or `supported` browser product surface.

## Included in v0

- local Chrome/Chromium launch, headless by default
- connection to an existing loopback `ws://` or remote `wss://` browser CDP
  endpoint
- page list, open, close, navigation, and reload
- viewport or full-page PNG capture
- JSON-returning JavaScript evaluation
- CSS-selector query, explicit element resolution, and wait
- element click and text entry
- hidden or visible `<input type="file">` selection without a native chooser
- page scrolling
- canonical `InputActionResult` values using the `cdp_input` delivery path

The driver keeps browser mechanics below the application-operation layer.
Douyin/TikTok navigation rules, feed policies, login handling, content
selection, and other site-specific behavior do not belong in this crate.

DOM observations and navigation-readiness checks use isolated JavaScript
worlds. Each `ElementRef` is bound to a CDP backend node, document loader
generation, and execution context. Clicks scroll the element into view, move
the pointer, then re-observe its bounds and center-point hit target before
dispatch. An old element reference is rejected after navigation instead of
being silently applied to a new document.

## Local session

```rust
use auv_driver_browser::{BrowserDriver, CssSelector, NavigationOptions};
use auv_driver_common::Driver;

let session = BrowserDriver::new().open_local()?;
let page = session
  .page()
  .open("https://example.com", NavigationOptions::default())?;
let heading = session
  .dom()
  .resolve(&page.reference, &CssSelector::new("h1")?)?;

println!("{}", heading.text);
# Ok::<(), auv_driver_common::DriverError>(())
```

Set `BrowserLaunchOptions::executable` for an explicit binary. Otherwise the
driver checks `CHROME`, common platform install locations, and executable names
on `PATH`. The default temporary browser profile is removed with the session;
provide `BrowserLaunchOptions::user_data_dir` only when persistent browser state
is intentional.

## Existing CDP session

```rust
use auv_driver_browser::{BrowserConnectOptions, BrowserDriver};

let session = BrowserDriver::connect(BrowserConnectOptions::new(
  "ws://127.0.0.1:9222/devtools/browser/<id>",
))?;

println!("{:?}", session.page().list()?);
# Ok::<(), auv_driver_common::DriverError>(())
```

`BrowserDriver::connect` expects the browser WebSocket URL itself. Unencrypted
`ws://` is accepted only for `localhost` or a loopback IP; non-loopback
endpoints must use certificate-validated `wss://`. WebSocket redirects are not
followed. HTTP endpoint discovery through `/json/version` is outside v0.

## Safety and limits

A CDP endpoint can inspect pages, execute JavaScript, and control the associated
browser profile. Keep it on a trusted loopback or otherwise authenticated
transport, and do not expose it to an untrusted network.

`BrowserConnectOptions` has separate connection and command deadlines and
redacts the endpoint from `Debug` output. A command transport timeout makes the
session connection non-reusable because the side-effect outcome may be
unknown. A deadline watchdog interrupts partial-frame or partial-write
slowloris behavior at the underlying connection. Outgoing request size,
incoming WebSocket messages, DOM match counts, per-element text and attributes,
aggregate DOM observation bytes, screenshot dimensions, pixels, and decode
allocation are bounded. `DomElement` reports when its text or attributes were
truncated.

`InputActionResult::success` proves that Chromium acknowledged the selected CDP
input delivery path. It does not prove that a website-level intent succeeded;
the site operation above this driver must observe and verify that semantic
postcondition. Likewise, `PageApi::evaluate` deliberately executes caller code
in the page's main world and should be treated as a high-trust primitive.
`DomApi::set_file_input_files` canonicalizes every path, accepts only existing
regular files, and replaces the input's current selection. The site layer must
still enforce its own file-count, media-type, size, and upload-completion rules.

This crate currently has no CLI or MCP registration and does not integrate
browser actions with AUV run recording or artifacts. It is Chromium/CDP-only;
WebDriver, WebDriver BiDi, Firefox, WebKit, iframe/shadow-DOM traversal, network
interception, downloads, and browser-extension control are outside v0.

Page capture returns still images. Video capture, audio capture, CDP screencast
assembly, and media start/end detection are intentionally not implemented.
