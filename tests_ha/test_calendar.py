"""Native HA calendar reads distinguish unsupported schedules from missing data."""

from datetime import timedelta

import pytest
from homeassistant.core import HomeAssistant
from homeassistant.util import dt as dt_util
from pytest_homeassistant.typing import ClientSessionGenerator

from custom_components.utilities import UtilitiesConfigEntry

from .conftest import Backend, coverage, entity_id


async def test_calendar_with_only_unsupported_sources_returns_an_honest_empty_window(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    backend.collections = []
    unsupported = coverage(state="unsupported", reason="provider_not_supported")
    unsupported["from"] = None
    unsupported["to"] = None
    backend.coverage = [
        unsupported,
        coverage("bills", state="partial", reason="document_import"),
    ]
    await entry.runtime_data.async_refresh()
    calendar = entity_id(hass, entry, "collections_calendar", "calendar")
    client = await hass_client()
    response = await client.get(
        f"/api/calendars/{calendar}",
        params={
            "start": "2026-11-17T00:00:00-08:00",
            "end": "2026-11-18T00:00:00-08:00",
        },
    )
    assert response.status == 200, await response.text()
    assert await response.json() == []
    state = hass.states.get(calendar)
    assert state is not None and state.state == "off"
    assert state.attributes["coverage_status"] == "unsupported"
    assert state.attributes["coverage_complete"] is False
    assert backend.requests[-1].query == {
        "from": "2026-11-17",
        "to": "2026-11-17",
        "limit": "100",
    }


@pytest.mark.parametrize("state", ["partial", "unavailable", "not_configured", None])
async def test_calendar_does_not_hide_missing_collection_access_as_unsupported(
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    state: str | None,
) -> None:
    backend.collections = []
    backend.coverage = [coverage(state="unsupported")]
    if state is not None:
        backend.coverage.append(coverage(state=state))
    else:
        backend.coverage = [coverage("bills", state="complete")]
    await entry.runtime_data.async_refresh()
    calendar = entity_id(hass, entry, "collections_calendar", "calendar")
    client = await hass_client()
    response = await client.get(
        f"/api/calendars/{calendar}",
        params={
            "start": "2026-11-17T00:00:00-08:00",
            "end": "2026-11-18T00:00:00-08:00",
        },
    )
    assert response.status == 500
    assert "Collection coverage is incomplete" in await response.text()


@pytest.mark.parametrize("status", [401, 503])
async def test_calendar_keeps_transport_and_authentication_failures_visible(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    status: int,
) -> None:
    backend.collections = []
    backend.coverage = [coverage(state="unsupported")]
    await entry.runtime_data.async_refresh()
    backend.responses["/v1/collections"] = (status, b"{}", {})
    client = await hass_client()
    calendar = entity_id(hass, entry, "collections_calendar", "calendar")
    now = dt_util.now()
    response = await client.get(
        f"/api/calendars/{calendar}",
        params={"start": now.isoformat(), "end": (now + timedelta(days=1)).isoformat()},
    )
    assert response.status == 500


async def test_calendar_keeps_stale_unsupported_data_unavailable(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
) -> None:
    backend.collections = []
    backend.coverage = [coverage(state="unsupported")]
    backend.stale = True
    await entry.runtime_data.async_refresh()
    client = await hass_client()
    calendar = entity_id(hass, entry, "collections_calendar", "calendar")
    now = dt_util.now()
    response = await client.get(
        f"/api/calendars/{calendar}",
        params={"start": now.isoformat(), "end": (now + timedelta(days=1)).isoformat()},
    )
    assert response.status == 500
    assert "Collection data is unavailable or stale" in await response.text()
