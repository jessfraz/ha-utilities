"""Use the shared version repair for the loaded Utilities entry."""

from typing import TYPE_CHECKING

from homeassistant.components.repairs import RepairsFlow
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError

from ._app_kit.repairs import VersionRepairSpec, create_version_fix_flow
from ._app_kit.version import LOADED_VERSION
from ._app_kit.versions import ReleaseVersion
from .const import CONF_ADDON_SLUG, DOMAIN
from .models import record, valid_system

if TYPE_CHECKING:
    from .coordinator import UtilitiesCoordinator


async def _async_probe(_hass: HomeAssistant, entry: ConfigEntry) -> ReleaseVersion:
    coordinator: UtilitiesCoordinator = entry.runtime_data
    system = record(await coordinator.read("/v1/system"))
    if not valid_system(system):
        raise HomeAssistantError("Utilities version is unavailable")
    return ReleaseVersion.parse(system.get("version"))


def _app_slug(entry: ConfigEntry) -> str | None:
    slug = entry.data.get(CONF_ADDON_SLUG)
    if slug is not None and not isinstance(slug, str):
        raise HomeAssistantError("Utilities app identity is invalid")
    return slug


VERSION_REPAIR = VersionRepairSpec(
    domain=DOMAIN,
    name="Utilities",
    repository_id="1452510907",
    loaded_version=ReleaseVersion.parse(LOADED_VERSION),
    probe=_async_probe,
    app_slug=_app_slug,
)


async def async_create_fix_flow(
    hass: HomeAssistant, issue_id: str, _data: dict[str, object] | None
) -> RepairsFlow:
    """Resolve the entry from the native issue registry."""
    return create_version_fix_flow(hass, issue_id, VERSION_REPAIR)
