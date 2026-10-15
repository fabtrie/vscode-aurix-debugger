"""Python client of the aurix-dbg debug server's REST API.

The API methods and types are generated from the server's API definition
(``server/crates/api-spec``) by ``cargo build``; see ``_generated.py``.
"""

from ._generated import *  # noqa: F401,F403
from ._generated import API_VERSION, MAX_TRANSFER
from .client import ApiError, Client
from .daq import DaqBatch, DaqError, DaqPoint, DaqStream

__all__ = [
    "Client",
    "ApiError",
    "API_VERSION",
    "MAX_TRANSFER",
    "DaqStream",
    "DaqBatch",
    "DaqPoint",
    "DaqError",
]
