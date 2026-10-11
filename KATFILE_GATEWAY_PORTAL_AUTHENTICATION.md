`KATFILE_GATEWAY_PORTAL_AUTHENTICATION.md`

---

# KatFile Gateway — Rust Authentication & Direct Link Implementation Plan

> **Document Type:** AI Coding Agent Implementation Specification
> **Version:** 1.0
> **Date:** 2026-10-11
> **Language:** Rust
> **Target Platform:** Linux ARM64 / AMD64
> **Primary Deployment:** Vultr VPS
> **Project Status:** POC validated; Rust production implementation pending

---

# 1. Project Objective

## 1.1 Overview

Implement a Rust-based KatFile Gateway capable of:

1. Automatically obtaining a valid KatFile authentication session.
2. Maintaining and renewing the session when required.
3. Resolving KatFile file-page URLs into direct-download URLs.
4. Returning direct-download URLs to authorized gateway clients.
5. Avoiding proxying file content whenever direct HTTP redirects are supported.
6. Operating on a resource-constrained Linux VPS.

The gateway must not require a continuously running graphical browser.

## 1.2 Primary Requirement

**The VPS must support unattended acquisition and renewal of KatFile authentication sessions.**

The system must not depend on scheduled manual browser login as its normal operating procedure.

An interactive recovery mechanism may exist, but it is a fallback and does not satisfy the primary requirement.

An automated verification approach must be confirmed to work reliably in the deployed environment before this requirement is marked complete. Do not treat successful CDP input dispatch as proof that verification succeeded.

## 1.3 Deployment Environment

Existing infrastructure:

| Resource | Specification |
|---|---|
| Provider | Vultr |
| CPU | 1 vCPU |
| RAM | 1 GB |
| Storage | 25 GB SSD |
| OS | Linux |
| Existing Service | WireGuard VPN |
| Additional Services | Rust Worker, SFTPGo, Caddy, SQLite |

Deployment constraints:

- Avoid long-running Chromium processes.
- Limit Chromium concurrency to one instance.
- Avoid keeping multiple browser profiles active simultaneously.
- Avoid large file buffering.
- Avoid downloading full file content through the gateway when unnecessary.
- Ensure authentication activity does not destabilize WireGuard.

The original POC used Docker Chromium on macOS. On 2026-10-11, one native headful Chromium authentication run on the Vultr VPS passed login, session acquisition, and Direct Link resolution. See [VPS POC results](docs/KATFILE_VPS_POC_RESULTS.md). Repeatability and production renewal remain unverified.

---

# 2. Verified POC Results

This section records actual experimental results.

**Do not repeat completed experiments unless validating the Rust implementation or investigating an observed regression.**

## 2.1 Chromium Environment

Successful browser configuration:

```bash
DISPLAY=:99 chromium \
  --no-sandbox \
  --disable-dev-shm-usage \
  --no-first-run \
  --no-default-browser-check \
  --remote-debugging-port=9227 \
  --user-data-dir=/tmp/chromium-cdp-click-test \
  --window-size=1280,900 \
  https://katfile.biz/login.html
```

Environment:

```text
Alpine Linux 3.23
Chromium 149.0.7827.53
Xvfb
Chrome DevTools Protocol
Python websocket-client
```

Observed browser properties:

```json
{
  "userAgent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36",
  "platform": "Linux x86_64",
  "webdriver": false,
  "languages": ["en-US", "en"],
  "hardwareConcurrency": 12
}
```

The JavaScript-reported architecture is not necessarily the physical CPU architecture.

Do not modify User-Agent or browser fingerprint values without an independently justified compatibility requirement.

### Headless Experiment

Headless Chromium 149 was successfully launched:

```bash
chromium \
  --headless \
  --no-sandbox \
  --disable-dev-shm-usage \
  --no-first-run \
  --no-default-browser-check \
  --remote-debugging-port=9228 \
  --user-data-dir=/tmp/chromium-headless-test \
  --window-size=1280,900 \
  https://katfile.biz/login.html
```

CDP returned:

```text
Browser: Chrome/149.0.7827.53
Protocol-Version: 1.3
```

Page inspection returned:

```json
{
  "title": "KatFile - Free Cloud Storage",
  "widgetPresent": true,
  "tokenLength": 0
}
```

**Conclusion:**

Headless Chromium can launch and load the KatFile login page.

**Not yet established:**

- Whether it can complete the required verification in the target environment.
- Whether it is more reliable than headful Chromium.
- Whether it significantly reduces memory consumption.

