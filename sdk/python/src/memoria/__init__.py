"""Memoria Python SDK.

Quick start::

    from memoria import MemoriaClient

    with MemoriaClient(base_url="http://localhost:8100", api_key="sk-...") as client:
        client.ping()
        mem = client.memories.store(content="Prefers concise answers", memory_type="profile")
        result = client.memories.retrieve(query="answer style")
        print(result.items[0].content)

Async usage::

    from memoria import AsyncMemoriaClient

    async with AsyncMemoriaClient(base_url="http://localhost:8100", api_key="sk-...") as client:
        await client.ping()
        mem = await client.memories.store(content="...", memory_type="semantic")
"""

from .client import AsyncMemoriaClient, MemoriaClient
from .exceptions import (
    MemoriaAPIError,
    MemoriaAuthError,
    MemoriaConnectionError,
    MemoriaError,
    MemoriaForbiddenError,
    MemoriaNotFoundError,
    MemoriaServerError,
    MemoriaUnprocessableError,
    MemoriaValidationError,
)
from .models import (
    ApplyResult,
    Branch,
    DisabledMemory,
    DisabledMemoryPage,
    GovernanceResult,
    Memory,
    MemoryPage,
    ObserveResult,
    Profile,
    PurgeResult,
    RetrieveResult,
    Snapshot,
)

try:
    from importlib.metadata import version as _pkg_version

    __version__: str = _pkg_version("memoria-client")
except Exception:
    __version__ = "dev"

__all__ = [
    # clients
    "MemoriaClient",
    "AsyncMemoriaClient",
    # exceptions
    "MemoriaError",
    "MemoriaConnectionError",
    "MemoriaAPIError",
    "MemoriaAuthError",
    "MemoriaForbiddenError",
    "MemoriaNotFoundError",
    "MemoriaUnprocessableError",
    "MemoriaServerError",
    "MemoriaValidationError",
    # models
    "Memory",
    "DisabledMemory",
    "DisabledMemoryPage",
    "MemoryPage",
    "RetrieveResult",
    "PurgeResult",
    "Snapshot",
    "Branch",
    "ApplyResult",
    "Profile",
    "GovernanceResult",
    "ObserveResult",
    # version
    "__version__",
]
