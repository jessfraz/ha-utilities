"""Actual HA recorder import with fictional meter intervals and restart continuity."""

from datetime import timedelta
from pathlib import Path

import pytest
from homeassistant.components.recorder import Recorder, get_instance
from homeassistant.components.recorder.statistics import get_last_statistics
from homeassistant.core import HomeAssistant
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.recorder import async_initialize_recorder
from homeassistant.setup import async_setup_component
from homeassistant.util import dt as dt_util
from pytest_homeassistant.components.recorder.common import async_wait_recording_done

from custom_components.utilities.const import DOMAIN
from custom_components.utilities.energy import statistic_id

from .conftest import Backend, connect, coverage


@pytest.fixture
async def recorder_instance(hass: HomeAssistant, tmp_path: Path) -> Recorder:
    """Run the native recorder against disposable SQLite, without mocks."""
    async_initialize_recorder(hass)
    assert await async_setup_component(
        hass,
        "recorder",
        {
            "recorder": {
                "db_url": f"sqlite:///{tmp_path / 'recorder.sqlite3'}",
                "commit_interval": 0,
            }
        },
    )
    await hass.async_block_till_done(wait_background_tasks=True)
    await async_wait_recording_done(hass)
    return get_instance(hass)


def energy_data(backend: Backend) -> None:
    start = dt_util.utcnow().replace(
        hour=0, minute=0, second=0, microsecond=0
    ) - timedelta(days=7)
    backend.coverage = [coverage(), coverage("usage")]
    backend.usage = [
        {
            "id": f"example-hour-{index}",
            "account_id": "example-account",
            "service_id": "example-electric",
            "start": (start + timedelta(hours=index)).isoformat(),
            "end": (start + timedelta(hours=index + 1)).isoformat(),
            "quantity": "1000",
            "unit": "Wh",
            "observed_at": dt_util.utcnow().isoformat(),
        }
        for index in range(7 * 24)
    ]


@pytest.mark.parametrize("minutes", [60, 15])
async def test_native_energy_statistics_use_kwh_and_do_not_double_count_after_reload(
    hass: HomeAssistant, recorder_instance: Recorder, backend: Backend, minutes: int
) -> None:
    energy_data(backend)
    if minutes == 15:
        rows = []
        for row in backend.usage:
            start = dt_util.parse_datetime(str(row["start"]))
            assert start is not None
            for index in range(4):
                rows.append(
                    dict(
                        row,
                        id=f"{row['id']}-{index}",
                        start=(start + timedelta(minutes=15 * index)).isoformat(),
                        end=(start + timedelta(minutes=15 * (index + 1))).isoformat(),
                        quantity="250",
                    )
                )
        backend.usage = rows
    entry = await connect(hass, backend)
    await entry.runtime_data.async_refresh()
    await async_wait_recording_done(hass)
    identity = statistic_id(entry.entry_id, "example-electric")
    stored = await recorder_instance.async_add_executor_job(
        get_last_statistics, hass, 200, identity, False, {"sum"}
    )
    assert len(stored[identity]) == 168
    assert stored[identity][0]["sum"] == 168.0
    assert stored[identity][-1]["sum"] == 1.0
    assert "example-electric" not in identity
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    await entry.runtime_data.async_refresh()
    await async_wait_recording_done(hass)
    repeated = await recorder_instance.async_add_executor_job(
        get_last_statistics, hass, 200, identity, False, {"sum"}
    )
    assert repeated == stored


@pytest.mark.parametrize("problem", ["gap", "overlap", "unit", "daily", "coverage"])
async def test_unsuitable_energy_data_never_enters_recorder(
    hass: HomeAssistant, recorder_instance: Recorder, backend: Backend, problem: str
) -> None:
    energy_data(backend)
    if problem == "gap":
        del backend.usage[12]
    elif problem == "overlap":
        backend.usage.insert(1, dict(backend.usage[0], id="example-overlap"))
    elif problem == "unit":
        backend.usage[12]["unit"] = "Therm"
    elif problem == "daily":
        backend.usage[0]["end"] = backend.usage[23]["end"]
    else:
        backend.coverage[1]["state"] = "partial"
    entry = await connect(hass, backend)
    await entry.runtime_data.async_refresh()
    await async_wait_recording_done(hass)
    identity = statistic_id(entry.entry_id, "example-electric")
    stored = await recorder_instance.async_add_executor_job(
        get_last_statistics, hass, 200, identity, False, {"sum"}
    )
    assert not stored
    if problem != "coverage":
        assert (
            ir.async_get(hass).async_get_issue(
                DOMAIN, f"{entry.entry_id}_energy_statistics"
            )
            is not None
        )


async def test_unloading_during_energy_read_cannot_import_late_statistics(
    hass: HomeAssistant, recorder_instance: Recorder, backend: Backend
) -> None:
    import asyncio

    energy_data(backend)
    entry = await connect(hass, backend)
    started, release = asyncio.Event(), asyncio.Event()
    backend.gates["/v1/usage"] = (started, release)
    pending = asyncio.create_task(entry.runtime_data.async_refresh())
    await started.wait()
    assert await hass.config_entries.async_unload(entry.entry_id)
    release.set()
    await pending
    await async_wait_recording_done(hass)
    identity = statistic_id(entry.entry_id, "example-electric")
    stored = await recorder_instance.async_add_executor_job(
        get_last_statistics, hass, 200, identity, False, {"sum"}
    )
    assert not stored
    assert (
        ir.async_get(hass).async_get_issue(
            DOMAIN, f"{entry.entry_id}_energy_statistics"
        )
        is None
    )
