"""Config flows, native entities, bridge boundaries, and entry-owned cleanup."""

import asyncio
import json
from datetime import timedelta
from hashlib import sha256

import pytest
from homeassistant.config_entries import SOURCE_USER, ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import issue_registry as ir
from homeassistant.loader import async_get_integration
from homeassistant.util import dt as dt_util
from pytest_homeassistant.common import MockUser
from pytest_homeassistant.typing import ClientSessionGenerator

from custom_components.utilities import UtilitiesConfigEntry
from custom_components.utilities.const import (
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
    MAX_DOCUMENT_BYTES,
    MAX_IMPORT_REQUEST_BYTES,
    MAX_REQUEST_BYTES,
)
from custom_components.utilities.diagnostics import async_get_config_entry_diagnostics
from custom_components.utilities.models import Collection

from .conftest import TOKEN, Backend, collection, coverage, entity_id, json_response

PREFIX = "/api/utilities/v1"


async def test_unavailable_repair_identity_does_not_discard_a_healthy_summary(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, backend: Backend
) -> None:
    coordinator = entry.runtime_data
    assert coordinator.data.summary.bills == 2
    backend.responses["/v1/summary"] = json_response(
        {
            **backend.summary(),
            "bills": 3,
            "stale_automatic_accounts": [
                {
                    "account_id": "example-account",
                    "name": "Example Electric Company",
                    "last_data_at": None,
                }
            ],
        }
    )
    backend.responses["/v1/accounts"] = (503, b"{}", {})
    await coordinator.async_refresh()
    assert coordinator.data.summary.bills == 3
    await hass.async_block_till_done(wait_background_tasks=True)
    assert coordinator.last_update_success


async def test_slow_repair_identity_is_coalesced_and_cancelled_without_blocking_data(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, backend: Backend
) -> None:
    backend.responses["/v1/summary"] = json_response(
        {
            **backend.summary(),
            "bills": 3,
            "stale_automatic_accounts": [
                {"account_id": "example-account", "last_data_at": None}
            ],
        }
    )
    started, release = asyncio.Event(), asyncio.Event()
    backend.gates["/v1/accounts"] = (started, release)
    coordinator = entry.runtime_data
    backend.requests.clear()
    try:
        await asyncio.wait_for(coordinator.async_refresh(), 1)
        await asyncio.wait_for(started.wait(), 1)
        assert coordinator.data.summary.bills == 3
        backend.responses["/v1/summary"] = json_response(
            {
                **backend.summary(),
                "bills": 4,
                "stale_automatic_accounts": [
                    {"account_id": "example-account", "last_data_at": None}
                ],
            }
        )
        await asyncio.wait_for(coordinator.async_refresh(), 1)
        assert coordinator.data.summary.bills == 4
        assert coordinator.last_update_success
        assert sum(row.path == "/v1/accounts" for row in backend.requests) == 1
        assert await asyncio.wait_for(
            hass.config_entries.async_unload(entry.entry_id), 1
        )
    finally:
        release.set()
    await hass.async_block_till_done(wait_background_tasks=True)
    assert not any(
        issue.config_entry_id == entry.entry_id
        and issue.translation_key == "stale_automatic_account_without_data"
        for issue in ir.async_get(hass).issues.values()
    )


