#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Michael Totten <mike@ozaru.io>
# SPDX-License-Identifier: GPL-3.0-or-later
"""
surf-relay — the internet gateway for the BLUETOOTH BROWSER app on Passport
Prime (KeyOS).

The Passport has no network stack: it is BLE-only. Quantum Link carries small
JSON envelopes between the device app (account "web-0") and a gateway. This
relay IS that gateway: it receives a `fetch` request, downloads the page over
the internet (optionally through Tor), converts the HTML into a small list of
*safe structured blocks* (no JavaScript, no raw HTML ever reaches the device),
and replies with a `page` envelope the device renders natively.

Transport today: TCP, one JSON envelope per line (newline-delimited) — the
exact interface a future BLE bridge (Web Bluetooth or a phone daemon) will
front-end. On real hardware the relay runs on any gateway with BLE + internet.

Usage:
    python3 surf_relay.py --port 8787                 # clearnet gateway
    python3 surf_relay.py --socks 127.0.0.1:9050      # everything via Tor
    python3 surf_relay.py --self-test https://example.com   # one-shot fetch+convert
"""

import argparse
import json
import logging
import re
import socketserver
import sys
import urllib.error
import urllib.parse
import urllib.request
from html.parser import HTMLParser

LOG = logging.getLogger("surf-relay")

# ---------------------------------------------------------------------------
# HTML -> safe blocks
# ---------------------------------------------------------------------------

# kebab->snake for headings and the generic text emitter
HEADINGS = {f"h{i}": f"heading-{i}" for i in range(1, 7)}

INLINE_RE = re.compile(r"\s+")


class BlockBuilder(HTMLParser):
    """Turn an HTML document into a flat list of safe text blocks."""

    def __init__(self, base_url: str, max_blocks: int = 400):
        super().__init__(convert_charrefs=True)
        self.base_url = base_url
        self.max_blocks = max_blocks
        self.blocks: list[dict] = []
        self._title = ""
        self._skip_depth = 0          # >0 while inside <script>/<style>
        self._text_buf: list[str] = []
        self._cur_heading: str | None = None   # e.g. "heading-1" when inside an h1
        self._in_anchor = False
        self._anchor_href = None
        self._pending_link: dict | None = None
        self._in_li = False
        self._in_pre = False
        self._in_quote = False
        self._in_title = False

    # -- helpers ------------------------------------------------------------

    def _flush_text(self, kind: str | None = None):
        """Emit the buffered inline text as one block."""
        text = " ".join(self._text_buf)
        self._text_buf = []
        text = re.sub(INLINE_RE, " ", text).strip()
        if not text:
            self._cur_heading = None
            return
        if len(self.blocks) >= self.max_blocks:
            self._cur_heading = None
            return
        if kind is None:
            kind = self._cur_heading or "text"
        self._cur_heading = None
        if self._pending_link is not None:
            # Text belonging to an <a>: emit as a link block.
            block = {"kind": "link", "text": text, "href": self._pending_link}
            self._pending_link = None
            self.blocks.append(block)
        else:
            self.blocks.append({"kind": kind, "text": text})

    def _emit(self, kind: str, text: str):
        if len(self.blocks) >= self.max_blocks:
            return
        self.blocks.append({"kind": kind, "text": text})

    def _resolve(self, href: str):
        href = href.strip()
        if not href:
            return None
        if href.startswith(("#", "javascript:", "mailto:", "tel:", "data:")):
            return None
        url = urllib.parse.urljoin(self.base_url, href)
        if url.startswith(("http://", "https://")):
            return url
        return None

    # -- parser callbacks ----------------------------------------------------

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag in ("script", "style"):
            self._skip_depth += 1
            return
        if self._skip_depth:
            return
        if tag == "title":
            self._in_title = True
            return
        if tag in HEADINGS:
            self._flush_text()
            self._cur_heading = HEADINGS[tag]
            return
        if tag == "p":
            self._flush_text()
            return
        if tag == "br":
            self._flush_text()
            return
        if tag == "hr":
            self._flush_text()
            self._emit("separator", "──────")
            return
        if tag == "a":
            href = self._resolve(attrs.get("href", ""))
            self._flush_text()
            if href:
                self._pending_link = href
            return
        if tag in ("b", "strong", "em", "i", "u", "span", "font", "small"):
            return
        if tag == "blockquote":
            self._flush_text()
            self._in_quote = True
            return
        if tag == "pre":
            self._flush_text()
            self._in_pre = True
            return
        if tag == "code" and self._in_pre:
            return
        if tag == "li":
            self._flush_text()
            self._in_li = True
            return
        if tag == "ul":
            self._flush_text()
            return
        if tag == "ol":
            self._flush_text()
            return
        if tag == "img":
            alt = attrs.get("alt", "").strip()
            if alt:
                self._emit("text", f"[image: {alt}]")
            return

    def handle_endtag(self, tag):
        if tag in ("script", "style"):
            self._skip_depth = max(0, self._skip_depth - 1)
            return
        if self._skip_depth:
            return
        if tag == "title":
            self._in_title = False
            return
        if tag in HEADINGS:
            self._flush_text()
            return
        if tag == "p":
            self._flush_text()
            return
        if tag == "a":
            if self._pending_link is not None:
                self._flush_text()  # anchor with no text -> drop
            return
        if tag == "blockquote":
            self._flush_text()
            self._in_quote = False
            return
        if tag == "pre":
            self._flush_text()
            self._in_pre = False
            return
        if tag == "li":
            self._flush_text()
            self._in_li = False
            return

    def handle_data(self, data):
        if self._skip_depth or self._in_title:
            return
        if self._in_pre:
            self._text_buf.append(data.strip("\n"))
            return
        self._text_buf.append(data)


