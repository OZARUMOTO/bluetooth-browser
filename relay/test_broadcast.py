#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Michael Totten <mike@ozaru.io>
# SPDX-License-Identifier: GPL-3.0-or-later
"""
Tests for the surf-relay `broadcast` envelope (signed tx -> your bitcoind).

Runs the real surf_relay.py server in-process, fronted by a *fake* bitcoind
RPC endpoint, so the whole path is exercised without touching a real node:

    fake bitcoind RPC  <-relay submits->  surf-relay TCP  <-envelope->  client

Usage:  python3 test_broadcast.py
"""

import http.server
import json
import os
import socket
import sys
import threading

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import surf_relay  # noqa: E402

FAKE_TXID = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"
FAKE_TXHEX = "02000000000101" + "00" * 40


class FakeBitcoind(http.server.BaseHTTPRequestHandler):
    """Minimal JSON-RPC stub: sendrawtransaction, finalizepsbt; flags to reject."""

    reject = False      # sendrawtransaction rejects (node-level error)
    incomplete = False  # finalizepsbt returns complete=False
    finalized = False   # set True once finalizepsbt was called (proves PSBT path)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        req = json.loads(self.rfile.read(length))
        if req.get("method") == "sendrawtransaction":
            if self.reject:
                self._reply({"error": {"code": -26, "message": "bad-txns-inputs-missingorspent"}})
            else:
                self._reply({"result": FAKE_TXID})
        elif req.get("method") == "finalizepsbt":
            type(self).finalized = True
            if self.incomplete:
                self._reply({"result": {"complete": False, "hex": ""}})
            else:
                self._reply({"result": {"complete": True, "hex": FAKE_TXHEX}})
        elif req.get("method") == "getblockchaininfo":
            self._reply({"result": {"blocks": 917287}})
        else:
            self._reply({"error": {"code": -32601, "message": "method not found"}})

    def _reply(self, payload):
        body = json.dumps({"jsonrpc": "1.0", "id": "test", **payload}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start_fake_node():
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeBitcoind)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd, f"http://127.0.0.1:{httpd.server_port}"


def start_relay(rpc_url, cookie_path, reject_on=()):
    """Start surf_relay in-process. Returns (server, port, stop_fn)."""
    server = surf_relay.RelayServer(
        ("127.0.0.1", 0), surf_relay.RelayHandler,
        opener=surf_relay.build_opener(None),
        fetch_timeout=10, max_bytes=65536,
        rpc_url=rpc_url, rpc_cookie=cookie_path, rpc_timeout=10,
    )
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, port


def client_exchange(port, payload):
    """One envelope in, one reply out (newline-delimited JSON over TCP)."""
    with socket.create_connection(("127.0.0.1", port), timeout=10) as s:
        s.sendall((json.dumps(payload) + "\n").encode())
        line = s.makefile("rb").readline()
    return json.loads(line)


def write_cookie(path, user="__cookie__", password="s3cret"):
    with open(path, "w", encoding="utf-8") as f:
        f.write(f"{user}:{password}")


def run():
    results = []

    with open(os.devnull, "w") as dn:
        import contextlib
        with contextlib.redirect_stdout(dn):
            # 1) happy path: valid hex -> node-confirmed txid
            node, rpc_url = start_fake_node()
            cookie = os.path.join(HERE, ".test_cookie")
            write_cookie(cookie)
            server, port = start_relay(rpc_url, cookie)
            try:
                reply = client_exchange(port, {"type": "broadcast", "id": "12",
                                               "txhex": "02000000000101" * 8})
                assert reply["type"] == "broadcast-result", reply
                assert reply["id"] == "12", reply
                assert reply["txid"] == FAKE_TXID, reply
                assert reply["error"] is None, reply
                results.append("ok   broadcast happy path -> txid")

                # 2) bad hex -> clean error, no RPC call
                reply = client_exchange(port, {"type": "broadcast", "id": "13",
                                               "txhex": "nothex!!"})
                assert reply["type"] == "broadcast-result", reply
                assert reply["txid"] is None and reply["error"], reply
                results.append("ok   bad txhex rejected locally")

                # 3) node rejection -> error surfaced with RPC message
                FakeBitcoind.reject = True
                reply = client_exchange(port, {"type": "broadcast", "id": "14",
                                               "txhex": "02000000000101" * 8})
                assert reply["type"] == "broadcast-result", reply
                assert reply["txid"] is None, reply
                assert "bad-txns-inputs-missingorspent" in reply["error"], reply
                results.append("ok   node rejection surfaced")

                # 4) missing cookie -> clean error
                server2, port2 = start_relay(rpc_url, os.path.join(HERE, ".nope_cookie"))
                reply = client_exchange(port2, {"type": "broadcast", "id": "15",
                                                "txhex": "02000000000101" * 8})
                assert reply["txid"] is None and "cookie" in reply["error"], reply
                results.append("ok   missing cookie -> clean error")

                # 5) ping still works on the same server
                reply = client_exchange(port, {"type": "ping"})
                assert reply == {"type": "pong"}, reply
                results.append("ok   ping unaffected")

                # 6) PSBT path: relay sends the base64 to finalizepsbt, node
                #    returns raw hex, relay then broadcasts it -> txid
                FakeBitcoind.reject = False
                FakeBitcoind.finalized = False
                FakeBitcoind.incomplete = False
                reply = client_exchange(port, {"type": "broadcast", "id": "16",
                                               "psbt": "cHNidP8BAH0CAAAA"})
                assert reply["type"] == "broadcast-result", reply
                assert reply["txid"] == FAKE_TXID and reply["error"] is None, reply
                assert FakeBitcoind.finalized, "finalizepsbt was never called"
                results.append("ok   psbt -> finalizepsbt -> broadcast -> txid")

                # 7) incomplete finalization -> clean error, no broadcast
                FakeBitcoind.incomplete = True
                reply = client_exchange(port, {"type": "broadcast", "id": "17",
                                               "psbt": "cHNidP8BAH0CAAAA"})
                assert reply["type"] == "broadcast-result", reply
                assert reply["txid"] is None, reply
                assert "incomplete" in reply["error"], reply
                results.append("ok   incomplete psbt -> clean error")
            finally:
                server.shutdown()
                server2.shutdown()
                node.shutdown()
                os.unlink(cookie)

    print("\n".join(results))
    print(f"\n{len(results)}/7 relay broadcast checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(run())
