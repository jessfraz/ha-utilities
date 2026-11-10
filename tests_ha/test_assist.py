"""Real Assist API tool discovery, caller authorization, and late revocation."""

import asyncio
import json

import pytest
from homeassistant.components.homeassistant import exposed_entities
from homeassistant.core import Context, HomeAssistant
from homeassistant.helpers import llm
from homeassistant.setup import async_setup_component
from pytest_homeassistant.common import MockUser

from custom_components.utilities import UtilitiesConfigEntry

from .conftest import TOKEN, Backend, coverage, entity_id, json_response


async def assist_api(hass: HomeAssistant, user_id: str | None) -> llm.APIInstance:
    assert await async_setup_component(hass, "llm", {})
    return await llm.async_get_api(
        hass,
        "assist",
        llm.LLMContext(
            platform="test_conversation",
            context=Context(user_id=user_id),
            language="en",
            assistant="conversation",
            device_id=None,
        ),
    )


async def expose(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, enabled: bool = True
) -> str:
    identity = entity_id(hass, entry, "data_status")
    exposed_entities.async_expose_entity(hass, "conversation", identity, enabled)
    return identity


async def test_native_tools_are_discovered_only_when_explicitly_exposed(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, hass_user: MockUser
) -> None:
    await expose(hass, entry, False)
    api = await assist_api(hass, hass_user.id)
    assert not {tool.name for tool in api.tools if tool.integration == "utilities"}
    await expose(hass, entry)
    api = await assist_api(hass, hass_user.id)
    tools = [tool for tool in api.tools if tool.integration == "utilities"]
    assert {tool.name for tool in tools} == {
        "utilities__summary",
        "utilities__bills",
        "utilities__collections",
        "utilities__usage",
        "utilities__health",
    }
    assert all(
        tool.annotations.read_only
        and tool.annotations.idempotent
        and not tool.annotations.destructive
        and not tool.annotations.open_world
        for tool in tools
    )
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert not {
        tool.name
        for tool in (await assist_api(hass, hass_user.id)).tools
        if tool.integration == "utilities"
    }


async def test_tool_reads_bounded_data_and_preserves_coverage_freshness(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    hass_user: MockUser,
    backend: Backend,
) -> None:
    await expose(hass, entry)
    api = await assist_api(hass, hass_user.id)
    backend.stale = True
    backend.coverage = [coverage("bills", "unavailable", "access_denied")]
    backend.requests.clear()
    result = await api.async_call_tool(llm.ToolInput("utilities__bills", {"limit": 1}))
    assert not result.error
    assert result.data["stale"] is True
    assert result.data["coverage"] == backend.coverage
    assert result.data["next_page"] is None
    encoded = json.dumps(result.data)
    assert "private_extra" not in encoded
    assert "fictional-only-do-not-expose" not in encoded
    assert TOKEN not in encoded
    assert [(request.method, request.path) for request in backend.requests] == [
        ("GET", "/v1/bills"),
        ("GET", "/health"),
    ]
    assert backend.requests[0].query == {"limit": "1"}


@pytest.mark.parametrize("mutation", ["exposure", "active_user", "unload"])
async def test_tool_rechecks_authorization_after_every_await(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    hass_user: MockUser,
    backend: Backend,
    mutation: str,
) -> None:
    gate = await expose(hass, entry)
    api = await assist_api(hass, hass_user.id)
    started, release = asyncio.Event(), asyncio.Event()
    backend.gates["/v1/bills"] = (started, release)
    pending = asyncio.create_task(
        api.async_call_tool(llm.ToolInput("utilities__bills", {}))
    )
    await started.wait()
    if mutation == "exposure":
        exposed_entities.async_expose_entity(hass, "conversation", gate, False)
    elif mutation == "active_user":
        await hass.auth.async_update_user(hass_user, is_active=False)
    else:
        assert await hass.config_entries.async_unload(entry.entry_id)
    release.set()
    result = await pending
    assert result.error and result.data == {"error": "not_exposed"}
    assert "example-bill" not in json.dumps(result.data)


async def test_tool_missing_caller_cannot_read(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, backend: Backend
) -> None:
    await expose(hass, entry)
    api = await assist_api(hass, None)
    backend.requests.clear()
    result = await api.async_call_tool(llm.ToolInput("utilities__summary", {}))
    assert result.error
    assert backend.requests == []


async def test_tool_invalid_dates_and_backend_errors_are_not_empty_answers(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    hass_user: MockUser,
    backend: Backend,
) -> None:
    await expose(hass, entry)
    api = await assist_api(hass, hass_user.id)
    backend.requests.clear()
    result = await api.async_call_tool(
        llm.ToolInput("utilities__usage", {"from": "2026-02-30"})
    )
    assert result.error and result.data == {"error": "invalid_arguments"}
    assert backend.requests == []
    backend.responses["/v1/usage"] = (503, b"fictional-private-error", {})
    result = await api.async_call_tool(llm.ToolInput("utilities__usage", {}))
    assert result.error and result.data == {"error": "unavailable"}
    backend.responses["/v1/usage"] = json_response({"items": [], "next_page": None})
    result = await api.async_call_tool(llm.ToolInput("utilities__usage", {}))
    assert result.error


async def test_tool_checks_native_entity_read_permission(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    hass_user: MockUser,
    backend: Backend,
) -> None:
    from homeassistant.auth.models import Group

    await expose(hass, entry)
    api = await assist_api(hass, hass_user.id)
    hass_user.groups = [Group(name="Example denied group", policy={"entities": {}})]
    backend.requests.clear()
    result = await api.async_call_tool(llm.ToolInput("utilities__summary", {}))
    assert result.error and result.data == {"error": "not_exposed"}
    assert backend.requests == []


async def test_tool_rechecks_exposure_after_freshness_read(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    hass_user: MockUser,
    backend: Backend,
) -> None:
    gate = await expose(hass, entry)
    api = await assist_api(hass, hass_user.id)
    started, release = asyncio.Event(), asyncio.Event()
    backend.gates["/health"] = (started, release)
    pending = asyncio.create_task(
        api.async_call_tool(llm.ToolInput("utilities__bills", {}))
    )
    await started.wait()
    exposed_entities.async_expose_entity(hass, "conversation", gate, False)
    release.set()
    result = await pending
    assert result.error and result.data == {"error": "not_exposed"}
