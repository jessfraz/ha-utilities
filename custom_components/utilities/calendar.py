"""Read-only collection dates; no booking or service-change controls."""

from datetime import datetime, timedelta

from aiohttp import ClientError
from homeassistant.components.calendar import CalendarEntity, CalendarEvent
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity, UpdateFailed
from homeassistant.util import dt as dt_util

from . import UtilitiesConfigEntry
from .client import ResponseTooLarge
from .const import DOMAIN
from .coordinator import UtilitiesCoordinator
from .models import CollectionCoverage


async def async_setup_entry(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    async_add_entities([CollectionCalendar(entry.runtime_data, entry.entry_id)])


class CollectionCalendar(CoordinatorEntity[UtilitiesCoordinator], CalendarEntity):
    """Unavailable coverage is not an empty collection schedule."""

    _attr_has_entity_name = True
    _attr_translation_key = "collections"
    _attr_supported_features = 0

    def __init__(self, coordinator: UtilitiesCoordinator, entry_id: str) -> None:
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry_id}_collections_calendar"
        self._attr_device_info = DeviceInfo(identifiers={(DOMAIN, entry_id)})
        self._set_coverage()

    @property
    def available(self) -> bool:
        return (
            super().available
            and not self.coordinator.data.summary.stale
            and (
                self.coordinator.data.collections_coverage
                is not CollectionCoverage.INCOMPLETE
                or bool(self.coordinator.data.collections)
            )
        )

    def _set_coverage(self) -> None:
        self._attr_extra_state_attributes = {
            "coverage_complete": self.coordinator.data.collections_complete,
            "coverage_status": self.coordinator.data.collections_coverage.value,
        }

    @callback
    def _handle_coordinator_update(self) -> None:
        self._set_coverage()
        super()._handle_coordinator_update()

    @property
    def event(self) -> CalendarEvent | None:
        now = dt_util.now()
        events = sorted(
            (
                row.event(complete=self.coordinator.data.collections_complete)
                for row in self.coordinator.data.collections
            ),
            key=lambda event: event.start_datetime_local,
        )
        return next((event for event in events if event.end_datetime_local > now), None)

    async def async_get_events(
        self, hass: HomeAssistant, start_date: datetime, end_date: datetime
    ) -> list[CalendarEvent]:
        coordinator = self.coordinator
        if not coordinator.active:
            raise HomeAssistantError("Utilities is not loaded")
        if end_date <= start_date or end_date - start_date > timedelta(days=366):
            raise HomeAssistantError("Request a calendar window of at most one year")
        try:
            rows, coverage = await coordinator.collections(
                dt_util.as_local(start_date).date().isoformat(),
                dt_util.as_local(end_date - timedelta.resolution).date().isoformat(),
            )
        except (
            ClientError,
            TimeoutError,
            ResponseTooLarge,
            ValueError,
            UpdateFailed,
        ) as err:
            raise HomeAssistantError("Cannot read the collection schedule") from err
        if not coordinator.active or coordinator.data.summary.stale:
            raise HomeAssistantError("Collection data is unavailable or stale")
        complete = coverage is CollectionCoverage.COMPLETE
        events = [
            event
            for row in rows
            if (event := row.event(complete=complete)).start_datetime_local < end_date
            and event.end_datetime_local > start_date
        ]
        if not events and coverage is CollectionCoverage.INCOMPLETE:
            raise HomeAssistantError("Collection coverage is incomplete")
        return events
