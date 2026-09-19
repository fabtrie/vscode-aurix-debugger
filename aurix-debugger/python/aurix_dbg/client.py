"""HTTP transport for the generated aurix-dbg API methods, plus helpers that span several calls."""

from __future__ import annotations

import json
from typing import Any, Iterator

import requests

from ._generated import MAX_TRANSFER, GeneratedClient
from .daq import DaqStream


class ApiError(Exception):
    """An operation failed. `code` is the API error kind (e.g. ``not_halted``, ``timeout``)."""

    def __init__(self, status: int, code: str, message: str):
        super().__init__(f"{code}: {message}")
        self.status = status
        self.code = code
        self.message = message


class Client(GeneratedClient):
    """Client of the aurix-dbg REST API (``api_port``, default 8080).

    All API operations are methods (generated from the API definition), e.g.::

        dbg = Client()
        dbg.halt("tc37x.cpu0")
        print(hex(dbg.read_register("tc37x.cpu0", "pc").value))
        data = dbg.read_bytes(0x70000000, 16)
    """

    def __init__(self, url: str = "http://127.0.0.1:8080", timeout: float | None = None, session: requests.Session | None = None):
        """``timeout``: seconds per request; None waits as long as an operation runs
        (flash programming, ``wait_state``)."""
        self.url = url.rstrip("/")
        self.timeout = timeout
        self.session = session or requests.Session()

    def _call(self, method: str, path: str, query: dict[str, Any] | None, body: dict[str, Any] | None) -> Any:
        r = self.session.request(method, self.url + path, params=query, json=body, timeout=self.timeout)
        if r.status_code >= 400:
            try:
                e = r.json()
                raise ApiError(r.status_code, e.get("code", "failed"), e.get("message", r.text))
            except ValueError:
                raise ApiError(r.status_code, "failed", r.text) from None
        return r.json()

    def read_bytes(self, address: int, length: int, target: str | None = None) -> bytes:
        """Reads memory of any length (split into ``MAX_TRANSFER`` sized calls)."""
        out = bytearray()
        while len(out) < length:
            n = min(MAX_TRANSFER, length - len(out))
            out += bytes.fromhex(self.read_memory(address=address + len(out), length=n, target=target).data)
        return bytes(out)

    def write_bytes(self, address: int, data: bytes, target: str | None = None) -> None:
        """Writes memory of any length (split into ``MAX_TRANSFER`` sized calls)."""
        for off in range(0, len(data), MAX_TRANSFER):
            self.write_memory(address=address + off, data=data[off:off + MAX_TRANSFER].hex(), target=target)

    def daq(self, *points: Any, rate_hz: float = 100, mode: str = "every_tick",
            queue: int | None = None, timeout: float | None = 10) -> DaqStream:
        """Samples variables periodically and returns the open stream of their values.

        Each point is a variable or C expression (``"g_speed"``), an ``(expression, rate_hz)``
        pair, or a dict with the full specification: ``expression`` or ``address`` with ``size``,
        plus ``rate_hz`` or ``interval_us``, and optionally ``name``, ``target``, ``decode``,
        ``mode`` and ``resend_after_us``. Points without their own rate use ``rate_hz``.

        Rates are rounded up to the server's 1 ms grid; each point reports what it was granted.
        What the probe sustains is lower than the grid allows: a tick costs one round trip
        (~1.4 ms on a miniWiggler at 30 MHz) however many points it carries, so count on ~200 Hz
        per session and expect ~700 Hz at best. Ticks that could not be served are counted in
        ``DaqStream.missed`` and ``DaqBatch.missed`` rather than hidden. Put several signals in
        one session instead of opening a session each - extra points in a tick are nearly free.
        Only globals and statics can be sampled - a frame local lives in a register and needs a
        halted target. Names need symbols: call ``load_symbols`` first, or have a VS Code debug
        session open.

        ``mode="on_change"`` delivers a point only when its value changes. The point is still
        read at its full rate, so this saves stream bandwidth, not target accesses; set
        ``resend_after_us`` if a quiet signal should still produce a value now and then.

        Sampling stops when the stream is closed, so use it as a context manager::

            with dbg.daq(("g_speed", 1000), "g_state") as daq:
                for batch in daq:
                    print(batch.t_us, batch.values)
        """
        url = self.url.replace("https://", "wss://", 1).replace("http://", "ws://", 1)
        return DaqStream(url + "/api/v1/daq", points, rate_hz, mode, queue, timeout)

    def events(self) -> Iterator[dict[str, Any]]:
        """Yields the events of all clients (halts, resumes, breakpoint changes, log lines, ...)
        as dicts with a ``type`` key, until the connection closes. See ``ApiEvent`` in the
        OpenAPI document (``/docs``)."""
        with self.session.get(self.url + "/api/v1/events", stream=True, timeout=(10, None)) as r:
            r.raise_for_status()
            data: list[str] = []
            for line in r.iter_lines(decode_unicode=True):
                if line is None:
                    continue
                if line.startswith("data:"):
                    data.append(line[5:].lstrip())
                elif line == "" and data:
                    yield json.loads("\n".join(data))
                    data = []
