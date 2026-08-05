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

It also carries signed transactions: a `broadcast` envelope (a raw tx hex)
is submitted to a Bitcoin node — by default your own bitcoind on
127.0.0.1:8332 using cookie auth — and the resulting txid is returned in a
`broadcast-result` envelope. No third party ever touches the transaction.

Usage:
    python3 surf_relay.py --port 8787                 # clearnet gateway
    python3 surf_relay.py --socks 127.0.0.1:9050      # everything via Tor
    python3 surf_relay.py --self-test https://example.com   # one-shot fetch+convert
    python3 surf_relay.py --self-broadcast <txhex>          # one-shot broadcast via your node
    python3 surf_relay.py --self-qrng 32                    # one-shot: 32 quantum bytes from ANU
    python3 surf_relay.py --rpc-url http://127.0.0.1:8332 --rpc-cookie ~/.bitcoin/.cookie
"""

import argparse
import base64
import http.server
import json
import logging
import os
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
        self._pending_link: dict | None = None
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
            if self._cur_heading is not None:
                kind = self._cur_heading
            elif self._in_quote:
                kind = "quote"
            else:
                kind = "text"
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


# ---------------------------------------------------------------------------
# Broadcast (signed tx -> your bitcoin node)
# ---------------------------------------------------------------------------

HEX_CHARS = set("0123456789abcdefABCDEF")


class RpcError(Exception):
    def __init__(self, code, message):
        super().__init__(f"rpc error {code}: {message}")
        self.code = code
        self.message = message


def is_valid_txhex(txhex: str) -> bool:
    """Cheap sanity gate: hex, even-length, plausible raw tx size (>= 1 tx header)."""
    if len(txhex) < 10 or len(txhex) % 2 != 0:
        return False
    return all(c in HEX_CHARS for c in txhex)


def rpc_call(rpc_url: str, cookie_path: str, method: str, params: list,
             timeout: int = 30):
    """Call a bitcoind JSON-RPC method. Auth comes from the cookie file
    bitcoind writes when it has no rpcuser in bitcoin.conf (cookie auth)."""
    if not os.path.exists(cookie_path):
        raise RpcError(-1, f"cookie not found at {cookie_path} — start bitcoind with -server")
    with open(cookie_path, encoding="utf-8") as f:
        user, _, password = f.read().strip().partition(":")
    if not user or not password:
        raise RpcError(-1, f"malformed cookie at {cookie_path}")
    body = json.dumps({"jsonrpc": "1.0", "id": "surf-relay",
                       "method": method, "params": params}).encode("utf-8")
    token = base64.b64encode(f"{user}:{password}".encode("utf-8")).decode("ascii")
    req = urllib.request.Request(
        rpc_url, data=body,
        headers={"Content-Type": "application/json",
                 "Authorization": f"Basic {token}"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            reply = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")[:300]
        try:
            inner = json.loads(body).get("error") or {}
            raise RpcError(inner.get("code", e.code),
                           inner.get("message", body))
        except (ValueError, AttributeError):
            raise RpcError(e.code, body)
    except urllib.error.URLError as e:
        raise RpcError(-1, f"cannot reach node at {rpc_url}: {e.reason}")
    err = reply.get("error")
    if err:
        raise RpcError(err.get("code", -1), err.get("message", "unknown rpc error"))
    return reply.get("result")


def broadcast_tx(txhex: str, rpc_url: str, cookie_path: str, timeout: int = 30) -> str:
    """Submit a raw transaction to the node via sendrawtransaction.
    Returns the node-confirmed txid."""
    txid = rpc_call(rpc_url, cookie_path, "sendrawtransaction", [txhex], timeout)
    if not isinstance(txid, str) or len(txid) != 64:
        raise RpcError(-1, f"unexpected txid reply: {txid!r}")
    return txid


def finalize_psbt(psbt_b64: str, rpc_url: str, cookie_path: str,
                  timeout: int = 30) -> str:
    """Ask bitcoind to finalize a signed PSBT (base64). Returns the raw
    transaction hex. Raises if finalization is incomplete."""
    result = rpc_call(rpc_url, cookie_path, "finalizepsbt", [psbt_b64, True], timeout)
    if not isinstance(result, dict):
        raise RpcError(-1, f"unexpected finalizepsbt reply: {result!r}")
    if not result.get("complete"):
        raise RpcError(-1, "finalizepsbt incomplete — missing signatures")
    txhex = result.get("hex", "")
    if not txhex:
        raise RpcError(-1, "finalizepsbt returned no tx hex")
    return txhex


def submit_broadcast(payload: dict, rpc_url: str, cookie_path: str,
                     timeout: int = 30) -> str:
    """Accept either a raw `txhex` or a signed `psbt` (base64) and broadcast
    it through the node. Returns the node-confirmed txid."""
    txhex = str(payload.get("txhex", "")).strip()
    psbt_b64 = str(payload.get("psbt", "")).strip()
    if psbt_b64:
        txhex = finalize_psbt(psbt_b64, rpc_url, cookie_path, timeout)
    if not is_valid_txhex(txhex):
        raise RpcError(-1, "bad tx — expected a raw signed transaction (txhex) or a signed PSBT (psbt)")
    return broadcast_tx(txhex, rpc_url, cookie_path, timeout)


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
# QRNG (real quantum entropy -> device seed ceremony)
# ---------------------------------------------------------------------------

ANU_QRNG_URL = "https://qrng.anu.edu.au/API/jsonI.php"
ANU_MAX_LENGTH = 1024
# ANU hard-limits to 1 request/minute regardless of size, so cache the last
# fetch for ~55s and serve repeats from it — the bytes are still genuinely
# quantum, just reused within the window (matching the companion's fetch-once
# strategy). A ceremony needs only 16 (12-word) or 32 (24-word) bytes.
QRNG_CACHE_SECONDS = 55.0
_qrng_cache: dict = {"at": 0.0, "data": b""}


def fetch_anu_entropy(length: int = 64, opener=None, timeout: int = 30) -> bytes:
    """Fetch `length` genuinely quantum bytes from the ANU QRNG API (vacuum
    fluctuations measured at the Australian National University). One request
    is rate-limited to ~1/min, so keep `length` at what the ceremony needs
    (16 for 12 words, 32 for 24 words). Returns raw bytes.

    `opener` is the relay's opener (honors --socks / ignores env proxies),
    matching every other outbound path in this file."""
    if not 1 <= length <= ANU_MAX_LENGTH:
        raise ValueError(f"ANU QRNG length must be 1..{ANU_MAX_LENGTH}")
    import time as _time
    now = _time.monotonic()
    if now - _qrng_cache["at"] < QRNG_CACHE_SECONDS:
        LOG.info("qrng cache hit: %d bytes (fresh %ds ago)", len(_qrng_cache["data"]),
                 int(now - _qrng_cache["at"]))
        return _qrng_cache["data"][:length]
    # Always fetch the maximum ANU allows in one request (1024) and cache it:
    # the API is hard-limited to 1 request/min regardless of size, so a single
    # call covers every ceremony (12/24 words) plus any retries within the
    # window. This mirrors the companion's fetch-once/stream-in-chunks design.
    fetch_len = ANU_MAX_LENGTH if length < ANU_MAX_LENGTH else length
    url = f"{ANU_QRNG_URL}?length={fetch_len}&type=uint8"
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with (opener or urllib.request.build_opener()).open(req, timeout=timeout) as resp:
        body = json.loads(resp.read().decode("utf-8"))
    if not isinstance(body, dict) or body.get("success") is not True:
        raise ValueError("ANU QRNG returned an unexpected payload")
    data = body.get("data")
    if not isinstance(data, list) or len(data) != fetch_len:
        raise ValueError(f"ANU QRNG returned {len(data) if isinstance(data, list) else '?'} bytes, wanted {fetch_len}")
    if not all(isinstance(b, int) and 0 <= b <= 255 for b in data):
        raise ValueError("ANU QRNG returned invalid byte values")
    raw = bytes(data)
    _qrng_cache["at"] = now
    _qrng_cache["data"] = raw
    return raw[:length]


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
            elif msg.get("type") == "broadcast":
                rid = str(msg.get("id", ""))
                try:
                    txid = submit_broadcast(msg, self.server.rpc_url,
                                            self.server.rpc_cookie,
                                            self.server.rpc_timeout)
                    LOG.info("broadcast ok txid=%s", txid)
                    self._reply({"type": "broadcast-result", "id": rid,
                                 "txid": txid, "error": None})
                except Exception as e:
                    LOG.warning("broadcast failed: %s", e)
                    self._reply({"type": "broadcast-result", "id": rid,
                                 "txid": None, "error": str(e)})
            elif msg.get("type") == "qrng":
                rid = str(msg.get("id", ""))
                try:
                    length = int(msg.get("length", 64))
                    data = fetch_anu_entropy(length, opener, timeout)
                    LOG.info("qrng ok length=%d", len(data))
                    self._reply({"type": "qrng-result", "id": rid,
                                 "bytes": data.hex(), "length": len(data),
                                 "error": None})
                except Exception as e:
                    LOG.warning("qrng failed: %s", e)
                    self._reply({"type": "qrng-result", "id": rid,
                                 "bytes": "", "length": 0, "error": str(e)})
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

    def __init__(self, addr, handler, opener, fetch_timeout, max_bytes,
                 rpc_url, rpc_cookie, rpc_timeout):
        self.opener = opener
        self.fetch_timeout = fetch_timeout
        self.max_bytes = max_bytes
        self.rpc_url = rpc_url
        self.rpc_cookie = rpc_cookie
        self.rpc_timeout = rpc_timeout
        super().__init__(addr, handler)


# ---------------------------------------------------------------------------
# HTTP endpoint (companion apps): POST /broadcast
# ---------------------------------------------------------------------------

class RelayHttpHandler(http.server.BaseHTTPRequestHandler):
    """Companion-friendly HTTP surface over the same broadcast envelope.
    The Envoy companion app POSTs {"psbt": <base64>} (or {"txhex": ...}) to
    /broadcast and gets the broadcast-result JSON back, so the real-hardware
    flow (device -> BLE -> companion -> this -> your node) needs no TCP."""

    MAX_BODY = 2 * 1024 * 1024  # a signed PSBT is a few KB; refuse absurd bodies

    def do_POST(self):
        if self.path != "/broadcast":
            self._reply({"error": "not found"}, status=404)
            return
        try:
            length = int(self.headers.get("Content-Length", 0))
        except (TypeError, ValueError):
            self._reply({"type": "broadcast-result", "id": "",
                         "txid": None, "error": "bad content-length"}, status=400)
            return
        if length > self.MAX_BODY:
            self._reply({"type": "broadcast-result", "id": "",
                         "txid": None, "error": "body too large"}, status=413)
            return
        try:
            msg = json.loads(self.rfile.read(length) or b"{}")
        except ValueError:
            self._reply({"type": "broadcast-result", "id": "",
                         "txid": None, "error": "bad json"}, status=400)
            return
        if not isinstance(msg, dict):
            self._reply({"type": "broadcast-result", "id": "",
                         "txid": None, "error": "bad body"}, status=400)
            return
        rid = str(msg.get("id", ""))
        try:
            txid = submit_broadcast(msg, self.server.rpc_url,
                                    self.server.rpc_cookie,
                                    self.server.rpc_timeout)
            LOG.info("http broadcast ok txid=%s", txid)
            self._reply({"type": "broadcast-result", "id": rid,
                         "txid": txid, "error": None})
        except RpcError as e:
            LOG.warning("http broadcast failed: %s", e)
            self._reply({"type": "broadcast-result", "id": rid,
                         "txid": None, "error": str(e)},
                        status=400 if "bad tx" in str(e) else 502)
        except Exception as e:
            LOG.warning("http broadcast failed: %s", e)
            self._reply({"type": "broadcast-result", "id": rid,
                         "txid": None, "error": str(e)}, status=502)

    def _reply(self, payload: dict, status: int = 200):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class RelayHttpServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, addr, handler, rpc_url, rpc_cookie, rpc_timeout):
        self.rpc_url = rpc_url
        self.rpc_cookie = rpc_cookie
        self.rpc_timeout = rpc_timeout
        super().__init__(addr, handler)


# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description="surf-relay gateway for Passport Prime browser")
    ap.add_argument("--port", type=int, default=8787)
    # Localhost by default: broadcasting is sensitive, and the simulator talks
    # to 127.0.0.1. Only open the LAN (--bind 0.0.0.0) when a future
    # BLE-bridge/companion needs to reach it, behind your own firewall rules.
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument("--socks", default=None, help="Tor SOCKS5 proxy host:port (e.g. 127.0.0.1:9050)")
    ap.add_argument("--timeout", type=int, default=30)
    ap.add_argument("--max-bytes", type=int, default=524288)
    ap.add_argument("--self-test", default=None, help="fetch a URL once, print blocks, exit")
    ap.add_argument("--self-broadcast", default=None,
                    help="submit a raw tx hex to the node once, print txid, exit")
    ap.add_argument("--self-qrng", type=int, nargs="?", const=64, default=None,
                    help="fetch N quantum bytes from ANU once, print hex, exit")
    ap.add_argument("--rpc-url", default=os.environ.get("SURF_RPC_URL", "http://127.0.0.1:8332"),
                    help="bitcoind JSON-RPC endpoint (default: your node on 8332)")
    ap.add_argument("--rpc-cookie",
                    default=os.environ.get("SURF_RPC_COOKIE", os.path.expanduser("~/.bitcoin/.cookie")),
                    help="bitcoind cookie file (default: ~/.bitcoin/.cookie)")
    ap.add_argument("--rpc-timeout", type=int, default=30)
    ap.add_argument("--http-port", type=int, default=8788,
                    help="companion HTTP endpoint (POST /broadcast); 0 disables")
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

    if args.self_broadcast:
        txid = submit_broadcast({"txhex": args.self_broadcast}, args.rpc_url,
                                args.rpc_cookie, args.rpc_timeout)
        print(f"broadcast ok txid={txid}")
        return 0

    if args.self_qrng is not None:
        data = fetch_anu_entropy(args.self_qrng, build_opener(args.socks))
        print(f"qrng ok length={len(data)} bytes={data.hex()}")
        return 0

    opener = build_opener(args.socks)
    server = RelayServer((args.bind, args.port), RelayHandler, opener,
                         args.timeout, args.max_bytes,
                         args.rpc_url, args.rpc_cookie, args.rpc_timeout)
    via = f"via Tor {args.socks}" if args.socks else "clearnet"
    LOG.info("surf-relay listening on %s:%d (%s) — broadcast node %s",
             args.bind, args.port, via, args.rpc_url)
    servers = [server]
    if args.http_port:
        httpd = RelayHttpServer((args.bind, args.http_port), RelayHttpHandler,
                                args.rpc_url, args.rpc_cookie, args.rpc_timeout)
        servers.append(httpd)
        LOG.info("surf-relay HTTP /broadcast on %s:%d", args.bind, args.http_port)
    try:
        import threading
        for s in servers[1:]:
            threading.Thread(target=s.serve_forever, daemon=True).start()
        servers[0].serve_forever()
    except KeyboardInterrupt:
        LOG.info("bye")


if __name__ == "__main__":
    sys.exit(main())
