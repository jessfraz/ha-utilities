"""Read-model counts and freshness, without invented monetary totals."""

from homeassistant.components.sensor import (
    SensorDeviceClass,
    SensorEntity,
    SensorEntityDescription,
)
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import UtilitiesConfigEntry
from .const import DOMAIN
from .coordinator import UtilitiesCoordinator

DESCRIPTIONS = (
    SensorEntityDescription(
        key="accounts", translation_key="accounts", icon="mdi:account-group-outline"
    ),
    SensorEntityDescription(
        key="bills", translation_key="bills", icon="mdi:receipt-text-outline"
    ),
    SensorEntityDescription(
        key="collections", translation_key="collections", icon="mdi:trash-can-outline"
    ),
    SensorEntityDescription(
        key="usage_intervals",
        translation_key="usage_intervals",
        icon="mdi:chart-timeline-variant",
    ),
    SensorEntityDescription(
        key="last_synced_at",
        translation_key="last_synced_at",
        device_class=SensorDeviceClass.TIMESTAMP,
    ),
    SensorEntityDescription(
        key="data_status",
        translation_key="data_status",
        device_class=SensorDeviceClass.ENUM,
        options=["current", "stale", "incomplete"],
    ),
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    async_add_entities(
        UtilitiesSensor(entry.runtime_data, entry.entry_id, description)
        for description in DESCRIPTIONS
    )


class UtilitiesSensor(CoordinatorEntity[UtilitiesCoordinator], SensorEntity):
    """No account numbers, amounts, provider messages or usage in attributes."""

    _attr_has_entity_name = True

    def __init__(
        self,
        coordinator: UtilitiesCoordinator,
        entry_id: str,
        description: SensorEntityDescription,
    ) -> None:
        super().__init__(coordinator)
        self.entity_description = description
        self._attr_unique_id = f"{entry_id}_{description.key}"
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, entry_id)}, name="Utilities", manufacturer="Utilities"
        )
        self._set_value()

    def _set_value(self) -> None:
        data = self.coordinator.data.summary
        key = self.entity_description.key
        if key == "data_status":
            self._attr_native_value = (
                "stale"
                if data.stale
                else "incomplete"
                if not data.coverage
                or any(
                    row.state not in {"complete", "unsupported"}
                    for row in data.coverage
                )
                else "current"
            )
        else:
            self._attr_native_value = getattr(data, key)

    @callback
    def _handle_coordinator_update(self) -> None:
        self._set_value()
        super()._handle_coordinator_update()
