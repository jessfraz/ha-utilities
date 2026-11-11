"""Use the shared version repair for the loaded Utilities entry."""

from homeassistant.components.repairs import RepairsFlow
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from ._app_kit.repairs import VersionRepairSpec, create_version_fix_flow
from ._app_kit.version import LOADED_VERSION
from ._app_kit.versions import ReleaseVersion
from .client import UtilitiesClient
from .const import CONF_ADDON_SLUG, CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from .models import record, valid_system


async def _async_probe(hass: HomeAssistant, entry: ConfigEntry) -> ReleaseVersion:
    client = UtilitiesClient(
        async_get_clientsession(hass),
        entry.data[CONF_BACKEND_URL],
        entry.data[CONF_TOKEN],
    )
    response = await client.request("GET", "/v1/system")
    if response.status != 200:
        raise HomeAssistantError("Utilities version is unavailable")
    system = record(response.json())
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
