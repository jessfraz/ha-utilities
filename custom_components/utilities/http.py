"""Authenticated and allowlisted HA bridge, not a generic HTTP proxy."""

import json
import re
from datetime import date
from urllib.parse import urlencode

from aiohttp import ClientError, web
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.helpers.http import HomeAssistantView

from .authorization import authorized
from .client import ResponseTooLarge
from .const import DOMAIN, MAX_REQUEST_BYTES
from .coordinator import UtilitiesCoordinator

_ID = r"(?!\.{1,2}$)[A-Za-z0-9_.:-]{1,200}"
_RECORD_ID = r"(?!\.{1,2}$)[A-Za-z0-9_.:-]{1,500}"
_ROUTES = (
    (re.compile("system|health|providers|accounts|summary"), {"GET"}, set()),
    (
        re.compile("bills|balances|collections|usage"),
        {"GET"},
        {"account", "service", "from", "to", "limit", "page"},
    ),
    (re.compile(rf"bills/{_RECORD_ID}|sync/{_ID}"), {"GET"}, set()),
    (re.compile("sync"), {"POST"}, set()),
)


def loaded_entry(hass: HomeAssistant) -> ConfigEntry[UtilitiesCoordinator] | None:
    for entry in hass.config_entries.async_entries(DOMAIN):
        if entry.state is ConfigEntryState.LOADED and entry.runtime_data.active:
            return entry
    return None


class UtilitiesView(HomeAssistantView):
    url = "/api/utilities/v1/{path:.*}"
    name = "api:utilities:v1"
    requires_auth = True

    def __init__(self, hass: HomeAssistant) -> None:
        self._hass = hass

    async def _proxy(self, request: web.Request, path: str) -> web.Response:
        route = next((row for row in _ROUTES if row[0].fullmatch(path)), None)
        if route is None:
            raise web.HTTPNotFound
        if request.method not in route[1]:
            raise web.HTTPMethodNotAllowed(request.method, route[1])
        if (
            len(request.query_string) > 7000
            or request.query.keys() - route[2]
            or len(request.query) != len(set(request.query))
        ):
            raise web.HTTPBadRequest(reason="Unsupported query")
        entry = loaded_entry(self._hass)
        if entry is None:
            raise web.HTTPServiceUnavailable
        coordinator = entry.runtime_data
        user_id = request["hass_user"].id
        if not await authorized(
            self._hass, entry, coordinator, user_id, admin=request.method != "GET"
        ):
            raise web.HTTPForbidden
        payload = bytearray()
        async for chunk in request.content.iter_chunked(1024):
            payload.extend(chunk)
            if len(payload) > MAX_REQUEST_BYTES:
                raise web.HTTPRequestEntityTooLarge(
                    max_size=MAX_REQUEST_BYTES, actual_size=len(payload)
                )
        if request.method == "POST":
            try:
                values = json.loads(payload)
                if (
                    not isinstance(values, dict)
                    or set(values) != {"account", "from", "to"}
                    or not isinstance(values["account"], str)
                    or re.fullmatch(_ID, values["account"]) is None
                ):
                    raise ValueError("Invalid sync request")
                start, end = (
                    date.fromisoformat(values["from"]),
                    date.fromisoformat(values["to"]),
                )
                if start > end or (end - start).days > 366:
                    raise ValueError("Invalid sync window")
            except ValueError, TypeError, UnicodeDecodeError:
                raise web.HTTPBadRequest(reason="Invalid sync request") from None
        elif payload:
            raise web.HTTPBadRequest(reason="GET requests have no body")
        target = "/health" if path == "health" else f"/v1/{path}"
        if request.query:
            target += "?" + urlencode(list(request.query.items()))
        # Streaming a request body can outlive the caller or the loaded entry.
        if not await authorized(
            self._hass, entry, coordinator, user_id, admin=request.method != "GET"
        ):
            raise web.HTTPForbidden
        try:
            response = await coordinator.client.request(
                request.method, target, body=bytes(payload) if payload else None
            )
        except TimeoutError:
            raise web.HTTPGatewayTimeout from None
        except ClientError, ResponseTooLarge:
            raise web.HTTPBadGateway from None
        if not await authorized(
            self._hass, entry, coordinator, user_id, admin=request.method != "GET"
        ):
            raise web.HTTPForbidden
        if 300 <= response.status < 400:
            raise web.HTTPBadGateway
        return web.Response(
            status=response.status,
            body=response.body,
            headers={**response.headers, "Cache-Control": "no-store"},
        )

    get = _proxy
    post = _proxy