async def test_identity_labels_page_once_and_reject_unknown_service_kinds(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, backend: Backend
) -> None:
    second: dict[str, object] = {
        "id": "example-second-account",
        "provider": "starlink",
        "connection_id": None,
        "services": [{"id": "example-device", "kind": "internet"}],
    }
    backend.responses["/v1/summary"] = json_response(
        {
            **backend.summary(),
            "stale_automatic_accounts": [
                {"account_id": "example-account", "last_data_at": None},
                {"account_id": second["id"], "last_data_at": None},
            ],
        }
    )
    backend.responses["/v1/accounts?limit=100"] = json_response(
        {**backend.page(backend.accounts), "next_page": "second"}
    )
    backend.responses["/v1/accounts?limit=100&page=second"] = json_response(
        backend.page([second])
    )
    backend.requests.clear()
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done(wait_background_tasks=True)
    registry = ir.async_get(hass)
    issues = {
        issue.issue_id: issue
        for issue in registry.issues.values()
        if issue.config_entry_id == entry.entry_id
        and issue.translation_placeholders
        and "account" in issue.translation_placeholders
    }
    labels = set()
    for issue in issues.values():
        assert issue.translation_placeholders is not None
        labels.add(issue.translation_placeholders["account"])
    assert labels == {
        "Example Electric Company (Electricity)",
        "Starlink (Internet)",
    }
    assert sum(row.path == "/v1/accounts" for row in backend.requests) == 2
    assert sum(row.path == "/v1/connections" for row in backend.requests) == 1
    second["services"] = [{"id": "example-device", "kind": "other"}]
    backend.responses["/v1/accounts?limit=100&page=second"] = json_response(
        backend.page([second])
    )
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done(wait_background_tasks=True)
    assert entry.runtime_data.last_update_success is True
    assert all(
        registry.async_get_issue(DOMAIN, key) == value for key, value in issues.items()
    )


async def test_identity_read_cannot_recreate_repairs_after_unload(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, backend: Backend
) -> None:
    backend.responses["/v1/summary"] = json_response(
        {
            **backend.summary(),
            "stale_automatic_accounts": [
                {"account_id": "example-account", "last_data_at": None}
            ],
        }
    )
    started, release = asyncio.Event(), asyncio.Event()
    backend.gates["/v1/accounts"] = (started, release)
    coordinator = entry.runtime_data
    pending = asyncio.create_task(coordinator.async_refresh())
    await started.wait()
    coordinator.stop()
    release.set()
    await pending
    assert not any(
        issue.config_entry_id == entry.entry_id
        and issue.translation_key == "stale_automatic_account_without_data"
        for issue in ir.async_get(hass).issues.values()
    )


async def test_registry_customizations_survive_typed_identity_reload(
    hass: HomeAssistant, entry: UtilitiesConfigEntry
) -> None:
    registry = er.async_get(hass)
    identity = entity_id(hass, entry, "bills")
    before = registry.async_get(identity)
    assert before is not None
    registry.async_update_entity(
        identity, name="Custom utilities label", icon="mdi:water"
    )
    assert await hass.config_entries.async_reload(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    after = registry.async_get(identity)
    assert after is not None
    assert after.unique_id == before.unique_id
    assert after.name == "Custom utilities label" and after.icon == "mdi:water"


async def test_flow_entities_redacted_diagnostics_and_unload(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
) -> None:
    assert entry.state is ConfigEntryState.LOADED
    counts = {
        "accounts": "1",
        "bills": "2",
        "collections": "1",
        "usage_intervals": "3",
        "data_status": "current",
    }
    for key, value in counts.items():
        state = hass.states.get(entity_id(hass, entry, key))
        assert state is not None and state.state == value
        assert "example-account" not in json.dumps(dict(state.attributes))
    diagnostics = await async_get_config_entry_diagnostics(hass, entry)
    assert diagnostics["available"] is True
    assert diagnostics["coverage_states"] == ["complete"]
    output = json.dumps(diagnostics)
    for private in (
        TOKEN,
        backend.origin,
        "example-account",
        "example-bill",
        "example-collection",
    ):
        assert private not in output
    client = await hass_client()
    assert (await client.get(f"{PREFIX}/summary")).status == 200
    coordinator = entry.runtime_data
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert coordinator.active is False
    assert (await client.get(f"{PREFIX}/summary")).status == 503
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert entry.runtime_data is not coordinator
    assert entry.runtime_data.active is True
    assert (await client.get(f"{PREFIX}/health")).status == 200


async def test_single_config_entry_and_bad_origin_are_rejected(
    hass: HomeAssistant, backend: Backend
) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_BACKEND_URL: backend.origin + "/path", CONF_TOKEN: TOKEN},
    )
    assert result["type"] is FlowResultType.FORM
    assert result["errors"] == {CONF_BACKEND_URL: "invalid_url"}
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_BACKEND_URL: backend.origin, CONF_TOKEN: "fictional-wrong"},
    )
    assert result["errors"] == {"base": "invalid_auth"}
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {CONF_BACKEND_URL: backend.origin, CONF_TOKEN: TOKEN}
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done(wait_background_tasks=True)
    duplicate = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    assert duplicate["type"] is FlowResultType.ABORT
    assert duplicate["reason"] == "single_instance_allowed"


