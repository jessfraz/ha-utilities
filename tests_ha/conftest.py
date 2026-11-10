"""Fictional HTTP data exercised through real Home Assistant entry lifecycle."""

import asyncio
import json
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta

import pytest
from aiohttp import web
from homeassistant.auth.const import GROUP_ID_USER
from homeassistant.auth.providers import AuthProvider
from homeassistant.config_entries import SOURCE_USER
from homeassistant.core import HomeAssistant
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.helpers import entity_registry as er
from homeassistant.setup import async_setup_component
from pytest_aiohttp.plugin import AiohttpServer
from pytest_homeassistant.common import MockUser

from custom_components.utilities import UtilitiesConfigEntry
from custom_components.utilities.const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN

TOKEN = "fictional-service-credential"


def coverage(
    capability: str = "collections", state: str = "complete", reason: str | None = None
) -> dict[str, object]:
    """Use only fictional local IDs and a current bounded date window."""
    now = datetime.now(UTC)
    return {
        "account_id": "example-account",
        "capability": capability,
        "state": state,
        "reason": reason,
        "checked_at": now.isoformat(),
        "from": (now - timedelta(days=365)).isoformat(),
        "to": (now + timedelta(days=365)).isoformat(),
    }


def collection(identity: str = "example-collection") -> dict[str, object]:
    now = datetime.now(UTC)
    return {
        "id": identity,
        "account_id": "example-account",
        "service_id": "example-waste",
        "scheduled_date": (now.date() + timedelta(days=1)).isoformat(),
        "status": "scheduled",
        "completed_at": None,
        "eta": None,
        "exception": None,
        "observed_at": now.isoformat(),
    }


@dataclass(frozen=True, slots=True)
class Received:
    method: str
    path: str
    query: dict[str, str]
    body: bytes
    authorization: str | None


@dataclass(slots=True)
class Backend:
    """Real HTTP fixture server; no adapter transport is mocked."""

    origin: str = ""
    version: str = "0.1.0"
    stale: bool = False
    requests: list[Received] = field(default_factory=list)
    coverage: list[dict[str, object]] = field(default_factory=lambda: [coverage()])
    collections: list[dict[str, object]] = field(default_factory=lambda: [collection()])
    usage: list[dict[str, object]] = field(default_factory=list)
    responses: dict[str, tuple[int, bytes, dict[str, str]]] = field(
        default_factory=dict
    )
    gates: dict[str, tuple[asyncio.Event, asyncio.Event]] = field(default_factory=dict)

    def summary(self) -> dict[str, object]:
        return {
            "accounts": 1,
            "bills": 2,
            "collections": len(self.collections),
            "usage_intervals": 3,
            "last_synced_at": datetime.now(UTC).isoformat(),
            "stale": self.stale,
            "coverage": self.coverage,
        }

    def page(self, rows: list[dict[str, object]]) -> dict[str, object]:
        return {"items": rows, "next_page": None, "coverage": self.coverage}

    async def handle(self, request: web.Request) -> web.Response:
        self.requests.append(
            Received(
                request.method,
                request.path,
                dict(request.query),
                await request.read(),
                request.headers.get("Authorization"),
            )
        )
        if gate := self.gates.get(request.path):
            gate[0].set()
            await gate[1].wait()
        if response := self.responses.get(
            request.path_qs, self.responses.get(request.path)
        ):
            status, body, headers = response
            return web.Response(status=status, body=body, headers=headers)
        if request.headers.get("Authorization") != f"Bearer {TOKEN}":
            return web.json_response({"error": "unauthorized"}, status=401)
        match request.path:
            case "/v1/system":
                return web.json_response(
                    {
                        "name": "Utilities",
                        "version": self.version,
                        "read_only_providers": True,
                    }
                )
            case "/v1/summary":
                return web.json_response(self.summary())
            case "/health":
                return web.json_response(
                    {
                        "status": "ok",
                        "version": self.version,
                        "last_synced_at": datetime.now(UTC).isoformat(),
                        "stale": self.stale,
                        "coverage": self.coverage,
                    }
                )
            case "/v1/collections":
                return web.json_response(self.page(self.collections))
            case "/v1/bills":
                return web.json_response(
                    self.page(
                        [
                            {
                                "id": "example-bill",
                                "account_id": "example-account",
                                "total": {"minor_units": 1200, "currency": "USD"},
                                "source_status": None,
                                "private_extra": "fictional-only-do-not-expose",
                            }
                        ]
                    )
                )
            case "/v1/usage":
                rows = [
                    row
                    for row in self.usage
                    if request.query.get("service", row["service_id"])
                    == row["service_id"]
                    and str(row["start"])[:10] >= request.query.get("from", "")
                    and str(row["start"])[:10] <= request.query.get("to", "9999-12-31")
                ]
                offset = int(request.query.get("page", "0"))
                limit = int(request.query.get("limit", "100"))
                return web.json_response(
                    {
                        "items": rows[offset : offset + limit],
                        "next_page": str(offset + limit)
                        if len(rows) > offset + limit
                        else None,
                        "coverage": self.coverage,
                    }
                )
            case "/v1/accounts":
                return web.json_response(
                    self.page(
                        [
                            {
                                "id": "example-account",
                                "name": "Example account",
                                "provider": "bayou",
                                "services": [
                                    {
                                        "id": "example-electric",
                                        "name": "Example electricity",
                                        "kind": "electricity",
                                    }
                                ],
                            }
                        ]
                    )
                )
            case "/v1/providers" | "/v1/balances":
                return web.json_response(self.page([]))
            case "/v1/sync":
                return web.json_response(
                    {"id": "example-job", "state": "running"}, status=202
                )
        return web.json_response({"error": "not_found"}, status=404)