The production browser mode must therefore be configurable:

```toml
[browser]
mode = "headless"
```

Supported modes:

```text
headless
headful
```

---

## 2.2 CDP Connectivity

CDP discovery endpoint:

```http
GET http://127.0.0.1:9227/json/version
```

Example response:

```json
{
  "Browser": "Chrome/149.0.7827.53",
  "Protocol-Version": "1.3",
  "webSocketDebuggerUrl": "ws://127.0.0.1:9227/devtools/browser/..."
}
```

Page discovery:

```http
GET http://127.0.0.1:9227/json/list
```

CDP communication uses JSON messages over WebSocket.

Successfully tested commands:

| CDP Command | Result |
|---|---|
| `Runtime.evaluate` | PASS |
| `Storage.getCookies` | PASS |
| `Network.getCookies` | PASS |
| `Input.dispatchMouseEvent` | PASS |
| `Network.enable` | PASS |

The Rust implementation should use CDP directly without depending on FlareSolverr.

---

# 3. Turnstile Verification POC

## 3.1 Login Page Structure

KatFile login page:

```text
https://katfile.biz/login.html
```

Relevant form element:

```html
<div class="cf-turnstile">
    ...
</div>
```

Verification response field:

```html
<input
    type="hidden"
    name="cf-turnstile-response"
>
```

The response field is part of the login form.

## 3.2 Verification State Detection

The following JavaScript was successfully used through CDP:

```javascript
(() => {
    const token = document.querySelector(
        'input[name="cf-turnstile-response"]'
    );

    return {
        widgetPresent: !!document.querySelector('.cf-turnstile'),
        tokenPresent: !!token?.value,
        tokenLength: token?.value.length ?? 0
    };
})()
```

Before verification:

```json
{
  "widgetPresent": true,
  "tokenPresent": false,
  "tokenLength": 0
}
```

After successful interactive verification:

```json
{
  "widgetPresent": true,
  "tokenPresent": true,
  "tokenLength": 752
}
```

A CDP-based monitor detected the change automatically.

Observed output:

```text
Waiting for Turnstile verification...
Turnstile token detected!
Token length: 752
State: VerificationCompleted
```

### Important Distinction

A non-empty token means only that the browser has obtained a response.

It does not prove:

- The token is still valid.
- The token has not been used.
- KatFile will accept the login request.
- Authentication has succeeded.

Only a successful KatFile login response and subsequent authenticated operation establish those later states.

## 3.3 Token Lifetime

Cloudflare Turnstile tokens are generally:

```text
Lifetime: 300 seconds
Successful verification uses: one
```

Implementation requirements:

- Do not persist verification tokens.
- Do not log tokens.
- Do not reuse tokens after a login attempt.
- Avoid unnecessary operations after acquiring a token.
- Expire locally held tokens conservatively.
- Restart the normal verification process if a token expires.

## 3.4 CDP Interaction Experiment

A separate user-conducted experiment used CDP to:

- Locate the `.cf-turnstile` widget.
- Scroll the widget into view.
- Obtain its viewport coordinates.
- Dispatch mouse input events.

The user reported that this procedure successfully obtained Turnstile verification.

**Evidence classification: USER-REPORTED SUCCESS.**

The shared script itself did not include a token check or an authenticated HTTP request.

Therefore, the production implementation must not assume that sending input events guarantees verification.

The native VPS run on 2026-10-11 independently confirmed this sequence once:

```text
Verification response available
          ↓
KatFile HTTP login accepted
          ↓
New xfss received
          ↓
Direct Link resolved
```

Any verification interaction must operate within the website's supported authentication requirements; do not add mechanisms intended to defeat anti-automation protections.

## 3.5 FlareSolverr-Go2 Findings

Tested versions:

```text
Chromium 136
Chromium 149
```

FlareSolverr failed to complete Turnstile verification in the tested configurations.

Representative log:

```text
Found Turnstile checkbox via full DOM tree scan
Successfully clicked Turnstile checkbox via shadow DOM traversal
Performed humanized click on Turnstile widget
Performed humanized positional click
Turnstile solve attempt failed
context deadline exceeded
```

In some previous experiments, FlareSolverr reported a challenge as solved without obtaining a valid KatFile login session.

### Design Decision

Do not depend on FlareSolverr's generic challenge-success result.

Use KatFile-specific success conditions.

FlareSolverr is not a required production dependency.

---

# 4. KatFile Authentication Protocol

## 4.1 Login Form

Verified form metadata:

