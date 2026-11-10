"""Entry-owned polling with explicit provider coverage."""

import logging
from datetime import date, timedelta
from urllib.parse import urlencode

from aiohttp import ClientError
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import ConfigEntryAuthFailed
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed
from homeassistant.util import dt as dt_util

from .client import ResponseTooLarge, UtilitiesClient
from .const import DOMAIN, MAX_CALENDAR_ROWS
from .energy import async_update as async_update_energy
from .models import Collection, Coverage, Snapshot, Summary, page, record, text

_LOGGER = logging.getLogger(__name__)
REPAIRS = (
    "version_mismatch",
    "provider_credentials",
    "provider_auth",
    "coverage_incomplete",
    "stale_data",
    "energy_statistics",
)


class UtilitiesCoordinator(DataUpdateCoordinator[Snapshot]):
    """Core owns the shared HTTP session and cancels entry-owned polling."""

    def __init__(
        self,
        hass: HomeAssistant,
        entry: ConfigEntry,
        client: UtilitiesClient,
        version: str,
    ) -> None:
        super().__init__(
            hass,
            _LOGGER,
            name=DOMAIN,
            config_entry=entry,
            update_interval=timedelta(minutes=5),
        )
        self.client = client
        self.integration_version = version
        self.entry = entry
        self.active = True
        self.backend_version: str | None = None

    @callback
    def stop(self) -> None:
        """Block late network completions from recreating entry-owned Repairs."""
        self.active = False
        for issue in REPAIRS:
            ir.async_delete_issue(self.hass, DOMAIN, f"{self.entry.entry_id}_{issue}")

    async def read(self, path: str) -> object:
        response = await self.client.request("GET", path)
        if response.status in {401, 403}:
            raise ConfigEntryAuthFailed("The Utilities credential was rejected")
        if response.status != 200:
            raise UpdateFailed("Utilities is unavailable")
        return response.json()

    async def collections(
        self, start: str, end: str
    ) -> tuple[tuple[Collection, ...], bool]:
        """Read a bounded calendar window; never silently drop another page."""
        rows: list[Collection] = []
        identities: set[str] = set()
        cursors: set[str] = set()
        cursor: str | None = None
        current_coverage: tuple[Coverage, ...] = ()
        coverage_pages: list[Coverage] = []
        while len(rows) < MAX_CALENDAR_ROWS:
            query = {"from": start, "to": end, "limit": "100"}
            if cursor is not None:
                query["page"] = cursor
            items, cursor, current_coverage = page(
                await self.read("/v1/collections?" + urlencode(query))
            )
            coverage_pages.extend(current_coverage)
            for item in items:
                collection = Collection.parse(item)
                if collection.id in identities:
                    raise ValueError("Repeated collection")
                identities.add(collection.id)
                rows.append(collection)
            if cursor is None:
                eligible = [
                    row
                    for row in coverage_pages
                    if row.capability == "collections" and row.state != "unsupported"
                ]
                now = dt_util.utcnow()
                complete = bool(eligible) and all(
                    row.state == "complete"
                    and row.start is not None
                    and row.end is not None
                    and row.start.date() <= date.fromisoformat(start)
                    and row.end.date() > date.fromisoformat(end)
                    and -300 <= (now - row.checked_at).total_seconds() < 24 * 3600
                    for row in eligible
                )
                return tuple(rows), complete
            if cursor in cursors or not items:
                raise ValueError("Non-advancing collection page")
            cursors.add(cursor)
        return tuple(rows), False

    async def _async_update_data(self) -> Snapshot:
        try:
            system = record(await self.read("/v1/system"))
            if (
                system.get("name") != "Utilities"
                or system.get("read_only_providers") is not True
            ):
                raise ValueError("Unexpected backend")
            self.backend_version = text(system.get("version"))
            summary = Summary.parse(await self.read("/v1/summary"))
            today = dt_util.now().date()
            collections, complete = await self.collections(
                today.isoformat(), (today + timedelta(days=31)).isoformat()
            )
        except (ClientError, TimeoutError, ResponseTooLarge, ValueError) as err:
            raise UpdateFailed("Cannot read Utilities") from err
        energy_incomplete = False
        if self.active and self.entry.state is ConfigEntryState.LOADED:
            try:
                energy_incomplete = await async_update_energy(
                    self.hass,
                    self.entry.entry_id,
                    summary,
                    self.read,
                    lambda: self.active and self.entry.state is ConfigEntryState.LOADED,
                )
            except ClientError, TimeoutError, ValueError, UpdateFailed:
                energy_incomplete = True
        if self.active and self.entry.state not in {
            ConfigEntryState.UNLOAD_IN_PROGRESS,
            ConfigEntryState.NOT_LOADED,
        }:
            states = {row.state for row in summary.coverage}
            reasons = {row.reason for row in summary.coverage}
            issues = {
                "version_mismatch": self.backend_version != self.integration_version,
                "provider_credentials": "not_configured" in states,
                "provider_auth": bool(
                    reasons
                    & {
                        "authentication",
                        "authentication_required",
                        "provider_reauthentication_required",
                        "access_denied",
                    }
                ),
                "coverage_incomplete": bool(states & {"partial", "unavailable"})
                or not summary.coverage,
                "stale_data": summary.stale,
                "energy_statistics": energy_incomplete,
            }
            for issue, present in issues.items():
                issue_id = f"{self.entry.entry_id}_{issue}"
                if present:
                    ir.async_create_issue(
                        self.hass,
                        DOMAIN,
                        issue_id,
                        is_fixable=False,
                        severity=ir.IssueSeverity.WARNING,
                        translation_key=issue,
                    )
                else:
                    ir.async_delete_issue(self.hass, DOMAIN, issue_id)
        return Snapshot(summary, collections, complete)
