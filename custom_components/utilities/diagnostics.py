"""Cached operational metadata only, never utility records or credentials."""

from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.helpers import issue_registry as ir

from . import UtilitiesConfigEntry
from .const import DOMAIN
from .coordinator import REPAIRS


async def async_get_config_entry_diagnostics(
    hass: HomeAssistant, entry: UtilitiesConfigEntry
) -> dict[str, object]:
    coordinator = getattr(entry, "runtime_data", None)
    registry = ir.async_get(hass)
    return {
        "entry_state": entry.state.value,
        "available": bool(
            entry.state is ConfigEntryState.LOADED
            and coordinator
            and coordinator.active
            and coordinator.last_update_success
        ),
        "backend_version": coordinator.backend_version if coordinator else None,
        "last_error_type": type(coordinator.last_exception).__name__
        if coordinator and coordinator.last_exception
        else None,
        "stale": coordinator.data.summary.stale
        if coordinator and coordinator.data
        else None,
        "coverage_states": sorted(
            {row.state for row in coordinator.data.summary.coverage}
        )
        if coordinator and coordinator.data
        else [],
        "repairs": {
            key: registry.async_get_issue(DOMAIN, f"{entry.entry_id}_{key}") is not None
            for key in REPAIRS
        },
    }
