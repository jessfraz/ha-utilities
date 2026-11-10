"""One explicit read-model entity gates private utility access."""

from homeassistant.auth.permissions.const import POLICY_READ
from homeassistant.components.homeassistant.exposed_entities import async_should_expose
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.helpers import entity_registry as er

from .const import DOMAIN
from .coordinator import UtilitiesCoordinator


def gate_entity(hass: HomeAssistant, entry: ConfigEntry) -> str | None:
    return er.async_get(hass).async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_data_status"
    )


async def authorized(
    hass: HomeAssistant,
    entry: ConfigEntry[UtilitiesCoordinator],
    coordinator: UtilitiesCoordinator,
    user_id: str | None,
    *,
    assistant: str | None = None,
    admin: bool = False,
) -> bool:
    """Re-resolve the user, exposure, and owned entry after each awaited read."""
    user = await hass.auth.async_get_user(user_id) if user_id else None
    entity_id = gate_entity(hass, entry)
    return bool(
        user is not None
        and user.is_active
        and (not admin or user.is_admin)
        and entity_id is not None
        and user.permissions.check_entity(entity_id, POLICY_READ)
        and hass.states.get(entity_id) is not None
        and coordinator.active
        and entry.state is ConfigEntryState.LOADED
        and entry.runtime_data is coordinator
        and (assistant is None or async_should_expose(hass, assistant, entity_id))
    )