@pytest.mark.parametrize(
    "state,reason,issue",
    [
        ("not_configured", "missing_credentials", "provider_credentials"),
        ("unavailable", "authentication_required", "provider_auth"),
        ("partial", "provider_reauthentication_required", "provider_auth"),
        ("partial", "provider_window_limited", "coverage_incomplete"),
    ],
)
async def test_coverage_repairs_never_report_current(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    state: str,
    reason: str,
    issue: str,
) -> None:
    backend.coverage = [coverage(state=state, reason=reason)]
    await entry.runtime_data.async_request_refresh()
    status = hass.states.get(entity_id(hass, entry, "data_status"))
    assert status is not None and status.state == "incomplete"
    registry = ir.async_get(hass)
    assert registry.async_get_issue(DOMAIN, f"{entry.entry_id}_{issue}") is not None
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert registry.async_get_issue(DOMAIN, f"{entry.entry_id}_{issue}") is None


async def test_version_repair_waits_for_loaded_version_parity(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, backend: Backend
) -> None:
    registry = ir.async_get(hass)
    issue_id = f"{entry.entry_id}_version_mismatch"
    issue = registry.async_get_issue(DOMAIN, issue_id)
    assert issue is not None and issue.is_fixable and issue.is_persistent
    assert issue.config_entry_id == entry.entry_id

    backend.responses["/v1/system"] = (503, b"{}", {})
    await entry.runtime_data.async_refresh()
    assert registry.async_get_issue(DOMAIN, issue_id) is not None

    assert await hass.config_entries.async_unload(entry.entry_id)
    assert registry.async_get_issue(DOMAIN, issue_id) is not None

    del backend.responses["/v1/system"]
    integration = await async_get_integration(hass, DOMAIN)
    backend.version = str(integration.version)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert registry.async_get_issue(DOMAIN, issue_id) is None


