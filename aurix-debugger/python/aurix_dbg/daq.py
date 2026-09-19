"""Periodic sampling (DAQ) over the server's WebSocket at ``/api/v1/daq``.

Sampling runs only while the socket is open. Closing the stream - or crashing - stops it, so a
script can never leave the debug probe busy.

Rates are per point. ``mode="on_change"`` delivers a point only when its value actually changes;
it is a *delivery* filter, so it saves bandwidth and not target accesses - the point is still read
at its full rate, because that is how a change is noticed.
"""

from __future__ import annotations

import json
import threading
from typing import Any, Dict, Iterator, List, Optional, Sequence, Union

import websocket  # websocket-client


class DaqError(Exception):
    """A DAQ request was refused. ``code`` is the API error kind."""

    def __init__(self, code: str, message: str):
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


class DaqPoint:
    """One configured point, as the server resolved it.

    ``interval_us`` is the period actually granted (the request is rounded up to the server's
    1 ms grid); ``error`` is set when the point could not be resolved, in which case it is not
    sampled while the other points still are.
    """

    __slots__ = ("id", "name", "expression", "target", "address", "size", "bits", "type_name",
                 "encoding", "mode", "requested_interval_us", "interval_us", "rate_hz", "error")

    def __init__(self, d: Dict[str, Any]):
        self.id = d.get("id")
        self.name = d.get("name", "")
        self.expression = d.get("expression")
        self.target = d.get("target")
        self.address = d.get("address")
        self.size = d.get("size")
        self.bits = d.get("bits")
        self.type_name = d.get("type_name")
        self.encoding = d.get("encoding", "hex")
        self.mode = d.get("mode", "every_tick")
        self.requested_interval_us = d.get("requested_interval_us", 0)
        self.interval_us = d.get("interval_us", 0)
        self.rate_hz = d.get("rate_hz", 0.0)
        self.error = d.get("error")

    def __repr__(self) -> str:
        at = "" if self.address is None else " @0x%08x" % self.address
        bad = " ERROR: %s" % self.error if self.error else ""
        return "<DaqPoint %s%s every %s us %s%s>" % (self.name, at, self.interval_us, self.encoding, bad)


class DaqBatch:
    """One tick: the points that were due and passed their change filter.

    Only those points appear in ``values``. With ``on_change`` points that is usually a subset,
    so carry values forward yourself or read :attr:`DaqStream.latest`.
    """

    __slots__ = ("t_us", "values", "by_id", "dropped", "missed")

    def __init__(self, t_us: int, values: Dict[str, Any], by_id: Dict[int, Any], dropped: int, missed: int):
        self.t_us = t_us
        self.values = values
        self.by_id = by_id
        #: Batches the server discarded before this one because this client was too slow.
        self.dropped = dropped
        #: Ticks the server could not sample before this one - the requested rate was not met,
        #: or the core thread was busy (flashing, a long Tcl script).
        self.missed = missed

    def __repr__(self) -> str:
        flags = ("" if not self.dropped else " DROPPED %d" % self.dropped) + ("" if not self.missed else " MISSED %d" % self.missed)
        return "<DaqBatch t=%d us %r%s>" % (self.t_us, self.values, flags)


# A point: "g_x", ("g_x", 1000), or a dict of the full specification.
PointSpec = Union[str, tuple, Dict[str, Any]]


def _spec(p: PointSpec, rate_hz: Optional[float], mode: str) -> Dict[str, Any]:
    if isinstance(p, str):
        d = {"expression": p}  # type: Dict[str, Any]
    elif isinstance(p, tuple):
        if len(p) != 2:
            raise ValueError("a point tuple is (expression, rate_hz)")
        d = {"expression": p[0], "rate_hz": p[1]}
    elif isinstance(p, dict):
        d = dict(p)
    else:
        raise TypeError("a point is a name, an (expression, rate_hz) pair or a dict, not %r" % type(p))
    if "rate_hz" not in d and "interval_us" not in d and rate_hz is not None:
        d["rate_hz"] = rate_hz
    d.setdefault("mode", mode)
    return d


