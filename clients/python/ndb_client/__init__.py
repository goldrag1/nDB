"""nDB Python client — pure-Python HTTP shim.

Public surface mirrors the ``ndb`` CLI binary. See README.md for usage.

The implementation uses stdlib only (``urllib``, ``json``, ``ssl``) so
the install footprint is zero. Optional Arrow interop is offered when
``pyarrow`` is installed; see :func:`Client.iter_arrow`.
"""

from .client import (
    Client,
    NdbError,
    NdbHttpError,
    NdbConnectionError,
)

__all__ = [
    "Client",
    "NdbError",
    "NdbHttpError",
    "NdbConnectionError",
]

__version__ = "1.1.0"
