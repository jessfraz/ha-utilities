"""The actual HA adapter, entities and Assist read the seeded Rust/SQLite service."""

import asyncio
import json
import os
from pathlib import Path

import pytest
from homeassistant.components.homeassistant import exposed_entities
from homeassistant.config_entries import SOURCE_USER, ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.helpers import llm
from pytest_homeassistant.common import MockUser
from pytest_homeassistant.typing import ClientSessionGenerator

from custom_components.utilities import UtilitiesConfigEntry
from custom_components.utilities.const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from custom_components.utilities.models import page, record
from tests_web.conftest import WebBackend
from tests_web.conftest import web_backend as web_backend

from .conftest import entity_id
from .test_assist import assist_api

_ROOT = Path(__file__).resolve().parents[1]


async def connect_backend(
    hass: HomeAssistant, backend: WebBackend
) -> UtilitiesConfigEntry:
    """Keep all consumers on the actual configured entry and native transport."""
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_BACKEND_URL: backend.origin, CONF_TOKEN: backend.token},
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY
    entry = result["result"]
    await hass.async_block_till_done(wait_background_tasks=True)
    assert entry.state is ConfigEntryState.LOADED
    return entry


async def test_native_store_read_model_is_shared_by_ha_and_assist(
    hass: HomeAssistant,
    web_backend: WebBackend,
    hass_client: ClientSessionGenerator,
    hass_user: MockUser,
) -> None:
    entry = await connect_backend(hass, web_backend)
    bills = hass.states.get(entity_id(hass, entry, "bills"))
    assert bills is not None and bills.state == "70"
    status = hass.states.get(entity_id(hass, entry, "data_status"))
    assert status is not None and status.state == "incomplete"
    client = await hass_client()
    bridge = await client.get("/api/utilities/v1/bills?limit=1")
    assert bridge.status == 200
    page = await bridge.json()
    assert len(page["items"]) == 1
    assert page["next_page"] is not None
    exposed_entities.async_expose_entity(
        hass, "conversation", entity_id(hass, entry, "data_status"), True
    )
    api = await assist_api(hass, hass_user.id)
    answer = await api.async_call_tool(llm.ToolInput("utilities__bills", {"limit": 1}))
    assert not answer.error
    assert answer.data["items"] == page["items"]
    assert answer.data["coverage"] == page["coverage"]
    assert answer.data["next_page"] == page["next_page"]
    assert answer.data["stale"] is False
    accepted = await client.post(
        "/api/utilities/v1/sync",
        json={"account": "demo-wm", "from": "2026-01-01", "to": "2026-01-02"},
    )
    assert accepted.status == 202


@pytest.mark.parametrize("revocation", ["inactive", "revoked_token"])
async def test_actual_cli_ha_transport_reads_native_bridge_and_denies_revoked_access(
    hass: HomeAssistant,
    web_backend: WebBackend,
    hass_client: ClientSessionGenerator,
    hass_user: MockUser,
    hass_user_token: str,
    tmp_path: Path,
    revocation: str,
) -> None:
    await connect_backend(hass, web_backend)
    client = await hass_client(hass_user_token)
    ha_origin = str(client.make_url("/")).rstrip("/")
    token_file = tmp_path / "ha-credential"
    await asyncio.to_thread(token_file.write_text, hass_user_token)
    await asyncio.to_thread(token_file.chmod, 0o600)
    binary = Path(
        os.environ.get(
            "HA_UTILITIES_TEST_BINARY",
            _ROOT / "target/debug/ha-utilities",
        )
    )
    assert await asyncio.to_thread(binary.is_file), (
        "Build the ha-utilities CLI before the native HA transport test"
    )

    async def run_cli(cursor: str | None = None) -> tuple[int, bytes, bytes]:
        arguments = [
            str(binary),
            "--transport",
            "ha",
            "--url",
            ha_origin,
            "--token-file",
            str(token_file),
            "--json",
            "bills",
            "list",
            "--limit",
            "1",
        ]
        if cursor is not None:
            arguments.extend(["--page", cursor])
        process = await asyncio.create_subprocess_exec(
            *arguments, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
        )
        try:
            async with asyncio.timeout(10):
                stdout, stderr = await process.communicate()
        finally:
            if process.returncode is None:
                process.kill()
                await process.wait()
        assert process.returncode is not None
        return process.returncode, stdout, stderr

    bridge = await client.get("/api/utilities/v1/bills?limit=1")
    assert bridge.status == 200
    expected = record(await bridge.json())
    expected_items, expected_cursor, expected_coverage = page(expected, maximum=1)
    assert len(expected_items) == 1 and expected_cursor is not None
    result, stdout, stderr = await run_cli()
    assert result == 0, stderr.decode()
    assert stderr == b""
    items, cursor, coverage = page(json.loads(stdout), maximum=1)
    assert items == expected_items
    assert cursor == expected_cursor
    assert coverage == expected_coverage
    first_bill = record(items[0])
    amount = record(first_bill["total"])
    assert type(amount["minor_units"]) is int and amount["currency"] == "USD"

    result, stdout, stderr = await run_cli(cursor)
    assert result == 0, stderr.decode()
    following, following_cursor, following_coverage = page(
        json.loads(stdout), maximum=1
    )
    assert len(following) == 1
    assert record(following[0])["id"] != first_bill["id"]
    assert following_cursor is not None and following_cursor != cursor
    assert following_coverage == coverage

    if revocation == "inactive":
        await hass.auth.async_update_user(hass_user, is_active=False)
    else:
        refresh_token = hass.auth.async_validate_access_token(hass_user_token)
        assert refresh_token is not None
        hass.auth.async_remove_refresh_token(refresh_token)
        assert hass_user.is_active
    assert hass.auth.async_validate_access_token(hass_user_token) is None
    assert (await client.get("/api/utilities/v1/bills?limit=1")).status == 401
    result, stdout, stderr = await run_cli()
    assert result != 0 and stdout == b""
    error = record(json.loads(stderr))
    assert error == {
        "error": "authentication_required",
        "message": "authentication required",
        "status": 401,
    }
    assert hass_user_token.encode() not in stderr
    assert web_backend.token.encode() not in stderr
