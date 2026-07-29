# Browser Driver v0 Implementation Reference

Date: 2026-07-29

Status: implemented as a library-only Chromium/CDP driver; automated behavior
is fixture-tested, with no live browser support claim.

## Summary

`auv-driver-browser` adds a capability-oriented browser driver below AUV's
application-operation layer. It can own a local Chrome/Chromium process or
connect directly to a browser-level Chrome DevTools Protocol (CDP) WebSocket.
The session exposes typed page and DOM APIs without adding browser commands to
the product CLI, MCP frontend, runtime catalog, or run-recording path.

The v0 boundary is deliberately browser-generic. Site workflows such as
Douyin/TikTok feed navigation, login handling, content selection, playback
state interpretation, and recording policy belong in a separate app/operation
crate. They are not driver capabilities.

## Crate Boundary

```text
site/application operation crate
  -> auv-driver-browser
       -> auv-driver-common contracts
       -> Chromium browser-level CDP WebSocket
```

`auv-driver-browser` depends on `auv-driver-common`, not on the root runtime,
CLI, tracing, or an app-specific crate. Browser input actions reuse the
canonical `InputActionResult` contract and identify their delivery path as
`InputDeliveryPath::CdpInput` (`cdp_input` on the wire).

The driver descriptor is:

| Field | Value |
| --- | --- |
| id | `browser.cdp` |
| platform | `PlatformKind::Browser` |
| evidence level | `tested` |
| product integration | library only |

## Implemented v0 Surface

| Area | API boundary | Behavior |
| --- | --- | --- |
| lifecycle | `BrowserDriver::open_local` | Finds or uses an explicit Chrome/Chromium binary, starts it with an isolated profile and browser-level CDP endpoint, and owns process cleanup. Headless mode is the default. |
| remote lifecycle | `BrowserDriver::connect` | Connects to an explicit loopback `ws://` or certificate-validated remote `wss://` browser CDP URL without following redirects. |
| page | `PageApi` | Lists, resolves, opens, closes, navigates, reloads, captures, evaluates JSON, and scrolls pages. |
| DOM | `DomApi` | Queries CSS selectors, resolves one explicit result, waits, clicks, and inserts text. |
| observation | `Page`, `DomElement` | Returns typed page metadata and DOM snapshots with attributes, text, visibility, and viewport bounds where available. |
| capture | `PageCaptureOptions` | Produces viewport or full-page still-image `Capture` values. |
| input | canonical common contracts | Dispatches CDP mouse, text, and wheel input and returns `InputActionResult`. |

Selectors are strict by default. A selector that matches more than one element
is rejected until the caller chooses an explicit index with
`CssSelector::at(index)`. A resolved `ElementRef` is tied to its page and CDP
backend node id, document loader generation, and isolated-world execution
context so action APIs do not silently re-resolve a different element. Clicks
scroll into view, move the pointer, re-observe the post-hover bounds, and
require the center-point hit test to resolve to the target or one of its
descendants before input is dispatched.

Each DOM snapshot truncates text and attributes at documented per-element
limits and reports the truncation. Queries also enforce match-count and
aggregate observation-byte limits.

## Local Launch Defaults

The launch path uses conservative defaults:

- headless mode enabled
- Chromium sandbox enabled
- certificate errors are not ignored
- temporary user-data directory unless the caller supplies one
- CDP bound to `127.0.0.1`
- explicit startup, connection, navigation-wait, and command deadlines
- driver-owned launch flags cannot be overridden through `extra_args`

Binary resolution checks, in order, an explicit
`BrowserLaunchOptions::executable`, the `CHROME` environment variable, common
platform installation paths, and common Chrome/Chromium executable names on
`PATH`.

## Evidence

The highest justified level is `tested`.

- descriptor and model tests verify the public identity, safe defaults, URL
  validation, and selector semantics
- session tests use a hermetic fake CDP WebSocket to exercise delayed loader
  commit, download rejection, page lifecycle, document-generation stale
  rejection, isolated-world readiness and DOM resolution, post-hover
  obscured-element rejection,
  still-image capture, JSON-only JavaScript evaluation, click, text, and scroll
  request/response behavior
- transport regressions cover continuous event streams, partial-frame
  slowloris delivery, stalled WebSocket handshakes, in-flight DOM-wait
  deadlines, and connection tainting after unknown command outcomes
