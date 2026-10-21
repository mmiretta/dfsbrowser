#!/usr/bin/env python3
"""Capture the README screenshots from a running demo.

Drives headless Chrome over the DevTools protocol (needs `pip install websocket-client`
and a Chrome/Chromium binary). Start the demo first:

    docker compose --env-file demo/demo.env --profile demo up -d --build
    python3 scripts/screenshots.py --url http://127.0.0.1:8888 --out docs/screenshots
"""
import argparse
import base64
import json
import os
import shutil
import subprocess
import tempfile
import time
import urllib.request

import websocket


class Page:
    def __init__(self, ws_url):
        self.ws = websocket.create_connection(ws_url, timeout=60, suppress_origin=True)
        self.n = 0

    def call(self, method, **params):
        self.n += 1
        self.ws.send(json.dumps({"id": self.n, "method": method, "params": params}))
        while True:
            msg = json.loads(self.ws.recv())
            if msg.get("id") == self.n:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg.get("result", {})

    def js(self, expr):
        r = self.call("Runtime.evaluate", expression=expr, awaitPromise=True, returnByValue=True)
        if "exceptionDetails" in r:
            raise RuntimeError(f"JS error in {expr!r}: {r['exceptionDetails']}")
        return r.get("result", {}).get("value")

    def wait(self, expr, timeout=30):
        end = time.time() + timeout
        while time.time() < end:
            if self.js(expr):
                return
            time.sleep(0.2)
        raise TimeoutError(expr)

    def shot(self, path):
        time.sleep(0.6)  # let transitions settle
        data = self.call("Page.captureScreenshot", format="png")["data"]
        with open(path, "wb") as f:
            f.write(base64.b64decode(data))
        print("wrote", path)

    def set_file(self, selector, path):
        root = self.call("DOM.getDocument")["root"]["nodeId"]
        node = self.call("DOM.querySelector", nodeId=root, selector=selector)["nodeId"]
        self.call("DOM.setFileInputFiles", nodeId=node, files=[os.path.abspath(path)])


IDLE = "!document.getElementById('loading').classList.contains('show')"


def open_row(p, name):
    p.wait(IDLE)
    p.js(f"""(() => {{
        const row = [...document.querySelectorAll('#files tr[data-i]')]
            .find(r => r.querySelector('.fname').textContent === {json.dumps(name)});
        row.click(); return true; }})()""")


def go(p, path):
    p.js(f"navigate({json.dumps(path)})")
    p.wait(IDLE + f" && CWD === {json.dumps(path)}")


def pane(p, name, ready):
    p.js(f"document.querySelector('.side-tabs .tab[data-pane={name}]').click()")
    p.wait(ready)


def login(p, args, with_key):
    p.wait("document.getElementById('l-bucket').value !== ''")
    p.js(f"""document.getElementById('l-access').value = {json.dumps(args.access_key)};
             document.getElementById('l-secret').value = {json.dumps(args.secret_key)}; true""")
    if with_key:
        p.set_file("#key-file", args.key)
        p.wait("document.getElementById('key-btn').classList.contains('loaded')")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://127.0.0.1:8888")
    ap.add_argument("--out", default="docs/screenshots")
    ap.add_argument("--access-key", default="demo-admin")
    ap.add_argument("--secret-key", default="demo-password-change-me")
    ap.add_argument("--key", default="demo/master.key")
    ap.add_argument("--chrome", default=shutil.which("google-chrome") or shutil.which("chromium") or "chromium")
    ap.add_argument("--port", type=int, default=9337)
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)

    profile = tempfile.mkdtemp(prefix="dfsb-shots-")
    chrome = subprocess.Popen(
        [args.chrome, "--headless=new", f"--remote-debugging-port={args.port}", f"--user-data-dir={profile}",
         "--no-first-run", "--disable-gpu", "--hide-scrollbars", "--window-size=1440,900",
         "--force-color-profile=srgb", "--lang=en-GB", "about:blank"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(50):
            try:
                tabs = json.load(urllib.request.urlopen(f"http://127.0.0.1:{args.port}/json"))
                break
            except OSError:
                time.sleep(0.2)
        page_ws = next(t["webSocketDebuggerUrl"] for t in tabs if t["type"] == "page")
        p = Page(page_ws)
        p.call("Page.enable")
        p.call("Emulation.setDeviceMetricsOverride", width=1440, height=900, deviceScaleFactor=1, mobile=False)
        p.call("Emulation.setTimezoneOverride", timezoneId="UTC")
        o = lambda n: os.path.join(args.out, n)

        p.call("Page.navigate", url=args.url + "/")
        login(p, args, with_key=True)
        p.shot(o("01-login.png"))
        p.js("document.querySelector('#login-form button[type=submit]').click()")
        p.wait("document.getElementById('app').classList.contains('visible') && document.querySelectorAll('#files tr[data-i]').length > 0")
        p.wait(IDLE)
        p.shot(o("02-browse.png"))

        go(p, "/backups")
        open_row(p, "photos-2026-10-01.tar.zst")
        p.wait("document.querySelectorAll('#side-body .chunk').length > 0")
        p.shot(o("03-details-chunks.png"))

        go(p, "/documents")
        open_row(p, "architecture.md")
        pane(p, "versions", "document.querySelectorAll('#side-body .ver').length > 1")
        p.shot(o("04-versions.png"))

        go(p, "/photos/2026-coast-trip")
        open_row(p, "lighthouse.svg")
        pane(p, "preview", "(() => { const i = document.querySelector('#side-body img'); return i && i.complete && i.naturalWidth > 0; })()")
        p.shot(o("05-preview-image.png"))

        go(p, "/projects/telemetry/src")
        open_row(p, "main.rs")
        pane(p, "preview", "!!document.querySelector('#side-body pre')")
        p.shot(o("06-preview-text.png"))

        p.js("document.querySelector('.topbar .tab[data-view=stats]').click()")
        p.wait("!!document.querySelector('#view-stats .cards')")
        p.shot(o("07-volume.png"))

        # A second session without the master key: encrypted files stay locked.
        p.js("document.getElementById('btn-logout').click()")
        p.wait("document.getElementById('login-screen').style.display === 'flex'")
        login(p, args, with_key=False)
        p.js("document.querySelector('#login-form button[type=submit]').click()")
        p.wait("document.getElementById('app').classList.contains('visible')")
        p.wait(IDLE)
        go(p, "/private")
        open_row(p, "budget-2026.csv")
        pane(p, "preview", "!!document.querySelector('#side-body .locked')")
        p.shot(o("08-encrypted-locked.png"))
    finally:
        chrome.terminate()
        chrome.wait(timeout=10)
        shutil.rmtree(profile, ignore_errors=True)


if __name__ == "__main__":
    main()
