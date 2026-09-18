"""HTTP transport for the generated aurix-dbg API methods, plus helpers that span several calls."""

from __future__ import annotations

import json
from typing import Any, Iterator

import requests

from ._generated import MAX_TRANSFER, GeneratedClient


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