async def test_stale_automatic_accounts_have_independent_repairs(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, backend: Backend
) -> None:
    registry = ir.async_get(hass)
    backend.stale = True
    backend.coverage = [
        coverage(state="partial", reason="provider_reauthentication_required")
    ]
    last_data = dt_util.utcnow() - timedelta(days=41)
    first = {
        "account_id": "example-automatic-1",
        "last_data_at": last_data.isoformat(),
    }
    second = {
        "account_id": "example-automatic-2",
        "last_data_at": None,
    }
    backend.accounts = [
        {**backend.accounts[0], "id": first["account_id"]},
        {
            "id": second["account_id"],
            "provider": "waste_management",
            "connection_id": None,
            "services": [{"id": "example-waste", "kind": "waste"}],
        },
    ]
    backend.responses["/v1/summary"] = json_response(
        {
            **backend.summary(),
            "manual_accounts_stale": False,
            "stale_automatic_accounts": [first, second],
        }
    )
    backend.requests.clear()
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done(wait_background_tasks=True)
    accounts = {
        issue.translation_placeholders["account"]: issue
        for issue in registry.issues.values()
        if issue.config_entry_id == entry.entry_id
        and issue.translation_placeholders
        and "account" in issue.translation_placeholders
    }
    assert set(accounts) == {
        "Example Electric Company (Electricity)",
        "Waste Management (Waste)",
    }
    electricity = accounts["Example Electric Company (Electricity)"]
    waste = accounts["Waste Management (Waste)"]
    assert sum(row.path == "/v1/accounts" for row in backend.requests) == 1
    assert sum(row.path == "/v1/connections" for row in backend.requests) == 1
    assert electricity.issue_id != waste.issue_id
    assert "example-automatic" not in electricity.issue_id + waste.issue_id
    assert electricity.translation_key == "stale_automatic_account"
    assert electricity.translation_placeholders == {
        "account": "Example Electric Company (Electricity)",
        "last_data_date": dt_util.as_local(last_data).date().isoformat(),
    }
    assert waste.translation_key == "stale_automatic_account_without_data"
    assert registry.async_get_issue(DOMAIN, f"{entry.entry_id}_stale_data") is None
    for suffix in ("provider_auth", "coverage_incomplete", "version_mismatch"):
        assert (
            registry.async_get_issue(DOMAIN, f"{entry.entry_id}_{suffix}") is not None
        )

    ir.async_ignore_issue(hass, DOMAIN, electricity.issue_id, True)
    backend.connections[0]["utility_name"] = "Example Electric Cooperative"
    backend.responses["/v1/summary"] = json_response(
        {
            **backend.summary(),
            "manual_accounts_stale": True,
            "stale_automatic_accounts": [
                second,
                first,
            ],
        }
    )
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done(wait_background_tasks=True)
    dismissed = registry.async_get_issue(DOMAIN, electricity.issue_id)
    assert dismissed is not None and dismissed.dismissed_version is not None
    assert dismissed.translation_placeholders == {
        "account": "Example Electric Cooperative (Electricity)",
        "last_data_date": dt_util.as_local(last_data).date().isoformat(),
    }
    assert registry.async_get_issue(DOMAIN, f"{entry.entry_id}_stale_data") is not None

    backend.responses["/v1/summary"] = (503, b"{}", {})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done(wait_background_tasks=True)
    assert registry.async_get_issue(DOMAIN, electricity.issue_id) == dismissed
    assert registry.async_get_issue(DOMAIN, waste.issue_id) is not None

    backend.responses["/v1/summary"] = json_response(
        {
            **backend.summary(),
            "manual_accounts_stale": False,
            "stale_automatic_accounts": [second],
        }
    )
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done(wait_background_tasks=True)
    assert registry.async_get_issue(DOMAIN, electricity.issue_id) is None
    assert registry.async_get_issue(DOMAIN, waste.issue_id) is not None
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert registry.async_get_issue(DOMAIN, waste.issue_id) is None


async def test_automatic_accounts_do_not_get_legacy_sync_staleness_repair(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, backend: Backend
) -> None:
    registry = ir.async_get(hass)
    issue_id = f"{entry.entry_id}_stale_data"
    backend.stale = True
    await entry.runtime_data.async_refresh()
    assert registry.async_get_issue(DOMAIN, issue_id) is not None

    backend.responses["/v1/summary"] = json_response(
        {
            **backend.summary(),
            "manual_accounts_stale": False,
            "stale_automatic_accounts": [],
        }
    )
    await entry.runtime_data.async_refresh()
    assert registry.async_get_issue(DOMAIN, issue_id) is None


async def test_bridge_allows_reads_and_admin_sync_only(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
) -> None:
    user = await hass_client(hass_user_token)
    admin = await hass_client()
    backend.requests.clear()
    response = await user.get(f"{PREFIX}/bills?limit=1&from=2026-01-01&to=2026-12-31")
    assert response.status == 200
    assert response.headers["Cache-Control"] == "no-store"
    assert backend.requests[-1].query == {
        "limit": "1",
        "from": "2026-01-01",
        "to": "2026-12-31",
    }
    assert backend.requests[-1].authorization == f"Bearer {TOKEN}"
    body = {"account": "example-account", "from": "2026-01-01", "to": "2026-01-02"}
    backend.requests.clear()
    assert (await user.post(f"{PREFIX}/sync", json=body)).status == 403
    assert backend.requests == []
    assert (await admin.post(f"{PREFIX}/sync", json=body)).status == 202
    assert backend.requests[-1].method == "POST"
    assert json.loads(backend.requests[-1].body) == body
    assert (await user.get(f"{PREFIX}/health")).status == 200
    assert backend.requests[-1].path == "/health"