- resource regressions cover request-size and aggregate DOM observation limits
  plus rejection of an oversized grayscale screenshot before RGBA expansion
- CI explicitly runs:

  ```text
  cargo check --package auv-driver-browser --all-targets
  cargo test --package auv-driver-browser
  ```

The `validate` example was also run successfully on 2026-07-29 with Google
Chrome 150.0.7871.187 on arm64 macOS 26.5.2: it launched a temporary headless
profile, clicked a local data-URL button, observed `text="clicked"`, and
captured the configured 1280x720 viewport. This is an ad hoc developer smoke
check, not an automated live regression or maintained product setup path. It
does not exercise a public website, real user profile, or media stream, so the
matrix remains at `tested` rather than `live-validated` or `supported`.

## Security Boundary

CDP is a high-trust control channel: a connected client can inspect pages,
execute JavaScript, and control the associated browser profile. Callers should
bind a debugging endpoint to loopback or place it behind an authenticated,
trusted transport. The v0 client enforces loopback for plaintext `ws://`,
requires TLS for non-loopback endpoints, validates certificates, refuses
WebSocket redirects, applies bounded handshake and absolute command deadlines,
and redacts supplied endpoint URLs from `Debug` output. AUV does not add
application authentication around a supplied remote endpoint.

The absolute command deadline is enforced by a per-command connection
watchdog, not only by socket idle timeouts. If a peer trickles a partial frame
or consumes a partial write without completing it, the watchdog shuts down the
underlying connection at the wall-clock deadline and the session rejects
further commands.

After a request is sent, a transport timeout has an unknown side-effect
outcome. The connection is therefore marked non-reusable instead of discarding
a late response and risking a duplicate click or navigation.

Page content is also an untrusted resource source. Outgoing CDP requests,
WebSocket messages, broad selector matches, per-element DOM text and
attributes, aggregate DOM observations, screenshot dimensions, pixel counts,
and image decode allocation have explicit v0 limits. Screenshot pixel
dimensions are validated before expanding a decoded image to RGBA. Navigation
readiness, internal DOM snapshots, and hit-test functions run in isolated
worlds; public `PageApi::evaluate` intentionally runs caller code in the page's
main world and remains a high-trust primitive.

Persistent `user_data_dir` use also changes the risk boundary. The default
temporary profile limits accidental state reuse; callers that opt into a real
profile own its cookies, sessions, extensions, and data exposure.

## Intentional Deferrals

| Area | v0 decision |
| --- | --- |
| AUV CLI, MCP, catalog, tracing, artifacts, and replay | Not registered. A later owner-approved runtime slice must define recording and frontend semantics before exposing browser commands. |
| Site-specific behavior | Excluded. Douyin/TikTok and other web-app workflows belong in separate operation crates above the browser driver. |
| Video/audio recording | Excluded. v0 provides still-image screenshots only; CDP screencast assembly, audio capture, and media start/end detection require a separately designed media/operation boundary. |
| HTTP CDP discovery | Deferred. `BrowserDriver::connect` requires the browser WebSocket URL rather than resolving `/json/version`. |
| Browser engines | Chromium/CDP only. WebDriver, WebDriver BiDi, Firefox, and WebKit are not claimed. |
| Complex DOM topology | Cross-frame selection, shadow-DOM traversal, ARIA locators, and automatic stale-element recovery are not part of v0. |
| Network and browser services | Request interception, downloads, extension control, permissions, and profile migration are not part of v0. |
| Site access policy | CAPTCHA bypass, authentication circumvention, DRM bypass, and anti-bot evasion are not driver responsibilities. |

These omissions are scope decisions, not evidence that the absent behavior can
be inferred from generic JavaScript evaluation.

An acknowledged `InputActionResult` is delivery evidence only. Website-level
success must be established by an operation-layer postcondition; the driver
does not infer that a click changed playback, opened a feed item, or completed
another site intent.

## Relationship to Android Automation

This crate automates Chromium pages through CDP. It does not operate Android
apps, ADB, or `scrcpy`, and it does not replace a future
`auv-driver-android`. A web version of an app may be automated by a separate
site operation crate using this browser driver; a native mobile app requires a
different platform driver and operation layer.