@pytest.fixture(autouse=True)
def custom_integrations(enable_custom_integrations: None) -> None:
    """Load the local integration rather than requiring installation into Core."""


@pytest.fixture
def hass_config_dir(hass_tmp_config_dir: str) -> str:
    return hass_tmp_config_dir


@pytest.fixture(autouse=True)
async def native_exposure(hass: HomeAssistant) -> None:
    assert await async_setup_component(hass, "homeassistant", {})


@pytest.fixture
async def backend(
    hass: HomeAssistant, aiohttp_server: AiohttpServer, socket_enabled: None
) -> AsyncIterator[Backend]:
    upstream = Backend()
    app = web.Application()
    app.router.add_route("*", "/{path:.*}", upstream.handle)
    server = await aiohttp_server(app)
    upstream.origin = str(server.make_url("")).rstrip("/")
    yield upstream


async def connect(hass: HomeAssistant, backend: Backend) -> UtilitiesConfigEntry:
    """Use the actual config flow and platform forwarding."""
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {CONF_BACKEND_URL: backend.origin, CONF_TOKEN: TOKEN}
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY, result
    entry = result["result"]
    await hass.async_block_till_done(wait_background_tasks=True)
    return entry


@pytest.fixture
async def entry(hass: HomeAssistant, backend: Backend) -> UtilitiesConfigEntry:
    return await connect(hass, backend)


def entity_id(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, key: str, platform: str = "sensor"
) -> str:
    identity = er.async_get(hass).async_get_entity_id(
        platform, DOMAIN, f"{entry.entry_id}_{key}"
    )
    assert identity is not None
    return identity


@pytest.fixture
async def hass_user(hass: HomeAssistant, local_auth: AuthProvider) -> MockUser:
    group = await hass.auth.async_get_group(GROUP_ID_USER)
    assert group is not None
    return MockUser(groups=[group]).add_to_hass(hass)


@pytest.fixture
async def hass_user_token(hass: HomeAssistant, hass_user: MockUser) -> str:
    refresh = await hass.auth.async_create_refresh_token(
        hass_user, client_id="https://example.invalid"
    )
    return hass.auth.async_create_access_token(refresh)


def json_response(
    value: object, status: int = 200
) -> tuple[int, bytes, dict[str, str]]:
    return status, json.dumps(value).encode(), {"Content-Type": "application/json"}