async def test_bridge_import_routes_require_admin_and_bound_their_bodies(
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
) -> None:
    user = await hass_client(hass_user_token)
    admin = await hass_client()
    document = b"Fictional source bytes\n" * 256
    assert len(document) > MAX_REQUEST_BYTES
    path = f"documents/{sha256(document).hexdigest()}"
    upload = {"Content-Type": "application/octet-stream"}
    manifest = b'{"records":[]}' + b" " * MAX_REQUEST_BYTES
    metadata = {"Content-Type": "application/json"}
    backend.responses[f"/v1/{path}"] = (204, b"", {})
    backend.responses["/v1/imports"] = (200, b"{}", {})

    backend.requests.clear()
    assert (
        await user.put(f"{PREFIX}/{path}", data=document, headers=upload)
    ).status == 403
    assert (
        await user.post(f"{PREFIX}/imports", data=manifest, headers=metadata)
    ).status == 403
    assert (await admin.get(f"{PREFIX}/{path}")).status == 405
    assert (
        await admin.put(f"{PREFIX}/documents/not-a-sha256", data=document)
    ).status == 404
    assert (
        await admin.put(f"{PREFIX}/{path}", data=document, headers=metadata)
    ).status == 415
    assert (
        await admin.put(
            f"{PREFIX}/{path}", data=b"x" * (MAX_DOCUMENT_BYTES + 1), headers=upload
        )
    ).status == 413
    assert (
        await admin.post(
            f"{PREFIX}/imports",
            data=b" " * (MAX_IMPORT_REQUEST_BYTES + 1),
            headers=metadata,
        )
    ).status == 413
    assert (
        await admin.post(f"{PREFIX}/sync", data=manifest, headers=metadata)
    ).status == 413
    assert not backend.requests

    assert (
        await admin.put(f"{PREFIX}/{path}", data=document, headers=upload)
    ).status == 204
    assert (
        await admin.post(f"{PREFIX}/imports", data=manifest, headers=metadata)
    ).status == 200
    assert [
        (received.method, received.path, received.content_type, received.body)
        for received in backend.requests
    ] == [
        ("PUT", f"/v1/{path}", "application/octet-stream", document),
        ("POST", "/v1/imports", "application/json", manifest),
    ]
    assert all(
        request.authorization == f"Bearer {TOKEN}" for request in backend.requests
    )


async def test_bridge_consolidation_is_an_admin_only_bounded_import_command(
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
) -> None:
    user = await hass_client(hass_user_token)
    admin = await hass_client()
    path = "imports/consolidate"
    metadata = {"Content-Type": "application/json"}
    body = b'{"source":{},"target":{}}'
    backend.responses[f"/v1/{path}"] = (200, b'{"replayed":false}', {})
    backend.requests.clear()
    assert (
        await user.post(f"{PREFIX}/{path}", data=body, headers=metadata)
    ).status == 403
    assert (await admin.get(f"{PREFIX}/{path}")).status == 405
    assert (await admin.post(f"{PREFIX}/{path}", data=body)).status == 415
    assert (
        await admin.post(
            f"{PREFIX}/{path}",
            data=b"x" * (MAX_IMPORT_REQUEST_BYTES + 1),
            headers=metadata,
        )
    ).status == 413
    assert not backend.requests
    response = await admin.post(f"{PREFIX}/{path}", data=body, headers=metadata)
    assert response.status == 200
    assert response.headers["Cache-Control"] == "no-store"
    assert len(backend.requests) == 1
    received = backend.requests[0]
    assert (received.method, received.path, received.content_type, received.body) == (
        "POST",
        f"/v1/{path}",
        "application/json",
        body,
    )
    assert received.authorization == f"Bearer {TOKEN}"