def html_to_blocks(html: str, url: str) -> tuple[str, list[dict]]:
    builder = BlockBuilder(url)
    try:
        builder.feed(html)
        builder.close()
    except Exception as e:  # never let a hostile page break the relay
        LOG.warning("parse hiccup: %s", e)
    builder._flush_text()
    title = ""
    m = re.search(r"<title[^>]*>(.*?)</title>", html, re.S | re.I)
    if m:
        title = re.sub(INLINE_RE, " ", m.group(1)).strip()[:120]
    return title, builder.blocks


# ---------------------------------------------------------------------------
# Fetch
# ---------------------------------------------------------------------------

USER_AGENT = "surf-relay/1.0 (Passport Prime BLUETOOTH BROWSER gateway)"


def build_opener(socks_addr: str | None):
    if socks_addr is None:
        return urllib.request.build_opener(
            urllib.request.ProxyHandler({})  # ignore env proxies; explicit
        )
    try:
        import socks  # PySocks
    except ImportError:
        raise SystemExit(
            "PySocks is required for --socks; install with: pip3 install PySocks"
        )
    host, _, port = socks_addr.partition(":")
    socks.set_default_proxy(socks.SOCKS5, host, int(port), rdns=True)
    socks.wrapmodule(urllib.request)
    return urllib.request.build_opener()


def fetch_page(url: str, opener, timeout: int, max_bytes: int):
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with opener.open(req, timeout=timeout) as resp:
        ctype = resp.headers.get("Content-Type", "")
        if "text/html" not in ctype and "application/xhtml" not in ctype:
            return (200, "text", None,
                    f"Not HTML ({ctype.split(';')[0] or 'unknown'}) — refusing to render")
        data = resp.read(max_bytes + 1)
        truncated = len(data) > max_bytes
        final_url = resp.geturl()
    html = data[:max_bytes].decode("utf-8", errors="replace")
    title, blocks = html_to_blocks(html, final_url)
    if not blocks:
        blocks = [{"kind": "text", "text": "(page contains no readable text)"}]
    if truncated:
        blocks.append({"kind": "separator", "text": "──────"})
        blocks.append({"kind": "text",
                       "text": f"[truncated at {max_bytes} bytes — device-safe size limit]"})
    return (200, "html", title, blocks, final_url)


