"""Entry-owned polling with explicit provider coverage."""

import asyncio
import logging
from datetime import date, timedelta
from hashlib import sha256
from urllib.parse import urlencode

from aiohttp import ClientError
from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import ConfigEntryAuthFailed
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed
from homeassistant.util import dt as dt_util

from ._app_kit.repairs import VersionRepairs
from ._app_kit.versions import ReleaseVersion
from .client import ResponseTooLarge, UtilitiesClient
from .const import DOMAIN, MAX_CALENDAR_ROWS
from .energy import async_update as async_update_energy
from .models import (
    AccountIdentity,
    Collection,
    Coverage,
    Snapshot,
    StaleAutomaticAccount,
    Summary,
    page,
    record,
    text,
    valid_system,
)

_LOGGER = logging.getLogger(__name__)
REPAIRS = (
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
    ) -> None:
        super().__init__(
            hass,
            _LOGGER,
            name=DOMAIN,
            config_entry=entry,
            update_interval=timedelta(minutes=5),
        )
        self.client = client
        self.entry = entry
        self.active = True
        self.backend_version: str | None = None
        self.version_repairs: VersionRepairs | None = None
        self._automatic_accounts: tuple[StaleAutomaticAccount, ...] = ()
        self._labels_task: asyncio.Task[None] | None = None

    @callback
    def stop(self) -> None:
        """Block late network completions from recreating entry-owned Repairs."""
        self.active = False
        if self._labels_task is not None:
            self._labels_task.cancel()
        for issue in REPAIRS:
            ir.async_delete_issue(self.hass, DOMAIN, f"{self.entry.entry_id}_{issue}")
        self._async_update_automatic_repairs((), {})

    @callback
    def _async_update_automatic_repairs(
        self, accounts: tuple[StaleAutomaticAccount, ...], labels: dict[str, str]
    ) -> None:
        """Keep dismissals for ongoing issues and remove resolved accounts."""
        prefix = f"{self.entry.entry_id}_stale_automatic_account_"
        current = {
            prefix + sha256(account.account_id.encode()).hexdigest()
            for account in accounts
        }
        for (domain, identity), issue in tuple(ir.async_get(self.hass).issues.items()):
            if (
                domain == DOMAIN
                and identity.startswith(prefix)
                and issue.config_entry_id == self.entry.entry_id
                and identity not in current
            ):
                ir.async_delete_issue(self.hass, DOMAIN, identity)
        for account in accounts:
            if account.account_id not in labels:
                # A failed/slow optional identity read does not resolve an
                # ongoing issue or discard its last known company label.
                continue
            identity = sha256(account.account_id.encode()).hexdigest()
            placeholders = {"account": labels[account.account_id]}
            key = "stale_automatic_account_without_data"
            if account.last_data_at is not None:
                key = "stale_automatic_account"
                placeholders["last_data_date"] = (
                    dt_util.as_local(account.last_data_at).date().isoformat()
                )
            ir.async_create_issue(
                self.hass,
                DOMAIN,
                prefix + identity,
                config_entry_id=self.entry.entry_id,
                is_fixable=False,
                severity=ir.IssueSeverity.WARNING,
                translation_key=key,
                translation_placeholders=placeholders,
            )

    @callback
    def _async_schedule_automatic_repairs(
        self, accounts: tuple[StaleAutomaticAccount, ...]
    ) -> None:
        """Repair labels never gate Summary, calendar or Energy polling."""
        self._automatic_accounts = accounts
        self._async_update_automatic_repairs(accounts, {})
        if not accounts:
            if self._labels_task is not None:
                self._labels_task.cancel()
            return
        if self._labels_task is None or self._labels_task.done():
            self._labels_task = self.entry.async_create_background_task(
                self.hass,
                self._async_refresh_labels(accounts),
                "Utilities Repair labels",
                eager_start=False,
            )

    async def _async_refresh_labels(
        self, accounts: tuple[StaleAutomaticAccount, ...]
    ) -> None:
        try:
            labels = await self.account_labels(accounts)
        except (
            ClientError,
            TimeoutError,
            ResponseTooLarge,
            ValueError,
            UpdateFailed,
            ConfigEntryAuthFailed,
        ):
            _LOGGER.debug("Cannot refresh Utilities Repair labels")
            return
        if self.active and self.entry.state not in {
            ConfigEntryState.UNLOAD_IN_PROGRESS,
            ConfigEntryState.NOT_LOADED,
        }:
            self._async_update_automatic_repairs(self._automatic_accounts, labels)

    async def read(self, path: str) -> object:
        response = await self.client.request("GET", path)
        if response.status in {401, 403}:
            raise ConfigEntryAuthFailed("The Utilities credential was rejected")
        if response.status != 200:
            raise UpdateFailed("Utilities is unavailable")
        return response.json()

    async def identity_rows(self, route: str) -> list[object]:
        """Read one bounded listing, never a separate request for each account."""
        rows: list[object] = []
        cursor: str | None = None
        seen: set[str] = set()
        while len(rows) < 1000:
            limit = min(100, 1000 - len(rows))
            query = {"limit": str(limit)}
            if cursor is not None:
                query["page"] = cursor
            items, cursor, _ = page(
                await self.read(route + "?" + urlencode(query)), maximum=limit
            )
            rows.extend(items)
            if cursor is None:
                return rows
            if cursor in seen or not items:
                raise ValueError("Non-advancing identity page")
            seen.add(cursor)
        raise ValueError("Incomplete account identity listing")

    async def account_labels(
        self, stale: tuple[StaleAutomaticAccount, ...]
    ) -> dict[str, str]:
        """Company comes from the canonical connection or the closed provider."""
        if not stale:
            return {}
        accounts: dict[str, AccountIdentity] = {}
        for value in await self.identity_rows("/v1/accounts"):
            account = AccountIdentity.parse(value)
            if account.id in accounts:
                raise ValueError("Repeated account identity")
            accounts[account.id] = account
        if any(account.account_id not in accounts for account in stale):
            raise ValueError("Missing automatic account identity")
        connections: dict[str, str] = {}
        if any(
            accounts[account.account_id].connection_id is not None for account in stale
        ):
            for value in await self.identity_rows("/v1/connections"):
                row = record(value)
                identity = text(row.get("id"))
                if identity in connections:
                    raise ValueError("Repeated connection identity")
                connections[identity] = text(row.get("utility_name"), max_length=500)
        return {
            account.account_id: accounts[account.account_id].label(connections)
            for account in stale
        }

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
            if not valid_system(system):
                raise ValueError("Unexpected backend")
            self.backend_version = text(system.get("version"))
            if self.active and self.version_repairs is not None:
                try:
                    version = ReleaseVersion.parse(self.backend_version)
                except ValueError:
                    # A development build cannot establish released version parity.
                    pass
                else:
                    self.version_repairs.async_report(version)
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
                "stale_data": summary.manual_accounts_stale,
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
            self._async_schedule_automatic_repairs(summary.stale_automatic_accounts)
        return Snapshot(summary, collections, complete)