async def test_hosted_connections_require_admin_even_for_the_authorization_link(
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
) -> None:
    user = await hass_client(hass_user_token)
    admin = await hass_client()
    identity = "01900000-0000-7000-8000-000000000001"
    paths = [
        "connections",
        "providers/bayou/utilities",
        f"connections/{identity}",
        f"connections/{identity}/authorization",
    ]
    for path in paths:
        backend.responses[f"/v1/{path}"] = (200, b"{}", {})
        backend.requests.clear()
        assert (await user.get(f"{PREFIX}/{path}")).status == 403
        assert backend.requests == []
        response = await admin.get(f"{PREFIX}/{path}")
        assert response.status == 200
        assert response.headers["Cache-Control"] == "no-store"
        assert backend.requests[-1].path == f"/v1/{path}"
    backend.requests.clear()
    assert (
        await admin.get(f"{PREFIX}/connections/not-a-uuid/authorization")
    ).status == 404
    assert backend.requests == []
    body = {
        "provider": "bayou",
        "request_id": identity,
        "utility_id": "speculoos_power",
        "currency": "USD",
        "accept_provider_settings": True,
    }
    backend.responses["/v1/connections"] = (202, b"{}", {})
    backend.requests.clear()
    assert (await user.post(f"{PREFIX}/connections", json=body)).status == 403
    assert backend.requests == []
    assert (await admin.post(f"{PREFIX}/connections", json=body)).status == 202
    assert json.loads(backend.requests[-1].body) == body
    for extra in [
        {"name": "Removed freeform account label"},
        {"password": "fictional-never-forward"},
        {"accept_provider_settings": False},
        {"request_id": "invalid"},
    ]:
        backend.requests.clear()
        assert (
            await admin.post(f"{PREFIX}/connections", json={**body, **extra})
        ).status == 400
        assert backend.requests == []


@pytest.mark.parametrize(
    "flags,accepted",
    [
        ({"read_only_providers": True}, True),
        (
            {
                "read_only_providers": False,
                "read_only_sync": True,
                "hosted_connections": True,
            },
            True,
        ),
        ({"read_only_providers": False}, False),
    ],
)
async def test_flow_accepts_the_explicit_hosted_setup_contract(
    hass: HomeAssistant,
    backend: Backend,
    flags: dict[str, bool],
    accepted: bool,
) -> None:
    backend.responses["/v1/system"] = (
        200,
        json.dumps({"name": "Utilities", "version": "0.1.2", **flags}).encode(),
        {},
    )
    flow = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        flow["flow_id"], {CONF_BACKEND_URL: backend.origin, CONF_TOKEN: TOKEN}
    )
    if accepted:
        assert result["type"] is FlowResultType.CREATE_ENTRY
    else:
        assert result["type"] is FlowResultType.FORM


@pytest.mark.parametrize(
    "method,path,payload,status",
    [
        ("post", "bills", {}, 405),
        ("get", "payments", None, 404),
        ("post", "book", {}, 404),
        ("get", "accounts?url=https://example.invalid", None, 400),
        ("get", "bills?limit=1&limit=2", None, 400),
        (
            "post",
            "sync",
            {"account": "example-account", "from": "2026-02-01", "to": "2026-01-01"},
            400,
        ),
        (
            "post",
            "sync",
            {
                "account": "example-account",
                "from": "2026-01-01",
                "to": "2026-01-02",
                "pay": True,
            },
            400,
        ),
        (
            "post",
            "sync",
            {"account": "example-account", "from": "bad", "to": "bad"},
            400,
        ),
    ],
)
async def test_bridge_rejects_unapproved_or_ambiguous_actions(
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    method: str,
    path: str,
    payload: object,
    status: int,
) -> None:
    client = await hass_client()
    backend.requests.clear()
    response = await client.request(method, f"{PREFIX}/{path}", json=payload)
    assert response.status == status
    assert backend.requests == []


