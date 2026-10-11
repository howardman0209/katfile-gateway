#!/usr/bin/env bash
set -euo pipefail
# Historical macOS Docker PoC; requires the existing flaresolverr-149 container.
python3 - <<'PY'
import json
import subprocess
from pathlib import Path

SECRET = Path.home() / ".config" / "katfile" / "secret"
CONTAINER = "flaresolverr-149"

lines = SECRET.read_text().splitlines()

if len(lines) != 2 or not all(lines):
    raise SystemExit("Secret must contain exactly two lines")

credentials = json.dumps({
    "username": lines[0],
    "password": lines[1],
})

script = r'''
import json
import os
import time
import random
import subprocess
import tempfile
import shutil
import signal
import urllib.error
import urllib.request
import urllib.parse
import websocket

credentials = json.loads(input())

PORT = 9233
CDP = f"http://127.0.0.1:{PORT}"

LOGIN = "https://katfile.biz/login.html"
TEST_FILE = "https://katfile.biz/zr4x96pk1tt6/probe-b.txt.html"
COOKIE_JAR = "/tmp/katfile-auth-session.cookies"

# Turnstile click settings
CLICK_TARGET = "checkbox"   # "checkbox" = left checkbox (x+30); "center" = widget center
RENDER_SETTLE = 3           # wait for iframe content after the widget appears
CLICK_INTERVAL = 20         # minimum interval between clicks (seconds)
MAX_CLICKS = 5              # maximum click attempts
TOKEN_TIMEOUT = 300         # total token wait timeout (seconds)

# ============================================================
# 1. Start Chromium
# ============================================================

env = os.environ.copy()
env["DISPLAY"] = ":99"

# Dedicated profile: never reuse an earlier headless/headful instance.
profile_dir = tempfile.mkdtemp(prefix="katfile-chromium-")
log_path = "/tmp/katfile-chromium.log"
browser = None

try:
    # Do not attach to an unrelated Chromium already using this CDP port.
    try:
        urllib.request.urlopen(f"{CDP}/json/version", timeout=1).close()
    except urllib.error.URLError:
        pass
    else:
        raise RuntimeError(f"CDP port {PORT} is already occupied")

    with open(log_path, "w") as log:
        browser = subprocess.Popen(
            [
                "chromium",
                "--no-sandbox",
                "--disable-dev-shm-usage",
                "--no-first-run",
                "--no-default-browser-check",
                f"--remote-debugging-port={PORT}",
                f"--user-data-dir={profile_dir}",
                "--window-size=1280,900",
                LOGIN,
            ],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )

    print("[1] Chromium started", flush=True)

    # ============================================================
    # 2. Discover CDP page
    # ============================================================

    page = None

    for _ in range(60):
        if browser.poll() is not None:
            raise RuntimeError(
                f"Chromium exited with code {browser.returncode}; inspect {log_path}"
            )
        try:
            with urllib.request.urlopen(
                f"{CDP}/json/list",
                timeout=2,
            ) as response:
                targets = json.load(response)

            page = next(
                t for t in targets
                if t.get("type") == "page"
                and "katfile.biz" in t.get("url", "")
            )
            break

        except Exception:
            time.sleep(0.5)

    if page is None:
        raise RuntimeError(f"CDP target not found; inspect {log_path}")

    ws = websocket.create_connection(
        page["webSocketDebuggerUrl"],
        timeout=10,
        suppress_origin=True,
    )

    counter = 0

    def cdp(method, params=None):
        global counter

        counter += 1
        request_id = counter

        ws.send(json.dumps({
            "id": request_id,
            "method": method,
            "params": params or {},
        }))

        while True:
            response = json.loads(ws.recv())

            if response.get("id") != request_id:
                continue

            if "error" in response:
                raise RuntimeError(
                    f"CDP {method}: {response['error']}"
                )

            return response.get("result", {})


    def evaluate(js):
        response = cdp("Runtime.evaluate", {
            "expression": js,
            "returnByValue": True,
            "awaitPromise": True,
        })

        if "exceptionDetails" in response:
            details = response["exceptionDetails"]
            exception = details.get("exception", {})

            print("JavaScript Exception:", flush=True)
            print("Text:", details.get("text"), flush=True)
            print("Line:", details.get("lineNumber"), flush=True)
            print("Column:", details.get("columnNumber"), flush=True)
            print(
                "Exception class:",
                exception.get("className"),
                flush=True,
            )

            raise RuntimeError("JavaScript evaluation failed")

        return response.get("result", {}).get("value")


    # ============================================================
    # Turnstile helpers
    # ============================================================

    def widget_rect():
        """Scroll into view and return widget viewport coordinates, or None if not rendered."""
        rect = evaluate("""
            (() => {
                const w = document.querySelector('.cf-turnstile');
                if (!w) return null;

                w.scrollIntoView({block: 'center', inline: 'center'});
                const r = w.getBoundingClientRect();

                return {x: r.x, y: r.y, width: r.width, height: r.height};
            })()
        """)

        # Unrendered widgets have zero dimensions; rendered widgets are usually about 300x65.
        if not rect or rect["width"] < 50 or rect["height"] < 40:
            return None

        return rect


    def mouse(event_type, x, y, **extra):
        cdp("Input.dispatchMouseEvent", {
            "type": event_type,
            "x": x,
            "y": y,
            **extra,
        })


    def click_turnstile(rect):
        if CLICK_TARGET == "center":
            x = rect["x"] + rect["width"] / 2
        else:
            x = rect["x"] + min(30, rect["width"] / 2)

        y = rect["y"] + rect["height"] / 2

        x += random.uniform(-2, 2)
        y += random.uniform(-2, 2)

        # Move from nearby, then press and release.
        mouse("mouseMoved", x - random.uniform(40, 80), y - random.uniform(10, 30))
        time.sleep(random.uniform(0.1, 0.3))
        mouse("mouseMoved", x, y)
        time.sleep(random.uniform(0.05, 0.15))

        mouse("mousePressed", x, y, button="left", buttons=1, clickCount=1)
        time.sleep(random.uniform(0.05, 0.12))
        mouse("mouseReleased", x, y, button="left", buttons=0, clickCount=1)

        return x, y


    try:
        cdp("Page.bringToFront")

        # ========================================================
        # 3. Wait for login form (credentials stay out of the browser)
        # ========================================================

        for _ in range(30):
            if evaluate(
                "!!document.querySelector('form:has(input[name=\"login\"])')"
            ):
                break
            time.sleep(1)
        else:
            raise RuntimeError("Login form not found")

        print("[2] Login form loaded", flush=True)

        # ========================================================
        # 4. Inspect verification state
        # ========================================================

        state = evaluate("""
            (() => ({
                title: document.title,
                widgetPresent: !!document.querySelector(
                    '.cf-turnstile'
                ),
                tokenLength: document.querySelector(
                    'input[name="cf-turnstile-response"]'
                )?.value.length ?? 0
            }))()
        """)

        print("[3] Page state:", json.dumps(state), flush=True)

        # ========================================================
        # 5. Click Turnstile & wait for token
        # ========================================================

        js_form_state = """
            (() => {
                const form = document.querySelector(
                    'form:has(input[name="login"])'
                );

                if (!form) return null;

                const fields = Array.from(
                    new FormData(form).entries()
                );

                const values = Object.fromEntries(fields);

                const token = document.querySelector(
                    'input[name="cf-turnstile-response"]'
                );

                if (token && !values["cf-turnstile-response"]) {
                    fields.push([
                        "cf-turnstile-response",
                        token.value
                    ]);
                }

                const submit = form.querySelector(
                    '[type="submit"][name]'
                );

                if (submit && !(submit.name in values)) {
                    fields.push([submit.name, submit.value]);
                }

                return {
                    action: form.action,
                    fields,
                    tokenReady: !!token?.value
                };
            })()
        """

        form_data = None
        started = time.time()
        widget_seen_at = None
        last_click = None
        clicks = 0
        last_log = -1

        while time.time() - started < TOKEN_TIMEOUT:
            now = time.time()
            elapsed = int(now - started)

            state = evaluate(js_form_state)

            if state and state["tokenReady"]:
                form_data = state
                break

            rect = widget_rect()

            if rect is None:
                widget_seen_at = None
            else:
                if widget_seen_at is None:
                    widget_seen_at = now
                    print(
                        f"Turnstile rendered: {rect['width']:.0f}x{rect['height']:.0f}",
                        flush=True,
                    )

                ready_to_click = (
                    now - widget_seen_at >= RENDER_SETTLE
                    and clicks < MAX_CLICKS
                    and (last_click is None or now - last_click >= CLICK_INTERVAL)
                )

                if ready_to_click:
                    x, y = click_turnstile(rect)
                    clicks += 1
                    last_click = now
                    print(
                        f"Turnstile click #{clicks} at ({x:.1f}, {y:.1f})",
                        flush=True,
                    )

            if elapsed % 15 == 0 and elapsed != last_log:
                last_log = elapsed
                print(
                    f"Waiting for verification: {elapsed}s",
                    flush=True,
                )

            time.sleep(1)

        if form_data is None:
            raise RuntimeError(
                f"Verification token not available after {clicks} click(s)"
            )

        print(
            f"[4] Verification token available ({clicks} click(s))",
            flush=True,
        )

        # ========================================================
        # 6. Extract browser cookies
        # ========================================================

        action = form_data["action"]
        parsed = urllib.parse.urlsplit(action)

        if parsed.scheme != "https" or parsed.hostname != "katfile.biz":
            raise RuntimeError("Unexpected form action")

        cookies = cdp("Network.getCookies", {
            "urls": [
                "https://katfile.biz/",
                LOGIN,
            ]
        }).get("cookies", [])

        # Use browser token and hidden fields; add credentials only here.
        fields = [
            tuple(field) for field in form_data["fields"]
            if field[0] not in ("login", "password")
        ]
        fields += [
            ("login", credentials["username"]),
            ("password", credentials["password"]),
        ]

        payload = urllib.parse.urlencode(fields).encode()

        # ========================================================
        # 7. Initialize cookie jar
        # ========================================================

        fd, initial_jar = tempfile.mkstemp(
            prefix="katfile-initial-"
        )

        os.chmod(initial_jar, 0o600)

        try:
            with os.fdopen(fd, "w") as f:
                f.write("# Netscape HTTP Cookie File\n")

                for c in cookies:
                    domain = c["domain"]

                    if c.get("httpOnly"):
                        domain = "#HttpOnly_" + domain

                    expiry = max(
                        0,
                        int(c.get("expires", 0))
                    )

                    f.write("\t".join([
                        domain,
                        "TRUE" if c["domain"].startswith(".") else "FALSE",
                        c.get("path", "/"),
                        "TRUE" if c.get("secure") else "FALSE",
                        str(expiry),
                        c["name"],
                        c["value"],
                    ]) + "\n")

            subprocess.run(
                ["cp", initial_jar, COOKIE_JAR],
                check=True,
            )

            os.chmod(COOKIE_JAR, 0o600)

            # ====================================================
            # 8. curl login
            # ====================================================

            print("[5] Submitting curl login", flush=True)

            result = subprocess.run(
                [
                    "curl",
                    "-sS",
                    "--max-time", "30",
                    "--cookie", COOKIE_JAR,
                    "--cookie-jar", COOKIE_JAR,
                    "--header",
                    "Content-Type: application/x-www-form-urlencoded",
                    "--header",
                    "Origin: https://katfile.biz",
                    "--referer", LOGIN,
                    "--data-binary", "@-",
                    "--output", "/dev/null",
                    "--write-out", "%{http_code} %{redirect_url}",
                    action,
                ],
                input=payload,
                capture_output=True,
                check=True,
            )

            output = result.stdout.decode().strip()
            status, _, redirect = output.partition(" ")

            destination = urllib.parse.urlsplit(redirect)

            print("Login HTTP status:", status)
            print("Login redirect:", destination.path)

            if status != "302":
                raise RuntimeError("Login did not return HTTP 302")

            if (
                destination.hostname != "katfile.biz"
                or destination.path != "/recommended.html"
            ):
                raise RuntimeError("Unexpected login redirect")

            # ====================================================
            # 9. Check xfss
            # ====================================================

            xfss_found = False

            with open(COOKIE_JAR) as f:
                for line in f:
                    if (
                        line.startswith("#")
                        and not line.startswith("#HttpOnly_")
                    ):
                        continue

                    parts = line.rstrip("\n").split("\t")

                    if (
                        len(parts) >= 7
                        and parts[5] == "xfss"
                        and parts[6]
                    ):
                        xfss_found = True
                        break

            if not xfss_found:
                raise RuntimeError("xfss cookie not found")

            print("[6] xfss acquired", flush=True)

            # ====================================================
            # 10. Resolve Direct Link
            # ====================================================

            resolved = subprocess.run(
                [
                    "curl",
                    "-sS",
                    "--max-time", "30",
                    "--cookie", COOKIE_JAR,
                    "--output", "/dev/null",
                    "--write-out", "%{http_code} %{redirect_url}",
                    TEST_FILE,
                ],
                capture_output=True,
                check=True,
            )

            direct_status, _, direct_url = (
                resolved.stdout.decode().strip().partition(" ")
            )

            direct = urllib.parse.urlsplit(direct_url)

            print("[7] Direct Link status:", direct_status)
            print("Direct Link host:", direct.hostname)
            print("Direct Link acquired:", bool(direct_url))

            if direct_status != "302" or not direct_url:
                raise RuntimeError("Direct Link resolution failed")

            print("SUCCESS")
            print("Cookie jar:", COOKIE_JAR)

        finally:
            os.unlink(initial_jar)

    finally:
        ws.close()
finally:
    # Terminate the entire Chromium session/process group (including children).
    if browser is not None:
        try:
            os.killpg(browser.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            browser.wait(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(browser.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            browser.wait()
    shutil.rmtree(profile_dir, ignore_errors=True)

'''

result = subprocess.run(
    [
        "docker",
        "exec",
        "-i",
        CONTAINER,
        "python3",
        "-c",
        script,
    ],
    input=credentials + "\n",
    text=True,
)

raise SystemExit(result.returncode)
PY
