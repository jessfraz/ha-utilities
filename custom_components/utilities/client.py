"""Bounded transport to one configured service origin."""

import json
from dataclasses import dataclass
from urllib.parse import urlsplit

from aiohttp import ClientSession, ClientTimeout

from .const import MAX_RESPONSE_BYTES, REQUEST_TIMEOUT_SECONDS


class InvalidBackendURL(ValueError):
    """An origin was not explicit or included credentials."""


class ResponseTooLarge(ValueError):
    """The backend exceeded the response budget."""


def normalize_backend_url(value: str) -> str:
    """Keep the credential on one explicit HTTP origin."""
    try:
        parsed = urlsplit(value)
        port = parsed.port
    except ValueError as err:
        raise InvalidBackendURL from err
    if (
        parsed.scheme not in {"http", "https"}
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.path not in {"", "/"}
        or parsed.query
        or parsed.fragment
        or port == 0
        or any(character.isspace() for character in value)
        or "\\" in value
    ):
        raise InvalidBackendURL
    return value.rstrip("/")


@dataclass(frozen=True, slots=True)
class BackendResponse:
    """A bounded upstream response, without credential-bearing headers."""

    status: int
    body: bytes
    headers: dict[str, str]

    def json(self) -> object:
        """Decode only when the caller expects JSON."""
        return json.loads(self.body)


class UtilitiesClient:
    """The shared session belongs to HA, not this integration."""

    def __init__(self, session: ClientSession, backend_url: str, token: str) -> None:
        self._session = session
        self._origin = normalize_backend_url(backend_url)
        if not token or len(token) > 8192 or any(char.isspace() for char in token):
            raise ValueError("Invalid credential")
        self._token = token

    async def request(
        self, method: str, path: str, *, body: bytes | None = None
    ) -> BackendResponse:
        """Never follow redirects or accept a per-request destination."""
        if not path.startswith("/") or path.startswith("//") or "\\" in path:
            raise ValueError("Invalid backend path")
        headers = {
            "Authorization": f"Bearer {self._token}",
            "Accept": "application/json",
        }
        if body:
            headers["Content-Type"] = "application/json"
        if method == "POST" and path == "/v1/sync":
            headers["X-Utilities-Request"] = "1"
        async with self._session.request(
            method,
            self._origin + path,
            data=body,
            headers=headers,
            allow_redirects=False,
            timeout=ClientTimeout(total=REQUEST_TIMEOUT_SECONDS),
        ) as response:
            payload = bytearray()
            async for chunk in response.content.iter_chunked(16 * 1024):
                payload.extend(chunk)
                if len(payload) > MAX_RESPONSE_BYTES:
                    raise ResponseTooLarge
            return BackendResponse(
                response.status,
                bytes(payload),
                {
                    "Content-Type": response.headers.get(
                        "Content-Type", "application/json"
                    )
                },
            )
