"""Profile-local settings. Credentials are always resolved by Hermes, never stored here."""

from dataclasses import asdict, dataclass
from pathlib import Path
from urllib.parse import urlsplit

DEFAULT_URL = "https://api.thememoria.ai"


@dataclass(frozen=True)
class Config:
    api_url: str = DEFAULT_URL
    branch: str = "main"
    auto_recall: bool = True
    # Opt-in: setup explains that completed user/assistant turns leave the device.
    auto_capture: bool = False
    top_k: int = 5
    context_chars: int = 6000
    recall_timeout: float = 2.0
    request_timeout: float = 15.0
    max_capture_chars: int = 24000
    queue_capacity: int = 1000

    @classmethod
    def load(cls, home: Path) -> "Config":
        path = home / "memoria.json"
        if not path.exists():
            return cls()
        import json

        values = json.loads(path.read_text(encoding="utf-8"))
        return cls.from_values(values)

    @classmethod
    def from_values(cls, values: dict) -> "Config":
        if not isinstance(values, dict):
            raise ValueError("memoria.json must contain an object")  # noqa: TRY004
        unknown = set(values) - set(asdict(cls()))
        if unknown:
            raise ValueError("Unknown Memoria config fields: " + ", ".join(sorted(unknown)))
        cfg = cls(**values)
        if not isinstance(cfg.api_url, str):
            raise ValueError("api_url must be a string")  # noqa: TRY004
        parsed = urlsplit(cfg.api_url)
        if (
            parsed.scheme not in {"https", "http"}
            or not parsed.hostname
            or parsed.username
            or parsed.password
            or parsed.query
            or parsed.fragment
            or parsed.path not in {"", "/"}
        ):
            raise ValueError("api_url must be an HTTP(S) origin without credentials or path")
        if parsed.scheme == "http" and parsed.hostname not in {"localhost", "127.0.0.1", "::1"}:
            raise ValueError("Use HTTPS for remote Memoria deployments")
        if not isinstance(cfg.branch, str) or not cfg.branch.strip():
            raise ValueError("branch must be non-empty")
        for key in ("auto_recall", "auto_capture"):
            if type(getattr(cfg, key)) is not bool:
                raise ValueError(f"{key} must be a JSON boolean")
        limits = {
            "top_k": (1, 20),
            "context_chars": (512, 20000),
            "max_capture_chars": (1000, 100000),
            "queue_capacity": (1, 10000),
        }
        for key, (low, high) in limits.items():
            value = getattr(cfg, key)
            if type(value) is not int or not low <= value <= high:
                raise ValueError(f"{key} must be an integer between {low} and {high}")
        for key in ("recall_timeout", "request_timeout"):
            value = getattr(cfg, key)
            if type(value) not in {int, float} or not 0.1 <= value <= 60:
                raise ValueError(f"{key} must be between 0.1 and 60 seconds")
        return cfg


def home_path() -> Path:
    from hermes_constants import get_hermes_home

    return Path(get_hermes_home()).resolve()


def api_key() -> str:
    from agent.secret_scope import get_secret

    return (get_secret("MEMORIA_API_KEY") or "").strip()


def writes_blocked() -> bool:
    """External writes have no native approval replay handler; pause them when gated."""
    try:
        from hermes_cli.config import cfg_get, load_config
        from utils import is_truthy_value

        return is_truthy_value(cfg_get(load_config(), "memory", "write_approval", default=False))
    except Exception:  # noqa: BLE001 — unknown approval state blocks external writes
        return True