```json
{
  "action": "https://katfile.biz/",
  "method": "post",
  "enctype": "application/x-www-form-urlencoded"
}
```

Form fields:

| Name | Type | Notes |
|---|---|---|
| `op` | hidden | Value observed as five characters |
| `token` | hidden | Empty in tested form |
| `rand` | hidden | Dynamic value, 39 characters |
| `redirect` | hidden | Empty in tested form |
| `login` | text | Username |
| `password` | password | Password |
| `cf-turnstile-response` | hidden | Verification token |
| `submit` | submit | Form submit control |

Do not hardcode transient form values.

Extract the current form data from the browser.

The `rand` field was observed changing after page refresh:

```text
Initial SHA256 prefix:
7f18eb83fcbf

Later SHA256 prefix:
ef916f8484c4
```

This establishes that `rand` is not universally constant.

Its exact server-side purpose remains unknown.

## 4.2 Authentication Architecture

Use a Hybrid Authentication design:

```text
Chromium
    │
    ├── Load login page
    ├── Complete supported verification flow
    └── Expose current form state
              │
              ▼
         CDP Client
              │
              ├── Extract form fields
              └── Extract relevant cookies
              │
              ▼
        Rust HTTP Client
              │
              │ POST https://katfile.biz/
              │ application/x-www-form-urlencoded
              ▼
           KatFile
              │
              │ HTTP 302
              │ Location: /recommended.html
              │ Set-Cookie: xfss=...
              │ Set-Cookie: login=...
              ▼
        SessionManager
```

Chromium does not need to submit the login form.

## 4.3 Verified HTTP Login Result

The Python POC successfully submitted the form outside Chromium.

Actual result:

```text
Login form: READY
Turnstile Token: PRESENT
Submitting HTTP POST...

HTTP Status: 302
Redirect Host: katfile.biz
Redirect Path: /recommended.html

Set-Cookie names: ['xfss', 'login']
xfss Set-Cookie: True
```

**Status: VERIFIED.**

This is strong evidence that the Hybrid Authentication design works.

### Implementation Requirements

The Rust HTTP client must:

1. Preserve relevant form fields.
2. Preserve form field ordering where possible.
3. Encode the request as `application/x-www-form-urlencoded`.
4. Include relevant browser cookies.
5. Use appropriate request origin and referrer context.
6. Disable automatic redirect following during login result inspection.
7. Capture all `Set-Cookie` headers.
8. Validate the authentication result.

Do not determine login success only from HTTP status `200`.

Expected successful response observed:

```http
HTTP/1.1 302 Found
Location: https://katfile.biz/recommended.html
Set-Cookie: xfss=...
Set-Cookie: login=...
```

Do not assume this exact redirect will remain unchanged forever.

---

# 5. Session Management

## 5.1 Verified Cookies

After successful login:

| Cookie | Purpose | Observed Expiry |
|---|---|---|
| `xfss` | Authenticated session candidate | Approximately 30 days |
| `login` | Persistent login-related cookie | Approximately 180 days |
| `lang` | Language | Session |
| `_ga` | Analytics | Long-lived |

The `xfss` Cookie was observed with:

```text
HttpOnly: true
Secure: true
```

## 5.2 Session Reuse

Chromium was closed and restarted using the same profile.

Result:

```text
KatFile remained logged in.
```

Observed `xfss` expiry remained unchanged:

```text
2026-11-09T15:57:07.186657+00:00
```

Therefore:

- Browser profiles can retain valid KatFile sessions across Chromium process restarts.
- Restarting Chromium did not extend the observed Cookie expiry.
- Browser persistence alone does not establish automatic session renewal.

## 5.3 Cookie Isolation Experiments

| Request | Cookies | Result |
|---|---|---|
| `GET /?op=my_files` | Full login session | Authenticated page indicators |
| `GET /?op=my_files` | Only `login` | Unauthenticated |
| `GET /login.html` | Only `login` | Login form shown |
| File Page GET | Only `xfss` | HTTP 302 Direct Link |
| File Page GET | None | HTTP 200, no Direct Link |

**Conclusion:**

`xfss` alone was sufficient to resolve the tested direct link.

`login` alone was not sufficient to restore authenticated access in the tested requests.

Do not assume that `login` can refresh `xfss`.

## 5.4 Session Renewal Experiments

Tested requests:

```text
GET /?op=my_files
GET /zr4x96pk1tt6/probe-b.txt.html
```

With a valid session, neither response included `Set-Cookie`.

Result:

```text
Set-Cookie names: []
```