async def test_bridge_rechecks_user_after_await(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    hass_user: MockUser,
    hass_user_token: str,
) -> None:
    client = await hass_client(hass_user_token)
    started, release = asyncio.Event(), asyncio.Event()
    backend.gates["/v1/bills"] = (started, release)
    pending = asyncio.create_task(client.get(f"{PREFIX}/bills"))
    await started.wait()
    await hass.auth.async_update_user(hass_user, is_active=False)
    release.set()
    response = await pending
    assert response.status == 403
    assert "example-bill" not in await response.text()


async def test_late_refresh_cannot_recreate_unloaded_repairs(
    hass: HomeAssistant, entry: UtilitiesConfigEntry, backend: Backend
) -> None:
    coordinator = entry.runtime_data
    started, release = asyncio.Event(), asyncio.Event()
    backend.gates["/v1/summary"] = (started, release)
    pending = asyncio.create_task(coordinator.async_refresh())
    await started.wait()
    backend.stale = True
    backend.responses["/v1/summary"] = json_response(
        {
            **backend.summary(),
            "manual_accounts_stale": True,
            "stale_automatic_accounts": [
                {
                    "account_id": "example-late-account",
                    "last_data_at": None,
                }
            ],
        }
    )
    coordinator.stop()
    release.set()
    await pending
    registry = ir.async_get(hass)
    assert registry.async_get_issue(DOMAIN, f"{entry.entry_id}_stale_data") is None
    assert not any(
        issue.config_entry_id == entry.entry_id
        and issue.translation_key == "stale_automatic_account_without_data"
        for issue in registry.issues.values()
    )


async def test_calendar_all_day_dates_and_bounded_pagination(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
) -> None:
    client = await hass_client()
    calendar = entity_id(hass, entry, "collections_calendar", "calendar")
    now = dt_util.now()
    response = await client.get(
        f"/api/calendars/{calendar}",
        params={"start": now.isoformat(), "end": (now + timedelta(days=3)).isoformat()},
    )
    assert response.status == 200
    events = await response.json()
    assert len(events) == 1
    row = Collection.parse(collection())
    assert events[0]["start"] == {"date": row.scheduled_date.isoformat()}
    assert events[0]["end"] == {
        "date": (row.scheduled_date + timedelta(days=1)).isoformat()
    }
    backend.responses["/v1/collections"] = json_response(
        {"items": [collection()], "next_page": "repeat", "coverage": backend.coverage}
    )
    with pytest.raises(ValueError, match="Repeated collection"):
        await entry.runtime_data.collections(
            now.date().isoformat(), (now + timedelta(days=3)).date().isoformat()
        )


async def test_partial_calendar_shows_known_dates_without_claiming_empty_coverage(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
) -> None:
    backend.coverage = [coverage(state="partial", reason="next_pickup_only")]
    await entry.runtime_data.async_refresh()
    calendar = entity_id(hass, entry, "collections_calendar", "calendar")
    state = hass.states.get(calendar)
    assert state is not None
    assert state.attributes["coverage_complete"] is False
    client = await hass_client()
    now = dt_util.now()
    response = await client.get(
        f"/api/calendars/{calendar}",
        params={"start": now.isoformat(), "end": (now + timedelta(days=3)).isoformat()},
    )
    assert response.status == 200
    events = await response.json()
    assert len(events) == 1
    assert "Coverage is partial" in events[0]["description"]
    backend.collections = []
    response = await client.get(
        f"/api/calendars/{calendar}",
        params={"start": now.isoformat(), "end": (now + timedelta(days=3)).isoformat()},
    )
    assert response.status != 200


async def test_bridge_rechecks_native_entity_read_permission(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    hass_user: MockUser,
    hass_user_token: str,
    backend: Backend,
    hass_client: ClientSessionGenerator,
) -> None:
    from homeassistant.auth.models import Group

    client = await hass_client(hass_user_token)
    hass_user.groups = [Group(name="Example denied group", policy={"entities": {}})]
    backend.requests.clear()
    assert (await client.get(f"{PREFIX}/summary")).status == 403
    assert backend.requests == []


