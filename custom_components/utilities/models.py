"""Strict snapshots at the HTTP boundary, not a second utility database."""

from dataclasses import dataclass
from datetime import date, datetime, timedelta
from decimal import Decimal, InvalidOperation
from typing import Literal

from homeassistant.components.calendar import CalendarEvent
from homeassistant.util import dt as dt_util
from homeassistant.util.json import JsonValueType

CAPABILITIES = {"bills", "balances", "payments", "services", "collections", "usage"}
COVERAGE_STATES = {
    "complete",
    "partial",
    "unavailable",
    "unsupported",
    "not_configured",
}


def record(value: object) -> dict[str, object]:
    """JSON objects, never arbitrary objects with a get method."""
    if not isinstance(value, dict) or any(not isinstance(key, str) for key in value):
        raise ValueError("Expected object")
    return value


def json_value(value: object, depth: int = 0) -> JsonValueType:
    """Narrow decoded JSON at the Assist boundary without unchecked casts."""
    if depth > 32:
        raise ValueError("JSON nesting is too deep")
    if value is None or isinstance(value, str | int | float | bool):
        return value
    if isinstance(value, list):
        return [json_value(item, depth + 1) for item in value]
    if isinstance(value, dict) and all(isinstance(key, str) for key in value):
        return {key: json_value(item, depth + 1) for key, item in value.items()}
    raise ValueError("Expected JSON data")


def text(value: object, *, max_length: int = 200) -> str:
    """Reject missing, unbounded, or control-bearing identifiers."""
    if (
        not isinstance(value, str)
        or not value
        or len(value) > max_length
        or any(ord(char) < 32 for char in value)
    ):
        raise ValueError("Expected bounded text")
    return value


def timestamp(value: object) -> datetime:
    """A naive timestamp cannot be interpreted as local time."""
    parsed = dt_util.parse_datetime(text(value))
    if parsed is None or parsed.tzinfo is None:
        raise ValueError("Expected zoned datetime")
    return dt_util.as_utc(parsed)


def optional_timestamp(value: object) -> datetime | None:
    return None if value is None else timestamp(value)


def count(value: object) -> int:
    if type(value) is not int or value < 0:
        raise ValueError("Expected nonnegative integer")
    return value


@dataclass(frozen=True, slots=True)
class Coverage:
    account_id: str
    capability: str
    state: str
    checked_at: datetime
    reason: str | None
    start: datetime | None
    end: datetime | None

    @classmethod
    def parse(cls, value: object) -> Coverage:
        row = record(value)
        capability, state = row.get("capability"), row.get("state")
        if (
            not isinstance(capability, str)
            or not isinstance(state, str)
            or capability not in CAPABILITIES
            or state not in COVERAGE_STATES
        ):
            raise ValueError("Invalid coverage")
        return cls(
            text(row.get("account_id")),
            text(capability),
            text(state),
            timestamp(row.get("checked_at")),
            text(row["reason"]) if row.get("reason") is not None else None,
            optional_timestamp(row.get("from")),
            optional_timestamp(row.get("to")),
        )


def coverage(value: object) -> tuple[Coverage, ...]:
    if not isinstance(value, list) or len(value) > 1000:
        raise ValueError("Expected bounded coverage")
    return tuple(Coverage.parse(row) for row in value)


def page(
    value: object, *, maximum: int = 100
) -> tuple[list[object], str | None, tuple[Coverage, ...]]:
    row = record(value)
    items = row.get("items")
    if not isinstance(items, list) or len(items) > maximum:
        raise ValueError("Expected bounded page")
    cursor = row.get("next_page")
    if cursor is not None:
        cursor = text(cursor, max_length=4096)
    return items, cursor, coverage(row.get("coverage"))


@dataclass(frozen=True, slots=True)
class Summary:
    accounts: int
    bills: int
    collections: int
    usage_intervals: int
    coverage: tuple[Coverage, ...]
    last_synced_at: datetime | None
    stale: bool

    @classmethod
    def parse(cls, value: object) -> Summary:
        row = record(value)
        stale = row.get("stale")
        if not isinstance(stale, bool):
            raise ValueError("Expected freshness")
        return cls(
            count(row.get("accounts")),
            count(row.get("bills")),
            count(row.get("collections")),
            count(row.get("usage_intervals")),
            coverage(row.get("coverage")),
            optional_timestamp(row.get("last_synced_at")),
            stale,
        )


@dataclass(frozen=True, slots=True)
class Collection:
    id: str
    service_id: str
    scheduled_date: date
    status: str
    observed_at: datetime

    @classmethod
    def parse(cls, value: object) -> Collection:
        row = record(value)
        status = row.get("status")
        if not isinstance(status, str) or status not in {
            "scheduled",
            "completed",
            "exception",
            "unknown",
        }:
            raise ValueError("Invalid collection status")
        return cls(
            text(row.get("id"), max_length=500),
            text(row.get("service_id")),
            date.fromisoformat(text(row.get("scheduled_date"))),
            text(status),
            timestamp(row.get("observed_at")),
        )

    def event(self, *, complete: bool = True) -> CalendarEvent:
        """Date-only provider schedules remain all day, with exclusive end."""
        return CalendarEvent(
            start=self.scheduled_date,
            end=self.scheduled_date + timedelta(days=1),
            summary=f"{self.service_id} collection",
            description=(
                f"Provider status: {self.status}. Not a booking confirmation."
                + (
                    ""
                    if complete
                    else " Coverage is partial; other collections may be missing."
                )
            ),
            uid=self.id,
        )


@dataclass(frozen=True, slots=True)
class Usage:
    id: str
    account_id: str
    service_id: str
    start: datetime
    end: datetime
    quantity: Decimal
    unit: Literal["Wh", "KWh", "Therm", "Ccf", "Gallon", "CubicMeter"]

    @classmethod
    def parse(cls, value: object) -> Usage:
        row = record(value)
        try:
            quantity = Decimal(text(row.get("quantity"), max_length=80))
        except InvalidOperation as err:
            raise ValueError("Invalid quantity") from err
        start, end, unit = (
            timestamp(row.get("start")),
            timestamp(row.get("end")),
            row.get("unit"),
        )
        if (
            not quantity.is_finite()
            or quantity < 0
            or start >= end
            or not isinstance(unit, str)
            or unit not in {"Wh", "KWh", "Therm", "Ccf", "Gallon", "CubicMeter"}
        ):
            raise ValueError("Invalid usage interval")
        # Match narrows the closed unit vocabulary without casts.
        match unit:
            case "Wh" | "KWh" | "Therm" | "Ccf" | "Gallon" | "CubicMeter":
                return cls(
                    text(row.get("id"), max_length=500),
                    text(row.get("account_id")),
                    text(row.get("service_id")),
                    start,
                    end,
                    quantity,
                    unit,
                )
        raise ValueError("Invalid usage unit")


@dataclass(frozen=True, slots=True)
class Snapshot:
    summary: Summary
    collections: tuple[Collection, ...]
    collections_complete: bool