**Conclusion:**

There is currently no experimental evidence that these requests refresh the client-side session expiry.

Do not implement an unverified keep-alive mechanism and assume it will prevent expiry.

## 5.5 Session Storage Requirements

Persist sessions securely.

Recommended design:

```text
SessionManager
    │
    ├── Encrypted session storage
    ├── Cookie expiry metadata
    ├── Last validation timestamp
    ├── Authentication status
    └── Reauthentication coordination
```

Requirements:

- Encrypt authentication cookies at rest.
- Restrict file permissions.
- Never include credentials or session values in logs.
- Do not expose cookies to API clients.
- Avoid storing Turnstile response tokens.
- Do not store login credentials in browser automation scripts.
- Keep encryption keys separate from the encrypted session database.
- Use secure secret injection for deployment.

SQLite can hold encrypted session records, but SQLite itself does not provide application-level encryption automatically.

---

# 6. Direct Link Resolution

## 6.1 Objective

Convert a KatFile file-page URL into a direct-download URL.

Example input:

```text
https://katfile.biz/zr4x96pk1tt6/probe-b.txt.html
```

Expected interaction:

```text
Rust DirectLinkResolver
        │
        │ GET File Page
        │ Cookie: xfss=...
        ▼
      KatFile
        │
        │ HTTP 302
        │ Location: https://s5085.katfile.biz/...
        ▼
   Direct Download URL
```

## 6.2 Verified Experiment

Authenticated request using only `xfss`:

```text
HTTP Status: 302
Redirect: True
Location present: True
Location Host: s5085.katfile.biz
Location Scheme: https
```

Anonymous request:

```text
HTTP Status: 200
Redirect: False
Location present: False
```

**Status: VERIFIED FOR THE TEST FILE.**

## 6.3 Resolver Implementation Requirements

Use `reqwest` with redirect following disabled:

```rust
use reqwest::{redirect::Policy, Client};

let client = Client::builder()
    .redirect(Policy::none())
    .build()?;
```

Resolution process:

```text
1. Validate input URL.
2. Verify allowed KatFile hostname.
3. Obtain a valid xfss session.
4. GET the KatFile file page.
5. Disable redirect following.
6. Require expected redirect status.
7. Extract Location.
8. Resolve relative Location if necessary.
9. Validate destination URL.
10. Return DirectLinkResult.
```

Recommended result structure:

```rust
pub struct DirectLinkResult {
    pub url: String,
    pub source_url: String,
    pub resolved_at: std::time::SystemTime,
}
```

Do not assume the returned URL's validity period is known.

### Security Requirements

Treat direct-download URLs as potentially sensitive bearer URLs.

The resolver must:

- Accept only approved source hostnames.
- Reject loopback, link-local, private and otherwise forbidden network destinations where relevant.
- Prevent SSRF through redirects or URL manipulation.
- Validate download redirect destinations against an appropriate policy.
- Use bounded timeouts.
- Limit response size.
- Avoid logging full direct-download URLs containing tokens.
- Never log `xfss`.

A redirect destination should not be accepted solely because its hostname contains the substring `katfile`.

---

# 7. Direct Download Experiment

## 7.1 Cookie-Free Download

After obtaining a Direct Link with `xfss`, an independent HTTP request was sent without Cookies.

Request:

```http
GET /...
Range: bytes=0-0
```

Result:

```text
Anonymous Download Status: 206
Content-Type: application/octet-stream
Content-Range: bytes 0-0/2097
Content-Disposition Present: True
```

**Conclusion:**

The tested direct-download URL could be used without KatFile authentication cookies.

## 7.2 Cross-IP Experiment

The Direct Link was resolved in the Docker test environment.

The URL was then passed through SSH to the Vultr VPS.

Test result:

```text
HTTP Status: 206
Downloaded Bytes: 1
Remote IP: 213.152.184.140
```

**Status: VERIFIED.**

The tested Direct Link was usable from a different network.

However, the following remain unverified:

- Direct Link lifetime.
- Behavior across different file sizes.
- Behavior after Session expiration.
- CDN-specific restrictions.
- Download concurrency limits.
- Rate limits.
- Whether all file categories behave identically.

---

# 8. Gateway Response Strategy

## 8.1 HTTP Gateway

For an authorized HTTP client:

```text
Client
   │
   ▼
Rust Gateway
   │
   │ Resolve Direct Link
   ▼
KatFile
   │
   │ 302 Location
   ▼
Rust Gateway
   │
   │ HTTP 302
   │ Location: Direct Link
   ▼
Client
   │
   ▼
KatFile Download Server
```

