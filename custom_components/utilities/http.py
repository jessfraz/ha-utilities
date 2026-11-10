"""Authenticated and allowlisted HA bridge, not a generic HTTP proxy."""

import json
import re
from datetime import date
from urllib.parse import urlencode
from uuid import UUID

from aiohttp import ClientError, web
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.helpers.http import HomeAssistantView

from .authorization import authorized
from .client import ResponseTooLarge
from .const import DOMAIN, MAX_REQUEST_BYTES
from .coordinator import UtilitiesCoordinator
from .models import record, text

_ID = r"(?!\.{1,2}$)[A-Za-z0-9_.:-]{1,200}"
_RECORD_ID = r"(?!\.{1,2}$)[A-Za-z0-9_.:-]{1,500}"
_CONNECTION_ID = (
    r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
)
_ROUTES = (
    (re.compile("system|health|providers|accounts|summary"), {"GET"}, set()),
    (
        re.compile("bills|balances|collections|usage"),
        {"GET"},
        {"account", "service", "from", "to", "limit", "page"},
    ),
    (re.compile(rf"bills/{_RECORD_ID}|sync/{_ID}"), {"GET"}, set()),
    (re.compile("sync"), {"POST"}, set()),
    (re.compile("connections"), {"GET", "POST"}, set()),
    (re.compile("providers/bayou/utilities"), {"GET"}, set()),
    (re.compile(rf"connections/{_CONNECTION_ID}(?:/authorization)?"), {"GET"}, set()),
    (
        re.compile(rf"connections/{_CONNECTION_ID}/(?:refresh|resume|bind)"),
        {"POST"},
        set(),
    ),
)


def connection_input(path: str, value: object) -> None:
    """Allow owned connection commands, never credentials or arbitrary provider JSON."""
    row = record(value)
    if path == "connections":
        required = {
            "provider",
            "request_id",
            "utility_id",
            "currency",
            "accept_provider_settings",
        }
        if set(row) - required - {"name"} or required - set(row):
            raise ValueError("Invalid connection request")
        if row["provider"] != "bayou" or row["accept_provider_settings"] is not True:
            raise ValueError("Provider consent required")
        if UUID(text(row["request_id"])).int == 0:
            raise ValueError("Invalid connection request")
        text(row["utility_id"])
        text(row["currency"], max_length=3)
        if row.get("name") is not None:
            text(row["name"], max_length=100)
    elif path.endswith("/bind"):
        values = row.get("accounts")
        if (
            set(row) != {"accounts"}
            or not isinstance(values, list)
            or not 1 <= len(values) <= 100
        ):
            raise ValueError("Invalid account selection")
        for value in values:
            selection = record(value)
            meters = selection.get("meters")
            if (
                set(selection) != {"account", "meters"}
                or not isinstance(meters, list)
                or not 1 <= len(meters) <= 64
            ):
                raise ValueError("Invalid meter selection")
            text(selection["account"], max_length=300)
            for meter in meters:
                text(meter, max_length=300)
    elif row:
        raise ValueError("This action has no input fields")


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
        admin = (
            request.method != "GET"
            or path.startswith("connections")
            or path == "providers/bayou/utilities"
        )
        if not await authorized(self._hass, entry, coordinator, user_id, admin=admin):
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
                if path.startswith("connections"):
                    connection_input(path, values)
                elif (
                    not isinstance(values, dict)
                    or set(values) != {"account", "from", "to"}
                    or not isinstance(values["account"], str)
                    or re.fullmatch(_ID, values["account"]) is None
                ):
                    raise ValueError("Invalid sync request")
                if path == "sync":
                    start, end = (
                        date.fromisoformat(values["from"]),
                        date.fromisoformat(values["to"]),
                    )
                    if start > end or (end - start).days > 366:
                        raise ValueError("Invalid sync window")
            except ValueError, TypeError, UnicodeDecodeError:
                raise web.HTTPBadRequest(reason="Invalid request") from None
        elif payload:
            raise web.HTTPBadRequest(reason="GET requests have no body")
        target = "/health" if path == "health" else f"/v1/{path}"
        if request.query:
            target += "?" + urlencode(list(request.query.items()))
        # Streaming a request body can outlive the caller or the loaded entry.
        if not await authorized(self._hass, entry, coordinator, user_id, admin=admin):
            raise web.HTTPForbidden
        try:
            response = await coordinator.client.request(
                request.method, target, body=bytes(payload) if payload else None
            )
        except TimeoutError:
            raise web.HTTPGatewayTimeout from None
        except ClientError, ResponseTooLarge:
            raise web.HTTPBadGateway from None
        if not await authorized(self._hass, entry, coordinator, user_id, admin=admin):
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
