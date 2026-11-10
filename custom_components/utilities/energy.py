"""Append validated hourly electricity data to HA's native statistics store."""

import math
from collections.abc import Awaitable, Callable
from datetime import UTC, datetime, timedelta
from decimal import Decimal
from hashlib import sha256
from urllib.parse import urlencode

from homeassistant.core import HomeAssistant
from homeassistant.util import dt as dt_util

from .const import DOMAIN, MAX_USAGE_ROWS
from .models import Coverage, Summary, Usage, page, record, text


def statistic_id(entry_id: str, service_id: str) -> str:
    """Identifiers contain no provider meter/account identifiers or private labels."""
    digest = sha256(service_id.encode()).hexdigest()[:24]
    return f"{DOMAIN}:{entry_id.lower()}_{digest}"


def _covered(
    rows: tuple[Coverage, ...], account: str, start: datetime, end: datetime
) -> bool:
    now = dt_util.utcnow()
    return any(
        row.account_id == account
        and row.capability == "usage"
        and row.state == "complete"
        and row.start is not None
        and row.start <= start
        and row.end is not None
        and row.end >= end
        and -300 <= (now - row.checked_at).total_seconds() < 24 * 3600
        for row in rows
    )


def hourly(
    rows: list[Usage], start: datetime, end: datetime, initial_sum: Decimal
) -> list[tuple[datetime, Decimal]]:
    """Only complete contiguous hours can become Energy statistics.

    Intervals may partition an hour, but are never spread across hours, merged
    across meters, or converted from demand units. Daily totals stay daily data.
    """
    if start.minute or start.second or start.microsecond or end <= start:
        raise ValueError("An exact hourly window is required")
    ordered = sorted(rows, key=lambda row: row.start)
    position = start
    hour_end = start + timedelta(hours=1)
    total = initial_sum
    results: list[tuple[datetime, Decimal]] = []
    for row in ordered:
        if row.start != position or row.end > hour_end or row.unit not in {"Wh", "KWh"}:
            raise ValueError("Usage units or continuity are not suitable for Energy")
        total += row.quantity / 1000 if row.unit == "Wh" else row.quantity
        position = row.end
        if position == hour_end:
            results.append((hour_end - timedelta(hours=1), total))
            hour_end += timedelta(hours=1)
    if position != end:
        raise ValueError("Usage does not cover the complete hourly window")
    return results


async def async_update(
    hass: HomeAssistant,
    entry_id: str,
    summary: Summary,
    read: Callable[[str], Awaitable[object]],
    active: Callable[[], bool],
) -> bool:
    """Import only append-contiguous, fresh local data. Return incomplete status.

    Recorder remains the owner of imported sums and restart continuity. Nothing
    is imported without explicit complete usage coverage or a running recorder.
    """
    # This is optional: disabled recorder and unsupported usage are not zero usage.
    if summary.stale or not active() or "recorder" not in hass.config.components:
        return False
    complete = [
        row
        for row in summary.coverage
        if row.capability == "usage"
        and row.state == "complete"
        and row.start
        and row.end
    ]
    if not complete:
        return False
    from homeassistant.components.recorder import get_instance
    from homeassistant.components.recorder.models import (
        StatisticData,
        StatisticMeanType,
        StatisticMetaData,
    )
    from homeassistant.components.recorder.statistics import (
        async_add_external_statistics,
        get_last_statistics,
    )

    instance = get_instance(hass)
    accounts, cursor, _ = page(await read("/v1/accounts"))
    if not active() or cursor is not None:
        return True
    incomplete = False
    for raw_account in accounts:
        account = record(raw_account)
        account_id = text(account.get("id"))
        services = account.get("services")
        if not isinstance(services, list) or len(services) > 64:
            raise ValueError("Invalid service bindings")
        for raw_service in services:
            service = record(raw_service)
            if service.get("kind") != "electricity":
                continue
            service_id = text(service.get("id"))
            account_coverage = [row for row in complete if row.account_id == account_id]
            if not account_coverage:
                continue
            coverage_start = max(
                row.start for row in account_coverage if row.start is not None
            )
            coverage_end = min(
                row.end for row in account_coverage if row.end is not None
            )
            identity = statistic_id(entry_id, service_id)
            latest = await instance.async_add_executor_job(
                get_last_statistics, hass, 1, identity, False, {"sum"}
            )
            if not active():
                return True
            midnight = dt_util.utcnow().replace(
                hour=0, minute=0, second=0, microsecond=0
            )
            start, initial = midnight - timedelta(days=7), Decimal(0)
            first_hour = coverage_start.replace(minute=0, second=0, microsecond=0)
            if first_hour != coverage_start:
                first_hour += timedelta(hours=1)
            start = max(start, first_hour)
            if existing := latest.get(identity):
                timestamp, value = existing[0].get("start"), existing[0].get("sum")
                if (
                    not isinstance(timestamp, int | float)
                    or not isinstance(value, int | float)
                    or not math.isfinite(value)
                ):
                    incomplete = True
                    continue
                start, initial = (
                    datetime.fromtimestamp(timestamp, UTC) + timedelta(hours=1),
                    Decimal(str(value)),
                )
            end = min(
                start.replace(hour=0, minute=0, second=0, microsecond=0)
                + timedelta(days=7),
                midnight,
                coverage_end.replace(minute=0, second=0, microsecond=0),
            )
            if start >= end:
                continue
            if not _covered(summary.coverage, account_id, start, end):
                incomplete = True
                continue
            rows: list[Usage] = []
            fetched = 0
            cursors: set[str] = set()
            cursor = None
            while fetched < MAX_USAGE_ROWS:
                query = {
                    "account": account_id,
                    "service": service_id,
                    "from": start.date().isoformat(),
                    "to": (end - timedelta(microseconds=1)).date().isoformat(),
                    "limit": "100",
                }
                if cursor is not None:
                    query["page"] = cursor
                items, cursor, current_coverage = page(
                    await read("/v1/usage?" + urlencode(query))
                )
                fetched += len(items)
                if not active():
                    return True
                if not _covered(current_coverage, account_id, start, end):
                    raise ValueError("Usage coverage changed during import")
                for raw in items:
                    row = Usage.parse(raw)
                    if row.account_id != account_id or row.service_id != service_id:
                        raise ValueError(
                            "Usage does not match the explicit service binding"
                        )
                    if row.start >= start and row.end <= end:
                        rows.append(row)
                if cursor is None:
                    break
                if cursor in cursors or not items:
                    raise ValueError("Non-advancing usage page")
                cursors.add(cursor)
            if cursor is not None:
                incomplete = True
                continue
            try:
                points = hourly(rows, start, end, initial)
            except ValueError:
                incomplete = True
                continue
            statistics: list[StatisticData] = [
                {"start": when, "sum": float(total)} for when, total in points
            ]
            if not all(math.isfinite(point["sum"]) for point in statistics):
                incomplete = True
                continue
            metadata: StatisticMetaData = {
                "mean_type": StatisticMeanType.NONE,
                "has_sum": True,
                "name": "Utilities electricity",
                "source": DOMAIN,
                "statistic_id": identity,
                "unit_class": "energy",
                "unit_of_measurement": "kWh",
            }
            if active():
                async_add_external_statistics(hass, metadata, statistics)
    return incomplete
