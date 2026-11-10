"""Native Supervisor discovery validates one origin before saving credentials."""

from uuid import UUID

from homeassistant.config_entries import SOURCE_HASSIO, ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.helpers.service_info.hassio import HassioServiceInfo

from custom_components.utilities.const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN

from .conftest import TOKEN, Backend


async def test_verified_supervisor_discovery_requires_confirmation(
    hass: HomeAssistant, backend: Backend
) -> None:
    info = HassioServiceInfo(
        config={CONF_BACKEND_URL: backend.origin, CONF_TOKEN: TOKEN},
        name="Utilities",
        slug="example_utilities",
        uuid=UUID(int=17).hex,
    )
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_HASSIO}, data=info
    )
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "hassio_confirm"
    assert hass.config_entries.async_entries(DOMAIN) == []
    assert any(request.path == "/v1/system" for request in backend.requests)
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done(wait_background_tasks=True)
    entry = result["result"]
    assert entry.state is ConfigEntryState.LOADED
    assert entry.data[CONF_BACKEND_URL] == backend.origin
    assert entry.data["addon_slug"] == "example_utilities"


async def test_invalid_discovery_cannot_save_or_follow_redirects(
    hass: HomeAssistant, backend: Backend
) -> None:
    backend.responses["/v1/system"] = (
        302,
        b"",
        {"Location": "https://example.invalid"},
    )
    info = HassioServiceInfo(
        config={CONF_BACKEND_URL: backend.origin, CONF_TOKEN: TOKEN},
        name="Utilities",
        slug="example_utilities",
        uuid=UUID(int=18).hex,
    )
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_HASSIO}, data=info
    )
    assert result["type"] is FlowResultType.ABORT
    assert hass.config_entries.async_entries(DOMAIN) == []
    assert len(backend.requests) == 1
