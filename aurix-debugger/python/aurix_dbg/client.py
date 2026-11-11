"""HTTP transport for the generated aurix-dbg API methods, plus helpers that span several calls."""

from __future__ import annotations

import json
import os
import uuid
from typing import Any, Iterator
from urllib.parse import quote

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
    """Client of the aurix-dbg hub (``aurix-hub``, port 3001).

    All API operations are methods (generated from the API definition). The hub's own operations
    (``open_session``, ``list_sessions``, ...) work on the hub; the session operations go to
    ``session`` (an ID or name), or to the only open session when it is None::

        dbg = Client()
        dbg.open_session(profile="my-board", launch={"run_to": "main"})
        dbg.halt("tc37x.cpu0")
        print(hex(dbg.read_register("tc37x.cpu0", "pc").value))
        data = dbg.read_bytes(0x70000000, 16)

    Each client has its own ID (``X-Aurix-Client``): it owns the breakpoints it sets and the
    control lease it takes, and ``end()`` removes them again.
    """

    def __init__(self, url: str = "http://127.0.0.1:3001", session: str | None = None, timeout: float | None = None,
                 token: str | None = None, client_id: str | None = None, http: requests.Session | None = None):
        """``session``: the session the session operations go to (default: the only open one).
        ``timeout``: seconds per request; None waits as long as an operation runs (flash
        programming, ``wait_state``). ``token``: the hub's token, needed from another machine
        (default: the ``AURIX_HUB_TOKEN`` environment variable). ``client_id``: this client's
        name (default: a new ``py:<uuid>``)."""
        self.url = url.rstrip("/")
        self.session = session
        self.timeout = timeout
        self.client_id = client_id or f"py:{uuid.uuid4().hex[:12]}"
        token = token if token is not None else os.environ.get("AURIX_HUB_TOKEN")
        if http is None:
            http = requests.Session()
            # The hub is usually local, so never look for a proxy. Left on (the requests
            # default), every single call pays a Windows proxy-configuration lookup: measured
            # 21 ms versus 0.5 ms here. Pass your own `http` session to get the usual behaviour.
            http.trust_env = False
        http.headers["X-Aurix-Client"] = self.client_id
        if token:
            http.headers["Authorization"] = f"Bearer {token}"
        self.http = http

    def for_session(self, session: str) -> "Client":
        """A client of the same hub and ID whose session operations go to ``session``."""
        c = Client.__new__(Client)
        c.url, c.session, c.timeout, c.client_id, c.http = self.url, session, self.timeout, self.client_id, self.http
        return c

    def _session_path(self) -> str:
        if self.session:
            return "/api/v1/sessions/" + quote(self.session, safe="")
        return "/api/v1/session"

    def _call(self, method: str, path: str, query: dict[str, Any] | None, body: dict[str, Any] | None) -> Any:
        r = self.http.request(method, self.url + path, params=query, json=body, timeout=self.timeout)
        if r.status_code >= 400:
            try:
                e = r.json()
                raise ApiError(r.status_code, e.get("code", "failed"), e.get("message", r.text))
            except ValueError:
                raise ApiError(r.status_code, "failed", r.text) from None
        return r.json()

    def upload(self, path: str) -> str:
        """Uploads a file to the hub machine and returns its ID (``file:<sha256>/<name>``), which
        works wherever an operation takes a path (``load_symbols``, ``flash_write``, the launch
        steps). Needed when the hub runs on another machine."""
        with open(path, "rb") as f:
            r = self.http.post(self.url + "/api/v1/files", params={"name": os.path.basename(path)}, data=f,
                               headers={"Content-Type": "application/octet-stream"}, timeout=self.timeout)
        if r.status_code >= 400:
            raise ApiError(r.status_code, "failed", r.text)
        return r.json()["file"]

    def end(self) -> None:
        """Ends this client on its session: removes its breakpoints and watchpoints and
        releases its control lease."""
        self.end_client(client=self.client_id)

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

        Rates are rounded up to the servers 100 us grid; each point reports what it was granted.

        **A rate is a floor, not a grid.** You get at least what you asked for; individual
        samples land where they land. An OS sleep cannot resolve better than ~0.5 ms, so the
        server schedules on a shorter period and corrects it from the rate actually achieved.
        Measured at 1 kHz: ~990 samples/s for ~8 % of a core, p95 spacing ~3.5 ms. Every sample
        carries its own timestamp, so the placement is known even when the spacing varies - do
        not assume a fixed step between consecutive values. The ceiling is ~1.5 kHz, because a
        sleep of any non-zero length costs a full ~512 us quantum.

        Over-delivering does not save anything: the core loop is flat but decoding each sample is
        not, so CPU tracks the delivered count (1000/s -> 10 %, 1860/s -> 17 %). Ask for the rate
        you need.

        The probe is the hard limit: a tick costs ~123 us for its first bus access and ~57 us for
        each one after it, while an extra byte inside a read already being made costs well under a
        microsecond. The server therefore folds points closer than 96 bytes into one read (never
        across peripheral space). **Lay the variables you sample together out together** - 12
        points 12 bytes apart are 3x cheaper than 12 points 2 KB apart, and 128 adjacent words
        sustain 129 000 values/s. Point count costs stream CPU, not sampling rate: the core thread
        is flat at 3-5 %% whether you sample 1 point or 128.

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
        headers = [f"{k}: {v}" for k, v in self.http.headers.items() if k in ("X-Aurix-Client", "Authorization")]
        return DaqStream(url + self._session_path() + "/daq", points, rate_hz, mode, queue, timeout, headers)

    def events(self) -> Iterator[dict[str, Any]]:
        """Yields the events of the session's clients (halts, resumes, breakpoint changes, log
        lines, ...) as dicts with a ``type`` key, until the connection closes. See ``ApiEvent``
        in the OpenAPI document (``/docs``)."""
        return self._sse(self._session_path() + "/events")

    def hub_events(self) -> Iterator[dict[str, Any]]:
        """Yields the hub's events (sessions opened, launched, closed) as dicts with a ``type``
        key. See ``HubEvent`` in the OpenAPI document."""
        return self._sse("/api/v1/events")

    def _sse(self, path: str) -> Iterator[dict[str, Any]]:
        with self.http.get(self.url + path, stream=True, timeout=(10, None)) as r:
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