# ---------------------------------------------------------------------------
# Server: one JSON envelope per line
# ---------------------------------------------------------------------------

class RelayHandler(socketserver.StreamRequestHandler):
    def handle(self):
        opener = self.server.opener
        timeout = self.server.fetch_timeout
        max_bytes = self.server.max_bytes
        while True:
            line = self.rfile.readline()
            if not line:
                return
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except ValueError:
                self._reply({"type": "error", "error": "bad json"})
                continue
            if msg.get("type") == "ping":
                self._reply({"type": "pong"})
            elif msg.get("type") == "fetch":
                url = str(msg.get("url", "")).strip()
                rid = str(msg.get("id", ""))
                if not url.startswith(("http://", "https://")):
                    self._reply({"type": "page", "id": rid, "status": 0,
                                 "title": "Bad URL", "url": url,
                                 "blocks": [{"kind": "text",
                                             "text": "Only http(s) URLs are allowed."}],
                                 "error": "bad url"})
                    continue
                try:
                    status, kind, title, blocks, final_url = fetch_page(
                        url, opener, timeout, max_bytes)
                    if kind == "html":
                        self._reply({"type": "page", "id": rid, "status": status,
                                     "title": title, "url": final_url,
                                     "blocks": blocks, "error": None})
                    else:
                        self._reply({"type": "page", "id": rid, "status": status,
                                     "title": "Not HTML", "url": url,
                                     "blocks": [{"kind": "text", "text": blocks}],
                                     "error": "not html"})
                except urllib.error.HTTPError as e:
                    self._reply({"type": "page", "id": rid, "status": e.code,
                                 "title": f"HTTP {e.code}", "url": url,
                                 "blocks": [{"kind": "text",
                                             "text": f"HTTP error {e.code}"}],
                                 "error": f"http {e.code}"})
                except Exception as e:
                    LOG.warning("fetch failed: %s", e)
                    self._reply({"type": "page", "id": rid, "status": 0,
                                 "title": "Fetch failed", "url": url,
                                 "blocks": [{"kind": "text",
                                             "text": f"Could not load: {e}"}],
                                 "error": str(e)})
            else:
                self._reply({"type": "error", "error": "unknown type"})

    def _reply(self, payload: dict):
        self.wfile.write((json.dumps(payload) + "\n").encode("utf-8"))
        self.wfile.flush()


class RelayServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, addr, handler, opener, fetch_timeout, max_bytes):
        self.opener = opener
        self.fetch_timeout = fetch_timeout
        self.max_bytes = max_bytes
        super().__init__(addr, handler)


# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description="surf-relay gateway for Passport Prime browser")
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--bind", default="0.0.0.0")
    ap.add_argument("--socks", default=None, help="Tor SOCKS5 proxy host:port (e.g. 127.0.0.1:9050)")
    ap.add_argument("--timeout", type=int, default=30)
    ap.add_argument("--max-bytes", type=int, default=524288)
    ap.add_argument("--self-test", default=None, help="fetch a URL once, print blocks, exit")
    args = ap.parse_args()

    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")

    if args.self_test:
        opener = build_opener(args.socks)
        status, kind, title, blocks, final_url = fetch_page(
            args.self_test, opener, args.timeout, args.max_bytes)
        print(f"status={status} title={title!r} url={final_url}")
        for b in blocks:
            print(f"  [{b['kind']}] {b['text'][:120]}")
        return 0

    opener = build_opener(args.socks)
    server = RelayServer((args.bind, args.port), RelayHandler, opener,
                         args.timeout, args.max_bytes)
    via = f"via Tor {args.socks}" if args.socks else "clearnet"
    LOG.info("surf-relay listening on %s:%d (%s)", args.bind, args.port, via)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        LOG.info("bye")


if __name__ == "__main__":
    sys.exit(main())