async def test_observation_identifiers_can_exceed_account_identifier_length(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
) -> None:
    identity = "example:" + "x" * 390
    backend.collections = [collection(identity)]
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.data.collections[0].id == identity
    client = await hass_client()
    backend.requests.clear()
    assert (await client.get(f"{PREFIX}/bills/{identity}")).status == 404
    assert (
        len(backend.requests) == 1
        and backend.requests[0].path == f"/v1/bills/{identity}"
    )


@pytest.mark.parametrize("revocation", ["inactive", "admin", "unload"])
async def test_streamed_sync_body_cannot_outlive_caller_or_entry_authorization(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
    hass_user: MockUser,
    hass_user_token: str,
    monkeypatch: pytest.MonkeyPatch,
    revocation: str,
) -> None:
    from collections.abc import AsyncIterator

    from homeassistant.auth.const import GROUP_ID_ADMIN, GROUP_ID_USER

    from custom_components.utilities import http as utilities_http
    from custom_components.utilities.coordinator import UtilitiesCoordinator

    admin_group = await hass.auth.async_get_group(GROUP_ID_ADMIN)
    user_group = await hass.auth.async_get_group(GROUP_ID_USER)
    assert admin_group is not None and user_group is not None
    hass_user.groups = [admin_group]
    client = await hass_client(hass_user_token)
    first_authorized, finish_body = asyncio.Event(), asyncio.Event()
    original_authorized = utilities_http.authorized

    async def observe_authorized(
        current_hass: HomeAssistant,
        configured_entry: UtilitiesConfigEntry,
        coordinator: UtilitiesCoordinator,
        user_id: str | None,
        *,
        assistant: str | None = None,
        admin: bool = False,
    ) -> bool:
        allowed = await original_authorized(
            current_hass,
            configured_entry,
            coordinator,
            user_id,
            assistant=assistant,
            admin=admin,
        )
        if allowed:
            first_authorized.set()
        return allowed

    # Synchronize only the race window. The actual native authorization still runs.
    monkeypatch.setattr(utilities_http, "authorized", observe_authorized)

    async def body_chunks() -> AsyncIterator[bytes]:
        yield b'{"account":"example-account",'
        await finish_body.wait()
        yield b'"from":"2026-01-01","to":"2026-01-02"}'

    backend.requests.clear()
    pending = asyncio.create_task(
        client.post(
            f"{PREFIX}/sync",
            data=body_chunks(),
            headers={"Content-Type": "application/json"},
        )
    )
    try:
        async with asyncio.timeout(5):
            await first_authorized.wait()
            if revocation == "inactive":
                await hass.auth.async_update_user(hass_user, is_active=False)
            elif revocation == "admin":
                hass_user.groups = [user_group]
            else:
                assert await hass.config_entries.async_unload(entry.entry_id)
            finish_body.set()
            response = await pending
        assert response.status == 403
        assert not [request for request in backend.requests if request.method == "POST"]
    finally:
        finish_body.set()
        if not pending.done():
            pending.cancel()
            await asyncio.gather(pending, return_exceptions=True)


async def test_calendar_exact_exclusive_window_can_return_complete_empty_results(
    hass: HomeAssistant,
    entry: UtilitiesConfigEntry,
    backend: Backend,
    hass_client: ClientSessionGenerator,
) -> None:
    start = dt_util.now().replace(hour=0, minute=0, second=0, microsecond=0)
    end = start + timedelta(days=3)
    bounded = coverage()
    bounded["from"] = start.isoformat()
    bounded["to"] = end.isoformat()
    backend.coverage = [bounded]
    backend.collections = []
    client = await hass_client()
    calendar = entity_id(hass, entry, "collections_calendar", "calendar")
    response = await client.get(
        f"/api/calendars/{calendar}",
        params={"start": start.isoformat(), "end": end.isoformat()},
    )
    assert response.status == 200, await response.text()
    assert await response.json() == []
    assert backend.requests[-1].query == {
        "from": start.date().isoformat(),
        "to": (end - timedelta.resolution).date().isoformat(),
        "limit": "100",
    }
