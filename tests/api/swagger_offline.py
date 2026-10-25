"""F-264: open /docs in headless Chromium with no network and check that Swagger UI renders from the bundled assets.

    python3 swagger_offline.py <base-url> <expected-path> [<expected-path>...]

Fails if the page logs a console error, violates its Content-Security-Policy, requests another origin, or does not
render an operation for each expected path within 20 seconds.
"""

import sys
from urllib.parse import urlsplit

from playwright.sync_api import sync_playwright

base, paths = sys.argv[1].rstrip("/"), sys.argv[2:]
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
    title = page.title()
    browser.close()

for kind, items in (("console errors", errors), ("requests to other origins", foreign)):
    if items:
        print(f"FAIL {kind}: {items}")
if errors or foreign:
    sys.exit(1)
print(f"ok   {base}/docs rendered {len(paths)} paths offline ({title})")