This is the preferred strategy when clients support redirects.

Advantages:

- Avoids downloading file content through the VPS.
- Reduces gateway bandwidth consumption.
- Avoids buffering large files.
- Supports a small VPS.

## 8.2 SFTP / WebDAV Considerations

The planned infrastructure includes SFTPGo.

Important:

**SFTP cannot directly return an HTTP 302 response.**

WebDAV clients also cannot universally be assumed to handle external download redirects.

Therefore, separate interfaces must be designed:

```text
HTTP API
    └── Direct Link / HTTP 302

WebDAV
    └── Compatible file-streaming or adapter mechanism

SFTP
    └── Compatible file-streaming backend
```

Do not claim the SFTPGo integration is finished merely because the HTTP Direct Link POC succeeded.

Any WebDAV/SFTP file streaming must have its own resource and bandwidth analysis.

---

# 9. Proposed Rust Architecture

## 9.1 Components

```text
                    KatFile Gateway
                           │
             ┌─────────────┴─────────────┐
             │                           │
        Gateway API                 SFTPGo Adapter
             │                           │
             └─────────────┬─────────────┘
                           │
                    DirectLinkResolver
                           │
                      SessionManager
                           │
                    AuthCoordinator
                           │
               ┌───────────┴───────────┐
               │                       │
         BrowserManager          HttpLoginClient
               │                       │
            Chromium                   │
               │                       │
            CdpClient                  │
               │                       │
         FormExtractor                 │
               │                       │
         Token Monitor                 │
               │                       │
               └───────────┬───────────┘
                           │
                        KatFile
```

## 9.2 Module Responsibilities

| Module | Responsibility |
|---|---|
| `BrowserManager` | Start/stop Chromium; manage profiles |
| `CdpClient` | CDP WebSocket commands and events |
| `FormExtractor` | Extract current login form fields |
| `VerificationMonitor` | Observe verification token availability |
| `HttpLoginClient` | Submit authentication form |
| `SessionManager` | Store and retrieve active sessions |
| `SessionValidator` | Confirm session is operational |
| `AuthCoordinator` | Coordinate authentication lifecycle |
| `DirectLinkResolver` | Resolve file pages into direct URLs |
| `GatewayApi` | Expose authenticated HTTP endpoints |

## 9.3 Recommended Dependencies

