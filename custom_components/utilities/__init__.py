"""Home Assistant entities and authenticated access to the Utilities read model."""

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant
from homeassistant.helpers import config_validation as cv
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.typing import ConfigType
from homeassistant.loader import async_get_integration

from .client import UtilitiesClient
from .const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from .coordinator import UtilitiesCoordinator
from .http import UtilitiesView

CONFIG_SCHEMA = cv.config_entry_only_config_schema(DOMAIN)
PLATFORMS = [Platform.SENSOR, Platform.CALENDAR]
type UtilitiesConfigEntry = ConfigEntry[UtilitiesCoordinator]


async def async_setup(hass: HomeAssistant, config: ConfigType) -> bool:
    """Register once; each request resolves the current loaded entry."""
    hass.http.register_view(UtilitiesView(hass))
    return True


async def async_setup_entry(hass: HomeAssistant, entry: UtilitiesConfigEntry) -> bool:
    """Validate the service before publishing counts or a calendar."""
    client = UtilitiesClient(
        async_get_clientsession(hass),
        entry.data[CONF_BACKEND_URL],
        entry.data[CONF_TOKEN],
    )
    integration = await async_get_integration(hass, DOMAIN)
    coordinator = UtilitiesCoordinator(hass, entry, client, str(integration.version))
    await coordinator.async_config_entry_first_refresh()
    entry.runtime_data = coordinator
    entry.async_on_unload(coordinator.stop)
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


async def async_unload_entry(hass: HomeAssistant, entry: UtilitiesConfigEntry) -> bool:
    """Invalidate late reads before the owned entities are removed."""
    coordinator = entry.runtime_data
    coordinator.active = False
    unloaded = False
    try:
        unloaded = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    finally:
        if unloaded:
            coordinator.stop()
        else:
            coordinator.active = True
    return unloaded
