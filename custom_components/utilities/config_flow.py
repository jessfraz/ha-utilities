"""Connect one explicit local read model; never link utility accounts."""

from collections.abc import Mapping
from uuid import UUID

import probatio as vol
from aiohttp import ClientError
from homeassistant.config_entries import SOURCE_REAUTH, ConfigFlow, ConfigFlowResult
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.selector import (
    TextSelector,
    TextSelectorConfig,
    TextSelectorType,
)
from homeassistant.helpers.service_info.hassio import HassioServiceInfo

from .client import (
    InvalidBackendURL,
    ResponseTooLarge,
    UtilitiesClient,
    normalize_backend_url,
)
from .const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from .models import record, valid_system


class UtilitiesConfigFlow(ConfigFlow, domain=DOMAIN):
    VERSION = 1

    def __init__(self) -> None:
        self._discovered: dict[str, str] | None = None

    async def async_step_hassio(
        self, discovery_info: HassioServiceInfo
    ) -> ConfigFlowResult:
        """Validate Supervisor discovery without enrolling any provider account."""
        if not isinstance(discovery_info, HassioServiceInfo):
            return self.async_abort(reason="invalid_discovery")
        values = discovery_info.config
        if not isinstance(values, dict):
            return self.async_abort(reason="invalid_discovery")
        origin, token = values.get(CONF_BACKEND_URL), values.get(CONF_TOKEN)
        if (
            not isinstance(origin, str)
            or not isinstance(token, str)
            or not isinstance(discovery_info.slug, str)
            or not discovery_info.slug
        ):
            return self.async_abort(reason="invalid_discovery")
        try:
            origin = normalize_backend_url(origin)
            unique_id = UUID(discovery_info.uuid).hex
            client = UtilitiesClient(async_get_clientsession(self.hass), origin, token)
            response = await client.request("GET", "/v1/system")
            system = record(response.json())
            if response.status != 200 or not valid_system(system):
                return self.async_abort(reason="cannot_connect")
        except ClientError, TimeoutError, ValueError, TypeError, AttributeError:
            return self.async_abort(reason="invalid_discovery")
        await self.async_set_unique_id(unique_id)
        if self.source == SOURCE_REAUTH:
            self._abort_if_unique_id_mismatch()
        else:
            self._abort_if_unique_id_configured()
            if self._async_current_entries():
                return self.async_abort(reason="single_instance_allowed")
        self._discovered = {
            CONF_BACKEND_URL: origin,
            CONF_TOKEN: token,
            "addon_slug": discovery_info.slug,
        }
        self.context["title_placeholders"] = {"name": "Utilities"}
        return await self.async_step_hassio_confirm()

    async def async_step_hassio_confirm(
        self, user_input: dict[str, object] | None = None
    ) -> ConfigFlowResult:
        """Persist a verified discovered service only after native confirmation."""
        if self._discovered is None:
            return self.async_abort(reason="invalid_discovery")
        if user_input is not None:
            if self.source == SOURCE_REAUTH:
                return self.async_update_reload_and_abort(
                    self._get_reauth_entry(), data_updates=self._discovered
                )
            if self._async_current_entries():
                return self.async_abort(reason="single_instance_allowed")
            return self.async_create_entry(title="Utilities", data=self._discovered)
        return self.async_show_form(
            step_id="hassio_confirm", data_schema=vol.Schema({})
        )

    async def async_step_user(
        self, user_input: dict[str, str] | None = None
    ) -> ConfigFlowResult:
        if self._async_current_entries():
            return self.async_abort(reason="single_instance_allowed")
        return await self._configure(user_input)

    async def async_step_reauth(
        self, entry_data: Mapping[str, object]
    ) -> ConfigFlowResult:
        return await self.async_step_reauth_confirm()

    async def async_step_reauth_confirm(
        self, user_input: dict[str, str] | None = None
    ) -> ConfigFlowResult:
        return await self._configure(user_input, reauth=True)

    async def _configure(
        self, user_input: dict[str, str] | None, *, reauth: bool = False
    ) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            try:
                origin = normalize_backend_url(user_input[CONF_BACKEND_URL])
                client = UtilitiesClient(
                    async_get_clientsession(self.hass), origin, user_input[CONF_TOKEN]
                )
                response = await client.request("GET", "/v1/system")
                if response.status in {401, 403}:
                    errors["base"] = "invalid_auth"
                elif response.status != 200 or not valid_system(response.json()):
                    errors["base"] = "cannot_connect"
                else:
                    values = {
                        CONF_BACKEND_URL: origin,
                        CONF_TOKEN: user_input[CONF_TOKEN],
                    }
                    if reauth:
                        return self.async_update_reload_and_abort(
                            self._get_reauth_entry(), data_updates=values
                        )
                    return self.async_create_entry(title="Utilities", data=values)
            except InvalidBackendURL:
                errors[CONF_BACKEND_URL] = "invalid_url"
            except ClientError, TimeoutError, ResponseTooLarge, ValueError:
                errors["base"] = "cannot_connect"
        default_url = self._get_reauth_entry().data[CONF_BACKEND_URL] if reauth else ""
        return self.async_show_form(
            step_id="reauth_confirm" if reauth else "user",
            data_schema=vol.Schema(
                {
                    vol.Required(CONF_BACKEND_URL, default=default_url): TextSelector(),
                    vol.Required(CONF_TOKEN): TextSelector(
                        TextSelectorConfig(type=TextSelectorType.PASSWORD)
                    ),
                }
            ),
            errors=errors,
        )