class DaqStream:
    """An open DAQ session. Iterate it for :class:`DaqBatch`es.

    Use it as a context manager so the socket - and with it the sampling - is closed even if the
    body raises::

        with dbg.daq(("g_speed", 1000)) as daq:
            for batch in daq:
                ...

    Reconfiguring from inside the loop is fine, as the example in :meth:`Client.daq` does: the
    generator is suspended while the body runs. Batches that arrive while a request waits for its
    reply are folded into :attr:`latest` rather than yielded, so a config change can swallow a few
    ticks. One stream is **not** safe to use from several threads at once - both would read the
    same socket; give each thread its own stream.
    """

    def __init__(self, url: str, points: Sequence[PointSpec], rate_hz: Optional[float],
                 mode: str, queue: Optional[int], timeout: Optional[float]):
        # Set before connecting: __del__ may run if the handshake raises.
        self._closed = True
        self._ws = None  # type: Any
        self._next_id = 0
        self._lock = threading.Lock()
        #: Last value of every point by name, updated as batches arrive.
        self.latest = {}  # type: Dict[str, Any]
        #: Batches the server dropped in total because this client did not keep up.
        self.dropped = 0
        #: Ticks the server could not sample at all, in total. A steady count means the requested
        #: rate is above what the probe sustains; a burst means something blocked the core thread.
        self.missed = 0
        #: Last reported session state: running, reset, unavailable or stalled.
        self.state = "running"
        #: Points that failed to read, as (point id, message, muted).
        self.point_errors = []  # type: List[tuple]
        self._pending_dropped = 0
        self._pending_missed = 0
        # A non-browser client sends no Origin; the server only rejects foreign ones.
        self._ws = websocket.create_connection(url, timeout=timeout, suppress_origin=True)
        self._closed = False
        req = {"op": "subscribe", "points": [_spec(p, rate_hz, mode) for p in points]}
        if queue is not None:
            req["queue"] = queue
        cfg = self._request(req, "config")
        #: Unix milliseconds of ``t_us == 0``.
        self.start_wall_ms = cfg.get("start_wall_ms", 0)
        self.points = [DaqPoint(p) for p in cfg.get("points", [])]
        self._names = {p.id: p.name for p in self.points if p.id is not None}
        # From here on, block indefinitely: an on_change stream may legitimately stay quiet for
        # a long time, and a dead socket raises rather than hanging.
        self._ws.settimeout(None)

    # --- configuration ------------------------------------------------------------------------

    def _send(self, req: Dict[str, Any]) -> int:
        self._next_id += 1
        req["id"] = self._next_id
        self._ws.send(json.dumps(req))
        return self._next_id

    def _request(self, req: Dict[str, Any], want: str) -> Dict[str, Any]:
        """Sends a request and returns its reply, passing samples that arrive first to `latest`."""
        with self._lock:
            rid = self._send(req)
            while True:
                m = json.loads(self._ws.recv())
                if m.get("type") == "error" and m.get("id") == rid:
                    raise DaqError(m.get("code", "failed"), m.get("message", ""))
                if m.get("type") == want and m.get("id") == rid:
                    return m
                self._absorb(m)

    def _point_id(self, point: Union[int, str]) -> int:
        if isinstance(point, int):
            return point
        for p in self.points:
            if p.name == point and p.id is not None:
                return p.id
        raise KeyError("no DAQ point named %r" % point)

    def set_rate(self, point: Union[int, str], rate_hz: Optional[float] = None,
                 interval_us: Optional[int] = None) -> int:
        """Retimes a point on the open stream. Returns the period actually granted."""
        req = {"op": "set_rate", "point": self._point_id(point)}  # type: Dict[str, Any]
        if interval_us is not None:
            req["interval_us"] = interval_us
        elif rate_hz is not None:
            req["rate_hz"] = rate_hz
        else:
            raise ValueError("give rate_hz or interval_us")
        return self._request(req, "changed").get("interval_us", 0)

    def set_mode(self, point: Union[int, str], mode: str) -> None:
        """Switches a point between ``every_tick`` and ``on_change``.

        The change-detection baseline is dropped, so the next read is delivered.
        """
        self._request({"op": "set_mode", "point": self._point_id(point), "mode": mode}, "changed")

    def add(self, *points: PointSpec, **kw: Any) -> List[DaqPoint]:
        """Adds points to the running session. Returns the new points, errors included."""
        rate_hz = kw.pop("rate_hz", None)
        mode = kw.pop("mode", "every_tick")
        if kw:
            raise TypeError("unexpected arguments %s" % ", ".join(kw))
        m = self._request({"op": "add", "points": [_spec(p, rate_hz, mode) for p in points]}, "changed")
        new = [DaqPoint(p) for p in m.get("points", [])]
        self.points.extend(new)
        self._names.update({p.id: p.name for p in new if p.id is not None})
        return new

    def remove(self, *points: Union[int, str]) -> None:
        ids = [self._point_id(p) for p in points]
        self._request({"op": "remove", "points": ids}, "changed")
        self.points = [p for p in self.points if p.id not in ids]
        for i in ids:
            self._names.pop(i, None)

    def pause(self) -> None:
        """Stops sampling without dropping the session."""
        self._request({"op": "pause"}, "changed")

    def resume(self) -> None:
        self._request({"op": "resume"}, "changed")

    def status(self) -> Dict[str, Any]:
        """Point list and the number of batches dropped because this client was too slow."""
        return self._request({"op": "status"}, "status")

    # --- the stream ---------------------------------------------------------------------------

    def _absorb(self, m: Dict[str, Any]) -> Optional[DaqBatch]:
        """Turns one server frame into a batch, or records it and returns None."""
        kind = m.get("type")
        if kind == "overrun":
            # Reported on its own frame, just before the samples that follow the gap.
            n = m.get("dropped", 0)
            self.dropped += n
            self._pending_dropped += n
            return None
        if kind == "gap":
            n = m.get("missed", 0)
            self.missed += n
            self._pending_missed += n
            return None
        if kind == "state":
            self.state = m.get("state", self.state)
            return None
        if kind == "point_error":
            self.point_errors.append((m.get("point"), m.get("message", ""), m.get("muted", False)))
            return None
        if kind != "samples":
            return None
        ids = m.get("points", [])
        vals = m.get("values", [])
        by_id = {}  # type: Dict[int, Any]
        values = {}  # type: Dict[str, Any]
        for i, pid in enumerate(ids):
            v = vals[i] if i < len(vals) else None
            by_id[pid] = v
            name = self._names.get(pid)
            if name is not None:
                values[name] = v
                self.latest[name] = v
        dropped, missed = self._pending_dropped, self._pending_missed
        self._pending_dropped = self._pending_missed = 0
        return DaqBatch(m.get("t_us", 0), values, by_id, dropped, missed)

    def __iter__(self) -> Iterator[DaqBatch]:
        while not self._closed:
            try:
                raw = self._ws.recv()
            except Exception:
                return
            if not raw:
                return
            m = json.loads(raw)
            if m.get("type") == "closed":
                return
            batch = self._absorb(m)
            if batch is not None:
                yield batch

    # --- lifetime -----------------------------------------------------------------------------

    def close(self) -> None:
        """Closes the socket, which stops the sampling on the server."""
        if not getattr(self, "_closed", True):
            self._closed = True
            try:
                self._ws.close()
            except Exception:
                pass

    def __enter__(self) -> "DaqStream":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def __del__(self) -> None:
        try:
            self.close()
        except Exception:
            pass