Suggested Rust crates:

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
tokio-tungstenite = "0.28"
futures-util = "0.3"
reqwest = { version = "0.13", features = ["json", "cookies", "form", "rustls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tracing = "0.1"
```

These versions are illustrative starting points, not a verified compatible lockfile.

The Coding Agent must verify current crate availability, features and compatibility before finalizing dependencies.

Additional components may require:

```text
axum
rusqlite
aes-gcm
secrecy
zeroize
url
```

Prefer a small dependency footprint.

---

# 10. CDP Client Design

## 10.1 Requirements

Implement:

- Browser discovery.
- Page discovery.
- WebSocket connection.
- Command ID generation.
- Response correlation.
- Event handling.
- Timeouts.
- Reconnection and process-exit detection.

CDP is asynchronous.

Commands and events may arrive interleaved.

Do not assume that the next received message corresponds to the last command sent.

## 10.2 Suggested Interface

```rust
pub struct CdpClient {
    // WebSocket connection
    // Pending request map
    // Event dispatcher
}

impl CdpClient {
    pub async fn connect(
        endpoint: &str,
    ) -> Result<Self, CdpError> {
        todo!()
    }

    pub async fn evaluate(
        &self,
        expression: &str,
    ) -> Result<serde_json::Value, CdpError> {
        todo!()
    }

    pub async fn get_cookies(
        &self,
    ) -> Result<Vec<BrowserCookie>, CdpError> {
        todo!()
    }
}
```

Suggested internal design:

```text
WebSocket Reader Task
        │
        ├── Response with ID
        │       │
        │       ▼
        │   Pending Request Map
        │
        └── Event without ID
                │
                ▼
            Event Dispatcher
```

The reader task must be the sole receiver for its WebSocket connection.

---

# 11. Authentication State Machine

Use explicit states.

```rust
pub enum AuthenticationState {
    Idle,
    CheckingSession,
    SessionValid,
    SessionExpired,
    StartingBrowser,
    LoadingLoginPage,
    WaitingForVerification,
    TokenAvailable,
    SubmittingLogin,
    SessionAcquired,
    ValidatingSession,
    SessionValidated,
    RetryScheduled,
    RequiresIntervention,
    Failed,
}
```

Recommended flow:

```text
Idle
 │
 ▼
CheckingSession
 │
 ├── Valid ───────────────────────► SessionValid
 │
 └── Invalid
        │
        ▼
   StartingBrowser
        │
        ▼
   LoadingLoginPage
        │
        ▼
   WaitingForVerification
        │
        │ Verification response available
        ▼
   TokenAvailable
        │
        ▼
   SubmittingLogin
        │
        ▼
   SessionAcquired
        │
        ▼
   ValidatingSession
        │
        ├── Valid ───────────────► SessionValidated
        │
        └── Invalid ─────────────► RetryScheduled / Failed
```

Important:

- `TokenAvailable` is not `SessionValidated`.
- `SessionAcquired` is not necessarily `SessionValidated`.
- Do not mark success solely because Chromium navigated away from the login page.
- Verification failures must be distinguishable from network failures.
- A successful gateway authentication requires a working session.

---

# 12. Session Validation

Use a known small test file:

```text
https://katfile.biz/zr4x96pk1tt6/probe-b.txt.html
```

Validation:

```text
GET File Page
Cookie: xfss=...
Redirect: disabled
```

Expected:

```text
HTTP 302
Location: approved KatFile download endpoint
```

This is a practical operational test, not a universal guarantee of account validity.

The test fixture must remain available and under the project's control.

If the test file disappears or its permissions change, that must not automatically be classified as session expiration.

The implementation should also recognize login-page responses and other explicit authentication failures.

---

# 13. Session Lifecycle

## 13.1 Normal Operation

```text
Incoming Request
      │
      ▼
SessionManager
      │
      ├── Session valid
      │       │
      │       ▼
      │   DirectLinkResolver
      │
      └── Session invalid
              │
              ▼
         AuthCoordinator
              │
              ▼
        Acquire New Session
              │
              ▼
         DirectLinkResolver
```

## 13.2 Renewal Strategy

The session manager should support:

- Validation on demand.
- Conservative expiry tracking.
- Coordinated reauthentication.
- Retry with exponential backoff.
- Separation of temporary errors from expired sessions.
- Proactive renewal before expected expiration where supported.
- Recovery after VPS restart.

Multiple simultaneous gateway requests must not launch multiple Chromium instances.

Use a single-flight authentication mechanism:

```text
Request A ─┐
Request B ─┼──► One Authentication Task
Request C ─┘
```

All waiting requests should receive a consistent result.

## 13.3 Persistent Browser Profiles

POC profiles were stored under:

```text
/tmp/chromium-...
```

Do not use container-local `/tmp` as the only production persistence mechanism.

Use a persistent, restricted directory or mounted volume.

Example:

```text
/var/lib/katfile-gateway/browser-profile/
```

Only the gateway service account should have access.

Browser profile persistence is useful for retaining valid sessions, but is not proof that expired sessions can be renewed automatically.

---

# 14. Reliability and Resource Management

## 14.1 Browser Lifecycle

The browser should normally be stopped.

```text
Authentication Required
          │
          ▼
     Start Chromium
          │
          ▼
     Authentication
          │
          ▼
      Obtain xfss
          │
          ▼
     Validate Session
          │
          ▼
     Persist Session
          │
          ▼
     Stop Chromium
```

## 14.2 VPS Constraints

The production environment contains only 1 GB RAM.

Requirements:

- Maximum one authentication browser.
- Browser startup timeout.
- Browser process cleanup.
- Memory monitoring.
- No unlimited retries.
- No persistent multi-browser pool.
- Avoid running Xvfb unless needed.
- Do not affect WireGuard availability.

A headless-first deployment is reasonable, but only after verifying the required authentication flow.

If headless mode cannot satisfy the authentication requirements, headful mode may be retained as a diagnostic or fallback option.

---

# 15. Suggested Repository Structure

```text
katfile-gateway/
│
├── Cargo.toml
├── Cargo.lock
│
├── crates/
│   │
│   ├── katfile-auth/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── coordinator.rs
│   │       ├── state.rs
│   │       ├── browser.rs
│   │       ├── cdp.rs
│   │       ├── form.rs
│   │       ├── verification.rs
│   │       ├── login.rs
│   │       └── session.rs
│   │
│   ├── katfile-client/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── resolver.rs
│   │       └── error.rs
│   │
│   └── gateway-api/
│       └── src/
│           ├── lib.rs
│           └── routes.rs
│
├── apps/
│   └── gateway-worker/
│       └── src/
│           └── main.rs
│
├── tests/
│   ├── authentication.rs
│   ├── session.rs
│   ├── direct_link.rs
│   └── integration.rs
│
├── config/
│   └── example.toml
│
├── deploy/
│   ├── Dockerfile
│   └── compose.yaml
│
└── docs/
    ├── POC_RESULTS.md
    └── AUTHENTICATION.md
```

Adapt this structure to the existing repository instead of creating redundant modules.

---

# 16. Implementation Phases

## Phase 1 — Rust DirectLinkResolver

**Priority: Highest**

Implement:

- KatFile URL validation.
- Session Cookie support.
- HTTP GET with redirects disabled.
- HTTP 302 Location extraction.
- Download endpoint validation.
- Structured errors.
- Unit tests with mocked HTTP responses.

### Acceptance Criteria

Given a valid `xfss`:

```text
Input:
https://katfile.biz/zr4x96pk1tt6/probe-b.txt.html
```

Result:

```text
HTTP 302 destination extracted successfully
```

Must not download the full file.

---

## Phase 2 — Rust HTTP Login Client

Implement:

- Form URL-encoded POST.
- Session Cookie handling.
- Redirect inspection.
- `Set-Cookie` parsing.
- `xfss` extraction.
- Authentication error handling.

### Acceptance Criteria

Given a legitimately obtained, unexpired verification response and current form data:

```text
POST Login
    ↓
HTTP 302
    ↓
xfss received
```

Do not store raw login credentials or verification tokens in logs.

---

## Phase 3 — Chromium + CDP Integration

Implement:

- Chromium lifecycle management.
- CDP discovery.
- WebSocket commands.
- Login page navigation.
- Form extraction.
- Verification-result monitoring.
- Browser cleanup.

### Acceptance Criteria

The Rust implementation reproduces the CDP capabilities confirmed in the Python POC.

This phase must not be treated as proof of unattended verification.

---

## Phase 4 — Authentication Orchestration

Integrate:

```text
Browser
    ↓
Verification
    ↓
Form Extraction
    ↓
HTTP Login
    ↓
xfss
    ↓
Session Validation
```

### Acceptance Criteria

Complete one end-to-end authentication operation and use the newly obtained `xfss` to resolve the test file.

The end-to-end test must use the **new session produced by the current login operation**, not an older Cookie from another browser profile.

---

## Phase 5 — Unattended Authentication

**Mandatory production milestone.**

Investigate and implement a supported authentication provider that can obtain a new KatFile session without human intervention.

Requirements:

- No pre-existing valid `xfss`.
- No manual browser login.
- No manual secret copying.
- No dependence on a permanently running graphical browser.
- Successful KatFile session acquisition.
- Successful Direct Link validation.
- Reliable error reporting when unattended authentication is unavailable.

One Python run on Vultr acquired a fresh session without manual browser interaction and resolved the test file. This milestone still requires repeatable deployment tests and production integration; initial secret provisioning was manual.

Do not claim unattended authentication is production-ready until repeatable deployment tests establish it.

---

## Phase 6 — VPS Deployment

Deploy with the existing infrastructure:

```text
Caddy
SFTPGo
Rust Worker
SQLite
WireGuard
```

Ensure HTTP download redirects are implemented independently from SFTP/WebDAV file streaming.

### Acceptance Criteria

- Services start after VPS reboot.
- Session storage persists.
- The worker does not interfere with WireGuard.
- Resource usage remains within configured limits.
- Authentication failures do not cause unbounded process creation.
- The gateway can recover from temporary network failures.
- Direct Link Resolution works from the actual VPS.

---

# 17. Required Tests

## 17.1 Unit Tests

Test:

- URL parsing and validation.
- Form encoding.
- Redirect extraction.
- Cookie parsing.
- Expiry calculation.
- Authentication state transitions.
- Retry behavior.
- CDP response correlation.

## 17.2 Integration Tests

Test:

- CDP connectivity.
- Login form extraction.
- HTTP Login.
- Session persistence.
- Session validation.
- Direct Link Resolution.

Use mocked HTTP responses for deterministic tests.

## 17.3 End-to-End Tests

Required scenarios:

| Scenario | Expected Result |
|---|---|
| Valid `xfss` | Direct Link resolves |
| Missing `xfss` | Authentication required |
| Invalid `xfss` | Session marked invalid |
| Valid login response | New session stored |
| Token expired | Authentication attempt fails safely |
| Chromium exits unexpectedly | No orphan processes |
| Network timeout | Bounded retry |
| KatFile returns 200 instead of 302 | Resolver reports unexpected response |
| Malicious redirect destination | Redirect rejected |
| Concurrent authentication requests | Single authentication operation |
| VPS reboot | Persistent state restored |

---

# 18. Logging and Security

Use structured logging through `tracing`.

Recommended event names:

```text
auth.browser.starting
auth.browser.started
auth.verification.waiting
auth.verification.token_available
auth.login.started
auth.login.succeeded
auth.login.failed
auth.session.acquired
auth.session.validated
auth.session.expired

resolver.started
resolver.redirect_received
resolver.completed
resolver.failed
```

Never log:

```text
KatFile Password
Turnstile Token
xfss Cookie Value
login Cookie Value
Full Direct Download URL containing tokens
CDP Messages containing credentials
```

CDP debugging endpoints must be accessible only to trusted local processes.

Do not publish debugging ports to the Internet.

Authentication credentials and Cookies must be treated as secrets.

---

# 19. Known Limitations and Open Questions

The following must remain explicitly documented.

| Issue | Current Status |
|---|---|
| Rust implementation | Not yet completed |
| Headless authentication success | Not established |
| Unattended authentication on Vultr | One Python end-to-end run passed; repeatability pending |
| `xfss` renewal without login | Not established |
| Direct Link lifetime | Unknown |
| Direct Link cross-IP usability | Confirmed for one test |
| Session persistence across browser restart | Confirmed |
| HTTP Client login outside Chromium | Confirmed |
| Token monitoring via CDP | Confirmed |
| Automatic verification via CDP | One native VPS end-to-end run confirmed token, login, session and redirect |
| SFTPGo integration | Pending |
| WebDAV compatibility | Pending |
| Actual VPS memory consumption | Before/after snapshots recorded; peak and VPN performance pending |

---

# 20. Instructions for AI Coding Agent

You are implementing a Rust-based KatFile Gateway.

The experimental findings documented above are the technical baseline.

Follow these rules:

**1. Do not redesign the project around FlareSolverr.**

Use native Chromium and a minimal CDP client.

**2. Do not repeat completed Python experiments without a clear reason.**

Port the confirmed behavior into Rust.

**3. Prioritize DirectLinkResolver and HTTP Login Client.**

These components have the strongest experimental evidence and are relatively independent of the browser.

**4. Keep authentication providers replaceable.**

The system must support future changes to the supported authentication mechanism without rewriting SessionManager or DirectLinkResolver.

**5. Never confuse browser interaction with authentication success.**

Require a valid KatFile session and a successful authenticated operation.

**6. Design for a 1 GB RAM VPS.**

Chromium must not become a permanent background dependency during normal download operations.

**7. Preserve security boundaries.**

Do not expose sessions or direct-download tokens to unauthorized users.

**8. Do not mark the project complete until unattended authentication is verified.**

This is an explicit user requirement.

**9. Implement incrementally.**

Each completed phase must compile, include relevant tests, and have a clear acceptance result.

**10. Report blockers accurately.**

If a component cannot be verified, document it as incomplete rather than implementing a speculative workaround.

---

# 21. Definition of Done

The overall project is complete only when the following workflow operates on the Vultr VPS:

```text
                    Gateway Client
                          │
                          │ Request File
                          ▼
                    Rust Gateway
                          │
                          ▼
                    SessionManager
                          │
                    Valid xfss?
                     │         │
                    YES        NO
                     │         │
                     │         ▼
                     │    AuthCoordinator
                     │         │
                     │    Acquire Session
                     │         │
                     │    No Human Input
                     │         │
                     │         ▼
                     │    New Valid xfss
                     │         │
                     └─────────┘
                          │
                          ▼
                   DirectLinkResolver
                          │
                          │ GET KatFile File Page
                          │ Cookie: xfss
                          ▼
                        KatFile
                          │
                          │ HTTP 302
                          ▼
                     Direct Link
                          │
                          ▼
                    Gateway HTTP 302
                          │
                          ▼
                         Client
                          │
                          │ GET without xfss
                          ▼
                 KatFile Download Server
                          │
                          ▼
                      File Content
```

**Final acceptance criteria:**

The gateway must be able to resolve KatFile file pages into working direct-download URLs while managing authentication sessions automatically.

An authenticated HTTP 302 response alone is not sufficient unless the returned download URL is usable.

A working DirectLinkResolver alone is not sufficient unless the gateway can also meet its unattended session-acquisition requirement.
