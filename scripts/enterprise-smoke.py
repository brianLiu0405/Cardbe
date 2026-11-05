#!/usr/bin/env python3
"""Exercise the enterprise prototype in a real WebKitGTK MiniBrowser.

Requires already-running WebKitWebDriver, Xvfb, the enterprise service and Vite.
Uses only Python's standard library. All test records are new timestamped fixtures;
use an isolated prototype database. This script never starts or stops services.
Credentials stay in memory; screenshots exclude credential forms and dialogs.

Example (from the repository root):
  python3 scripts/enterprise-smoke.py --driver-url http://127.0.0.1:9515 \
    --base-url http://127.0.0.1:1441 --credentials /private/owner.json \
    --output /tmp/cardbe-enterprise-smoke
"""

import argparse
import base64
import datetime as dt
import json
from pathlib import Path
import sys
import time
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener


ELEMENT_KEY = "element-6066-11e4-a52e-4f735466cecf"
DISCONNECT = ["Disconnect", "中斷連線"]
REFRESH = ["Refresh from service", "從服務端重新載入"]


class SmokeError(RuntimeError):
    """Messages must describe an operation without including response bodies."""


class NoRedirect(HTTPRedirectHandler):
    # A misconfigured local proxy must never forward a Bearer credential elsewhere.
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def local_url(value):
    parsed = urlsplit(value)
    if (
        parsed.scheme != "http"
        or parsed.hostname not in ("127.0.0.1", "localhost", "::1")
        or parsed.username is not None
        or parsed.password is not None
        or parsed.path not in ("", "/")
        or parsed.query
        or parsed.fragment
    ):
        raise argparse.ArgumentTypeError("Use a loopback HTTP origin without credentials or a path")
    try:
        if not parsed.port:
            raise ValueError
    except ValueError as error:
        raise argparse.ArgumentTypeError("An explicit valid port is required") from error
    return value.rstrip("/")


def require(condition, description):
    if not condition:
        raise SmokeError(description)


FIND = r"""
const s = arguments[0];
const norm = value => (value || '').replace(/\s+/g, ' ').trim();
const visible = e => e.getClientRects().length && getComputedStyle(e).visibility !== 'hidden';
let root = document;
if (s.heading) {
  const heading = [...document.querySelectorAll('h2')].find(e => norm(e.textContent) === s.heading);
  if (!heading) return null;
  root = heading.parentElement;
  while (root && ![...root.querySelectorAll(s.selector || 'button')].some(e =>
    visible(e) && (!s.text || s.text.includes(norm(e.innerText || e.textContent))))) root = root.parentElement;
  if (!root) return null;
}
for (const e of root.querySelectorAll(s.selector || 'button')) {
  if (!visible(e) || e.disabled || e.getAttribute('aria-disabled') === 'true') continue;
  const text = norm(e.innerText || e.textContent);
  if (s.text && !s.text.includes(text)) continue;
  if (s.contains && !text.includes(s.contains)) continue;
  return e;
}
return null;
"""


