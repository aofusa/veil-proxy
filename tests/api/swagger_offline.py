"""F-264: open /docs in headless Chromium with no network and check that Swagger UI renders from the bundled assets.

    python3 swagger_offline.py <base-url> <expected-path> [<expected-path>...]

Fails if the page logs a console error, violates its Content-Security-Policy, requests another origin, or does not
render an operation for each expected path within 20 seconds. A path written as ``ws:/path`` must render without a
"Try it out" button (a WebSocket operation) and one written as ``try:/path`` with it.
"""

import sys
from urllib.parse import urlsplit

from playwright.sync_api import sync_playwright

base, args = sys.argv[1].rstrip("/"), sys.argv[2:]
paths = [a.split(":", 1)[1] if a.startswith(("ws:", "try:")) else a for a in args]
bad_try = []
origin = urlsplit(base).netloc
errors, foreign = [], []

with sync_playwright() as p:
    browser = p.chromium.launch()
    page = browser.new_page()
    page.on("console", lambda m: m.type == "error" and errors.append(m.text))
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.on("request", lambda r: urlsplit(r.url).netloc not in ("", origin) and not r.url.startswith("data:") and foreign.append(r.url))
    page.add_init_script(
        "document.addEventListener('securitypolicyviolation', e => console.error('CSP ' + e.violatedDirective + ' ' + e.blockedURI))"
    )
    page.goto(f"{base}/docs", wait_until="networkidle")
    for path in paths:
        page.wait_for_selector(f".opblock-summary-path[data-path='{path}']", timeout=20000)
    for arg, path in zip(args, paths):
        if not arg.startswith(("ws:", "try:")):
            continue
        block = page.locator(".opblock", has=page.locator(f".opblock-summary-path[data-path='{path}']")).first
        block.locator(".opblock-summary").first.click()
        block.locator(".opblock-body").first.wait_for(timeout=10000)
        has_try = block.locator(".try-out__btn").count() > 0
        if has_try != arg.startswith("try:"):
            bad_try.append(f"{path}: Try it out {'shown' if has_try else 'missing'}")
    title = page.title()
    browser.close()

for kind, items in (("console errors", errors), ("requests to other origins", foreign), ("Try it out", bad_try)):
    if items:
        print(f"FAIL {kind}: {items}")
if errors or foreign or bad_try:
    sys.exit(1)
print(f"ok   {base}/docs rendered {len(paths)} paths offline ({title})")