class Smoke:
    def __init__(self, args):
        self.args = args
        self.output = Path(args.output).resolve()
        self.output.mkdir(parents=True, exist_ok=True)
        self.opener = build_opener(ProxyHandler({}), NoRedirect())
        self.session = None
        self.secrets = []
        self.report = {
            "status": "failed",
            "started_at": dt.datetime.now(dt.timezone.utc).isoformat(),
            "base_url": args.base_url,
            "checks": [],
            "screenshots": [],
            "scope": "MiniBrowser UI creation, member visibility and board-access revocation; isolated development prototype",
        }

    def checked(self, name):
        self.report["checks"].append(name)
        print(json.dumps({"check": name, "status": "passed"}), flush=True)

    def http(self, method, url, body=None, headers=None):
        encoded = None if body is None else json.dumps(body, ensure_ascii=False).encode("utf-8")
        request = Request(url, data=encoded, method=method, headers=headers or {})
        request.add_header("Accept", "application/json")
        if encoded is not None:
            request.add_header("Content-Type", "application/json; charset=utf-8")
        try:
            with self.opener.open(request, timeout=30) as response:
                status, raw = response.status, response.read()
        except HTTPError as error:
            status, raw = error.code, error.read()
        except (URLError, TimeoutError, OSError) as error:
            raise SmokeError("Local HTTP transport failed") from error
        try:
            value = json.loads(raw) if raw else {}
        except (ValueError, UnicodeError) as error:
            raise SmokeError(f"Local endpoint returned non-JSON (HTTP {status})") from error
        return status, value

    def driver(self, method, endpoint, body=None):
        status, payload = self.http(method, self.args.driver_url + endpoint, body)
        value = payload.get("value", payload) if isinstance(payload, dict) else payload
        if status >= 400 or (isinstance(value, dict) and value.get("error")):
            # WebDriver error messages can echo typed credentials. Never log them.
            raise SmokeError(f"WebDriver {method} {endpoint.split('/session/')[0] or 'session command'} failed (HTTP {status})")
        return value

    def command(self, method, endpoint, body=None):
        require(self.session is not None, "No browser session")
        return self.driver(method, f"/session/{quote(self.session, safe='')}{endpoint}", body)

    def execute(self, script, *args):
        return self.command("POST", "/execute/sync", {"script": script, "args": list(args)})

    def api(self, token, method, path, body=None, expected=200):
        status, value = self.http(
            method,
            self.args.base_url + "/enterprise-api" + path,
            body,
            {"Authorization": f"Bearer {token}"},
        )
        require(status == expected, f"API {method} {path} expected HTTP {expected}, received {status}")
        return value

    def wait(self, description, operation):
        deadline = time.monotonic() + self.args.timeout
        while time.monotonic() < deadline:
            try:
                result = operation()
                if result:
                    return result
            except SmokeError:
                pass
            time.sleep(0.2)
        raise SmokeError(f"Timed out: {description}")

    def find(self, **spec):
        return self.execute(FIND, spec)

    def click(self, description, **spec):
        def attempt():
            element = self.find(**spec)
            if not element:
                return False
            self.command("POST", f"/element/{quote(element[ELEMENT_KEY], safe='')}/click", {})
            return True
        self.wait(description, attempt)

    def fill(self, field_id, value):
        element = self.wait(f"editable {field_id}", lambda: self.find(selector=f"#{field_id}"))
        endpoint = f"/element/{quote(element[ELEMENT_KEY], safe='')}"
        self.command("POST", endpoint + "/clear", {})
        # WebDriver send-keys generates input events and updates Svelte bindings.
        self.command("POST", endpoint + "/value", {"text": value})
        self.wait(f"input events for {field_id}", lambda: self.execute("return arguments[0].value", element) == value)

    def submit(self, field_id):
        script = """
const field = document.getElementById(arguments[0]);
const form = field && field.closest('form');
const button = form && form.querySelector('button[type=submit]');
if (!button || button.disabled || !form.checkValidity()) return false;
form.requestSubmit(button);
return true;
"""
        self.wait(f"submit form for {field_id}", lambda: self.execute(script, field_id))

    def select(self, field_id, label):
        self.click(f"open {field_id}", selector=f"#{field_id}")
        self.click(f"choose {field_id} option", selector='[role="option"]', text=[label])
        self.wait(f"selected {field_id}", lambda: self.execute(
            "return document.getElementById(arguments[0])?.textContent.trim() === arguments[1]",
            field_id, label,
        ))

    def text_present(self, text):
        return self.execute("return document.body.innerText.includes(arguments[0])", text)

    def start(self):
        session = self.driver("POST", "/session", {
            "capabilities": {"alwaysMatch": {"browserName": "MiniBrowser"}},
        })
        self.session = session.get("sessionId")
        require(bool(self.session), "WebKitWebDriver did not create a session")
        self.command("POST", "/timeouts", {"implicit": 0, "pageLoad": 60000, "script": 10000})
        self.command("POST", "/window/rect", {"width": 1280, "height": 1100})
        self.command("POST", "/url", {"url": self.args.base_url + "/enterprise-dev"})
        self.wait("prototype login form", lambda: self.find(selector="#enterprise-token"))
        if self.find(text=["繁體中文"]):
            self.click("switch interface to Traditional Chinese", text=["繁體中文"])
        self.wait("Traditional Chinese interface", lambda: (
            self.find(text=["English"]) and self.text_present("連接企業原型")
        ))
        self.report["interface_language"] = "zh-TW"
        self.execute("""
window.__enterpriseSmokeRequests = [];
const originalFetch = window.fetch.bind(window);
window.fetch = async function(input, options) {
  const response = await originalFetch(input, options);
  const pathname = new URL(typeof input === 'string' ? input : input.url, location.href).pathname;
  if (pathname.startsWith('/enterprise-api/')) {
    window.__enterpriseSmokeRequests.push({path: pathname, method: (options?.method || input?.method || 'GET').toUpperCase(), status: response.status});
  }
  return response;
};
""")
        self.checked("real_minibrowser_opened")

    def login(self, token, actor_id):
        self.fill("enterprise-token", token)
        self.submit("enterprise-token")
        self.wait("authenticated actor visible", lambda: (
            self.find(text=DISCONNECT) and self.text_present(actor_id)
            and not self.find(selector="#enterprise-token")
        ))
        # A fresh organization has no workspace yet, so its selector is disabled.
        self.wait("workspace selector loaded", lambda: self.execute("return !!document.querySelector('#enterprise-workspace')"))

    def settled_paint(self):
        # DOM assertions can finish before WebKit has painted the new frame.
        # Require two animation frames and unchanged viewport geometry after a
        # short compositor allowance, rather than treating a sleep as proof.
        return self.command("POST", "/execute/async", {"script": """
const done = arguments[arguments.length - 1];
let finished = false;
const finish = value => {
  if (finished) return;
  finished = true;
  clearTimeout(watchdog);
  done(value);
};
const geometry = () => JSON.stringify([
  innerWidth, innerHeight, scrollX, scrollY,
  ...[...document.querySelectorAll('main, [data-slot="scroll-area-viewport"]')].map(e => {
    const rect = e.getBoundingClientRect();
    return [rect.x, rect.y, rect.width, rect.height, e.scrollTop, e.scrollLeft, e.scrollHeight];
  })
]);
const watchdog = setTimeout(() => finish(false), 2000);
requestAnimationFrame(() => requestAnimationFrame(() => {
  const before = geometry();
  setTimeout(() => finish(document.visibilityState === 'visible'
    && (!document.fonts || document.fonts.status === 'loaded')
    && geometry() === before), 150);
}));
""", "args": []})

    def revoked_content_cleared(self, names, member_id):
        return self.execute("""
const body = document.body.innerText;
return !body.includes(arguments[0]) && !body.includes(arguments[1])
  && body.includes(arguments[2]) && !document.querySelector('#enterprise-token')
  && !document.querySelector('#enterprise-edit-title')
  && !document.querySelector('#enterprise-comment')
  && [...document.querySelectorAll('button')].some(e => arguments[3].includes(e.textContent.trim()));
""", names["task"], names["comment"], member_id, DISCONNECT)

    def capture(self, name, optional=False, verify=None):
        if not self.session:
            return
        self.wait("stable painted frame for " + name, self.settled_paint)
        if verify is not None:
            require(verify(), "Screenshot state validation failed after paint: " + name)
        # Recheck privacy after painting; a state transition could have returned
        # to the login form or opened a credential dialog while waiting.
        safe = self.execute("""
const visible = e => e.getClientRects().length && getComputedStyle(e).visibility !== 'hidden';
return ![...document.querySelectorAll('#enterprise-token, #enterprise-issued-token, input[type=password], [role=dialog]')].some(visible);
""")
        if not safe:
            if optional:
                self.report["failure_screenshot"] = "omitted: credential form or dialog visible"
                return
            raise SmokeError("Screenshot refused while a credential form or dialog is visible")
        encoded = self.command("GET", "/screenshot")
        if verify is not None:
            require(verify(), "Screenshot state changed during capture: " + name)
        filename = name + ".png"
        (self.output / filename).write_bytes(base64.b64decode(encoded, validate=True))
        self.report["screenshots"].append(filename)

    def run(self):
        try:
            credentials = json.loads(Path(self.args.credentials).read_text(encoding="utf-8"))
            owner_token = credentials["token"]
            owner_id = credentials["user"]["id"]
            require(isinstance(owner_token, str) and bool(owner_token), "Invalid owner credentials")
        except (OSError, ValueError, KeyError, TypeError) as error:
            raise SmokeError("Cannot read a valid bootstrap credentials file") from error
        self.secrets.append(owner_token)
        owner = self.api(owner_token, "GET", "/v1/me")
        require(owner["user"]["id"] == owner_id and owner["user"]["role"] == "owner", "Bootstrap identity is not the expected owner")
        stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%d-%H%M%S-%f")
        names = {
            "workspace": "瀏覽器測試工作區 " + stamp,
            "board": "企業共同看板 " + stamp,
            "task": "中文協作任務 " + stamp,
            "description": "真實瀏覽器輸入與服務端保存 " + stamp,
            "comment": "中文討論與撤權驗證 " + stamp,
            "member": "測試協作者 " + stamp,
            "device": "測試瀏覽器裝置 " + stamp,
        }
        self.report["fixtures"] = names
        self.start()
        self.login(owner_token, owner_id)
        self.checked("owner_authenticated_in_ui")
        self.click("expand resource forms", selector="summary", text=["Create a workspace or board", "建立工作區或看板"])
        self.fill("enterprise-workspace-name", names["workspace"])
        self.submit("enterprise-workspace-name")
        self.wait("new workspace selected", lambda: self.find(selector="#enterprise-workspace", text=[names["workspace"]]))
        workspaces = self.api(owner_token, "GET", "/v1/workspaces")["workspaces"]
        matches = [item for item in workspaces if item["name"] == names["workspace"]]
        require(len(matches) == 1, "UI workspace creation did not persist exactly once")
        workspace_id = matches[0]["id"]
        self.fill("enterprise-board-name", names["board"])
        self.submit("enterprise-board-name")
        self.wait("new board selected", lambda: self.find(selector="#enterprise-board", text=[names["board"]]))
        boards = self.api(owner_token, "GET", "/v1/boards?workspace_id=" + quote(workspace_id, safe=""))["boards"]
        matches = [item for item in boards if item["name"] == names["board"]]
        require(len(matches) == 1, "UI board creation did not persist exactly once")
        board_id = matches[0]["id"]
        board_path = "/v1/boards/" + quote(board_id, safe="")
        self.checked("workspace_and_board_created_through_ui")
        self.fill("enterprise-task-title", names["task"])
        self.fill("enterprise-task-description", names["description"])
        self.submit("enterprise-task-title")
        self.wait("task displayed and selected", lambda: self.execute(
            "return document.querySelector('#enterprise-edit-title')?.value === arguments[0] && document.querySelector('#enterprise-task-title')?.value === ''",
            names["task"],
        ))
        snapshot = self.api(owner_token, "GET", board_path + "/snapshot")
        tasks = [task for task in snapshot["tasks"] if task["title"] == names["task"]]
        require(len(tasks) == 1 and tasks[0]["description"] == names["description"], "UI task content did not persist exactly once")
        task_id = tasks[0]["id"]
        self.fill("enterprise-comment", names["comment"])
        self.submit("enterprise-comment")
        self.wait("comment displayed", lambda: self.find(selector="article", contains=names["comment"]))
        snapshot = self.api(owner_token, "GET", board_path + "/snapshot")
        comments = [comment for comment in snapshot["comments"] if comment["body"] == names["comment"]]
        require(len(comments) == 1 and comments[0]["task_id"] == task_id and comments[0]["author_id"] == owner_id, "UI comment body or authenticated author was not preserved")
        self.report["ids"] = {"workspace": workspace_id, "board": board_id, "task": task_id}
        self.checked("task_and_comment_created_through_ui_and_verified_by_api")
        self.execute("document.querySelector('#enterprise-edit-title').scrollIntoView({block:'start'}); return true")
        self.capture("owner-created")

        member = self.api(owner_token, "POST", "/v1/members", {
            "name": names["member"], "role": "member", "device_name": names["device"],
        })
        member_token = member["token"]
        self.secrets.append(member_token)
        member_id = member["member"]["id"]
        self.report["ids"]["member"] = member_id
        self.api(owner_token, "PUT", board_path + "/access", {"member_id": member_id, "role": "editor"})
        self.click("owner disconnect", text=DISCONNECT)
        self.wait("login after disconnect", lambda: self.find(selector="#enterprise-token"))
        self.login(member_token, member_id)
        self.select("enterprise-workspace", names["workspace"])
        self.select("enterprise-board", names["board"])
        self.wait("member sees shared task and comment", lambda: (
            self.find(selector='button[aria-pressed]', contains=names["task"])
            and self.find(selector="article", contains=names["comment"])
        ))
        member_snapshot = self.api(member_token, "GET", board_path + "/snapshot")
        require(member_snapshot["board"]["role"] == "editor", "Member did not receive editor access")
        require(any(task["id"] == task_id for task in member_snapshot["tasks"]), "Member snapshot lacks the created task")
        self.checked("editor_member_authenticated_and_sees_existing_content")
        self.execute("document.querySelector('#enterprise-edit-title').scrollIntoView({block:'start'}); return true")
        self.capture("member-visible")

        marker = self.execute("return window.__enterpriseSmokeRequests.length")
        self.api(owner_token, "PUT", board_path + "/access", {"member_id": member_id, "role": None})
        self.api(member_token, "GET", board_path + "/snapshot", expected=404)
        self.click("board refresh after revocation", heading=names["board"], text=REFRESH)
        request_path = "/enterprise-api" + board_path + "/snapshot"
        self.wait("browser refresh received snapshot 404", lambda: self.execute(
            "return window.__enterpriseSmokeRequests.slice(arguments[0]).some(r => r.path === arguments[1] && r.method === 'GET' && r.status === 404)",
            marker, request_path,
        ))
        self.wait("revoked content removed while member remains signed in",
                  lambda: self.revoked_content_cleared(names, member_id))
        require(bool(self.find(text=DISCONNECT)), "Board revocation incorrectly ended the member session")
        require(self.api(member_token, "GET", "/v1/me")["user"]["id"] == member_id, "Member identity no longer valid after board revocation")
        self.checked("ui_refresh_404_cleared_task_and_comment_without_logging_out_member")
        self.execute("document.querySelector('main').scrollIntoView({block:'start'}); return true")
        self.capture("member-revoked", verify=lambda: self.revoked_content_cleared(names, member_id))
        self.checked("revoked_dom_revalidated_after_stable_paint_and_screenshot")
        self.report["browser_snapshot_revocation"] = {"method": "GET", "path": request_path, "status": 404}
        self.report["status"] = "passed"

    def finish(self):
        if self.session:
            try:
                self.command("DELETE", "")
                self.report["browser_session_closed"] = True
            except SmokeError:
                self.report["browser_session_closed"] = False
            self.session = None
        self.report["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
        encoded = json.dumps(self.report, ensure_ascii=False, indent=2)
        for secret in self.secrets:
            if secret:
                encoded = encoded.replace(secret, "[REDACTED]")
        report_path = self.output / "enterprise-smoke-result.json"
        report_path.write_text(encoded + "\n", encoding="utf-8")
        print(json.dumps({"status": self.report["status"], "report": str(report_path)}, ensure_ascii=False), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver-url", type=local_url, default="http://127.0.0.1:9515")
    parser.add_argument("--base-url", type=local_url, default="http://127.0.0.1:1441")
    parser.add_argument("--credentials", required=True, help="Private bootstrap JSON file; its contents are never printed")
    parser.add_argument("--output", required=True, help="Directory for safe screenshots and a credential-free JSON report")
    parser.add_argument("--timeout", type=float, default=45, help="Seconds for each UI condition")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    smoke = Smoke(args)
    try:
        smoke.run()
    except Exception as error:
        # Unexpected errors report their class only, never raw data or traceback.
        message = str(error) if isinstance(error, SmokeError) else type(error).__name__
        for secret in smoke.secrets:
            if secret:
                message = message.replace(secret, "[REDACTED]")
        smoke.report["error"] = message
        try:
            smoke.capture("failure", optional=True)
        except Exception:
            smoke.report["failure_screenshot"] = "unavailable"
        print(json.dumps({"status": "failed", "error": message}, ensure_ascii=False), flush=True)
    finally:
        smoke.finish()
    return 0 if smoke.report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
