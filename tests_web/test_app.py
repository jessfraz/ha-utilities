"""Exercise real read-model views, paging, filters, and recoverable failures."""

import asyncio
import re
import sqlite3
from datetime import UTC, datetime, timedelta
from hashlib import sha256
from pathlib import Path
from typing import Literal
from urllib.parse import parse_qs, urlsplit

import pytest
from aiohttp import ClientSession, web
from playwright.async_api import Locator, Page, Request, Route, async_playwright, expect
from pytest_aiohttp.plugin import AiohttpServer

from tests_web.conftest import WebBackend


async def open_app(page: Page, backend: WebBackend) -> None:
    """Wait for the visible application, not an implementation-only readiness flag."""
    await page.goto(backend.origin)
    await expect(
        page.get_by_role("heading", name="Overview", exact=True)
    ).to_be_visible()
    await expect(page.locator("#content")).to_have_attribute("aria-busy", "false")
    await expect(page.get_by_role("region", name="Saved utility data")).to_be_visible()
    await expect(page.locator(".coverage-panel")).to_have_count(0)


@pytest.mark.parametrize("web_backend", ["account_cards"], indirect=True)
async def test_settings_show_sources_categories_and_collapsed_diagnostics(
    utility_page: Page, web_backend: WebBackend, tmp_path: Path
) -> None:
    source = b"Fictional statement for account display tests"
    digest = sha256(source).hexdigest()
    imported_accounts = [
        {
            "id": "fictional-water",
            "name": "Water",
            "provider": "lvmwd",
            "services": [
                {"id": "water-meter", "name": "Water", "kind": "water"},
                {"id": "second-water-meter", "name": "Water", "kind": "water"},
            ],
        },
        {
            "id": "fictional-internet",
            "name": "Internet (Starlink)",
            "provider": "starlink",
            "services": [
                {"id": "fixed-dish", "name": "Fixed dish", "kind": "internet"},
                {"id": "portable-dish", "name": "Portable dish", "kind": "internet"},
            ],
        },
    ]
    async with ClientSession(
        headers={"Authorization": f"Bearer {web_backend.token}"}
    ) as client:
        async with client.put(
            f"{web_backend.origin}/v1/documents/{digest}",
            data=source,
            headers={"Content-Type": "application/octet-stream"},
        ) as response:
            assert response.status == 204
        for account in imported_accounts:
            async with client.post(
                f"{web_backend.origin}/v1/imports",
                json={
                    "account": {**account, "start_on": "2025-02-01"},
                    "currency": "USD",
                    "records": [
                        {
                            "kind": "bill",
                            "reference": {
                                "kind": "invoice_number",
                                "value": account["id"],
                            },
                            "issued_on": "2025-02-20",
                            "totals": {"new_charges_minor": 1000},
                            "sources": [{"sha256": digest, "role": "invoice"}],
                        }
                    ],
                },
            ) as response:
                assert response.status == 200, await response.text()

    page = utility_page
    await open_app(page, web_backend)
    await expect(page.locator(".coverage-panel")).to_have_count(0)
    for view in ("Bills", "Usage"):
        await page.get_by_role("button", name=view, exact=True).click()
        await expect(page.locator("#content")).to_have_attribute("aria-busy", "false")
        await expect(page.locator(".coverage-panel")).to_have_count(0)
        if view == "Bills":
            await expect(
                page.get_by_role(
                    "button", name=re.compile(r"^View Waste Management bill issued")
                ).first
            ).to_be_visible()
    await page.get_by_role("button", name="Settings", exact=True).click()
    await expect(
        page.get_by_role("heading", name="Utility accounts", exact=True)
    ).to_be_visible()
    await expect(
        page.get_by_role("heading", name="Bound accounts", exact=True)
    ).to_have_count(0)

    cards = page.locator(".account-grid article")
    electricity = cards.filter(
        has=page.get_by_role("heading", name="Electricity (SCE)", exact=True)
    )
    await expect(electricity.get_by_text("Bayou", exact=True)).to_have_count(1)
    await expect(
        electricity.get_by_role(
            "group", name="Data source and service categories"
        ).locator(".badge")
    ).to_have_text(["Bayou", "Electricity"])
    await expect(electricity.locator("li")).to_have_count(0)
    water = cards.filter(has=page.get_by_role("heading", name="LVMWD", exact=True))
    await expect(water.get_by_text("Manual import", exact=True)).to_have_count(1)
    await expect(
        water.get_by_role("group", name="Data source and service categories").locator(
            ".badge"
        )
    ).to_have_text(["Manual import", "Water"])
    await expect(water.locator("li")).to_have_count(0)
    await expect(water.locator(".account-services")).to_have_count(0)
    waste = cards.filter(
        has=page.get_by_role("heading", name="Waste Management", exact=True)
    )
    await expect(waste.get_by_text("Waste Management API", exact=True)).to_have_count(1)
    await expect(waste.get_by_text("Waste Management", exact=True)).to_have_count(1)
    await page.get_by_label("Account to sync", exact=True).select_option("demo-wm")
    await expect(page.locator("#sync-availability")).to_have_text(
        "Waste Management · Provider credentials are not configured."
    )
    await expect(page.locator("#sync-status")).not_to_contain_text(
        "Waste Management · Waste Management"
    )
    internet = cards.filter(
        has=page.get_by_role("heading", name="Starlink", exact=True)
    )
    await expect(internet.get_by_text("Manual import", exact=True)).to_have_count(1)
    await expect(
        internet.get_by_role(
            "group", name="Data source and service categories"
        ).locator(".badge")
    ).to_have_text(["Manual import", "Internet"])
    await expect(internet.get_by_text("Starlink", exact=True)).to_have_count(1)
    await expect(internet.get_by_text("Fixed dish", exact=True)).to_be_hidden()
    await expect(internet.get_by_text("Portable dish", exact=True)).to_be_hidden()
    await page.screenshot(path=str(tmp_path / "settings-desktop.png"), full_page=True)
    await page.emulate_media(color_scheme="dark")
    await page.screenshot(path=str(tmp_path / "settings-dark.png"), full_page=True)
    await page.emulate_media(color_scheme="light")
    await internet.get_by_text("Services", exact=True).click()
    await expect(internet.locator(".account-services .capabilities span")).to_have_text(
        ["Fixed dish", "Portable dish"]
    )
    await expect(cards.locator("ul, ol, li")).to_have_count(0)

    diagnostics = page.locator("details.diagnostics")
    await expect(diagnostics).to_have_js_property("open", False)
    await expect(page.get_by_label("Utility providers")).to_be_hidden()
    await expect(page.get_by_role("heading", name="Data coverage")).to_be_hidden()
    await page.set_viewport_size({"width": 390, "height": 844})
    await assert_horizontal_visibility(
        page, page.get_by_role("button", name="Settings", exact=True)
    )
    await expect(page.locator("#freshness-badge .badge")).to_have_text(
        [
            "Bayou · Electricity (SCE) · Needs attention",
            "Waste Management · Needs attention",
        ]
    )
    await page.screenshot(path=str(tmp_path / "settings-mobile.png"), full_page=True)
    assert await page.evaluate(
        "document.documentElement.scrollWidth <= window.innerWidth"
    )
    await diagnostics.get_by_text("Diagnostics", exact=True).click()
    await expect(page.get_by_label("Utility providers")).to_be_visible()
    await expect(page.get_by_role("heading", name="Data coverage")).to_be_visible()
    await diagnostics.get_by_text("Diagnostics", exact=True).click()
    await expect(page.get_by_label("Utility providers")).to_be_hidden()
    await expect(page.get_by_role("heading", name="Data coverage")).to_be_hidden()


async def assert_horizontal_visibility(page: Page, locator: Locator) -> None:
    viewport = page.viewport_size
    assert viewport is not None
    box = await locator.bounding_box()
    assert box is not None
    assert box["x"] >= 0 and box["x"] + box["width"] <= viewport["width"]


@pytest.mark.parametrize("web_backend", ["bayou_only"], indirect=True)
async def test_electricity_account_has_no_trash_tab_or_collection_requests(
    utility_page: Page,
    web_backend: WebBackend,
) -> None:
    page = utility_page
    requests: list[str] = []
    page.on("request", lambda request: requests.append(request.url))
    await open_app(page, web_backend)
    await expect(
        page.get_by_role("button", name=re.compile(r"^(Trash )?[Cc]ollections$"))
    ).to_have_count(0)
    await expect(page.get_by_text("Saved trash collections", exact=True)).to_have_count(
        0
    )
    await page.goto(web_backend.origin + "/#collections")
    await expect(
        page.get_by_role("heading", name="Overview", exact=True)
    ).to_be_visible()
    await page.get_by_role("button", name="Usage", exact=True).click()
    await expect(
        page.get_by_label("Account", exact=True).locator("option")
    ).to_have_count(2)
    await expect(page.get_by_role("button", name="Bills", exact=True)).to_be_visible()
    assert not any("/v1/collections" in url for url in requests)


async def test_native_sign_in_is_masked_and_uses_only_an_httponly_session(
    web_backend: WebBackend,
) -> None:
    """Exercise real token rejection and session creation through the login UI."""
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            requests: list[Request] = []
            page.on("request", lambda request: requests.append(request))
            await page.goto(web_backend.origin)
            await expect(
                page.get_by_role("heading", name="Sign in to Utilities")
            ).to_be_visible()
            token = page.get_by_label("API token", exact=True)
            await expect(token).to_have_attribute("type", "password")
            await token.fill("fictional-rejected-token")
            await page.get_by_role("button", name="Sign in", exact=True).click()
            await expect(page.get_by_role("alert")).to_contain_text(
                "This token was not accepted"
            )
            await token.fill(web_backend.token)
            await page.get_by_role("button", name="Sign in", exact=True).click()
            await expect(
                page.get_by_role("heading", name="Overview", exact=True)
            ).to_be_visible()
            await expect(page.locator("#content")).to_have_attribute(
                "aria-busy", "false"
            )
            await expect(
                page.get_by_role("region", name="Saved utility data")
            ).to_be_visible()
            cookie = next(
                cookie
                for cookie in await page.context.cookies()
                if cookie["name"] == "utilities_session"
            )
            assert cookie["httpOnly"] is True and cookie["sameSite"] == "Strict"
            assert await page.evaluate("Object.keys(localStorage)") == []
            assert await page.evaluate("Object.keys(sessionStorage)") == []
            assert all(web_backend.token not in request.url for request in requests)
            assert web_backend.token not in await page.content()
        finally:
            await browser.close()


async def test_relative_assets_and_api_requests_work_behind_a_real_proxy_prefix(
    web_backend: WebBackend, aiohttp_server: AiohttpServer
) -> None:
    """Forward to the real Rust service without replacing its API responses."""
    async with ClientSession() as client:

        async def proxy(request: web.Request) -> web.Response:
            headers = {
                key: value
                for key, value in request.headers.items()
                if key.lower() not in {"host", "content-length"}
            }
            async with client.request(
                request.method,
                f"{web_backend.origin}/{request.match_info['path']}",
                params=request.query,
                headers=headers,
                data=await request.read(),
                allow_redirects=False,
            ) as response:
                return web.Response(
                    status=response.status,
                    headers={
                        key: value
                        for key, value in response.headers.items()
                        if key.lower() not in {"transfer-encoding", "content-length"}
                    },
                    body=await response.read(),
                )

        app = web.Application()
        app.router.add_route("*", "/example-prefix/{path:.*}", proxy)
        server = await aiohttp_server(app)
        origin = str(server.make_url("/example-prefix/"))
        async with async_playwright() as playwright:
            browser = await playwright.chromium.launch()
            try:
                page = await browser.new_page()
                requests: list[Request] = []
                page.on("request", lambda request: requests.append(request))
                await page.goto(origin)
                await page.get_by_label("API token", exact=True).fill(web_backend.token)
                await page.get_by_role("button", name="Sign in", exact=True).click()
                await expect(page.locator("#content")).to_have_attribute(
                    "aria-busy", "false"
                )
                await expect(
                    page.get_by_role("region", name="Saved utility data")
                ).to_be_visible()
                await page.get_by_role("button", name="Bills", exact=True).click()
                await expect(page.locator(".data-table tbody tr")).to_have_count(25)
                assert all(
                    urlsplit(request.url).path.startswith("/example-prefix/")
                    for request in requests
                )
                await expect(page.locator(".wordmark")).to_have_attribute(
                    "href", origin
                )
            finally:
                await browser.close()


async def test_bills_page_filter_and_honest_read_only_detail(
    utility_page: Page, web_backend: WebBackend, tmp_path: Path
) -> None:
    """Pagination reads the real producer, and provider processing is not payment."""
    page = utility_page
    requests: list[Request] = []
    page.on("request", lambda request: requests.append(request))
    await open_app(page, web_backend)
    await page.get_by_role("button", name="Bills", exact=True).click()
    await expect(page.locator(".data-table tbody tr")).to_have_count(25)
    account_badges = page.locator(".data-table .account-badge")
    energy_badges = account_badges.filter(has_text=re.compile("^Energy service$"))
    waste_badges = account_badges.filter(has_text=re.compile("^Waste service$"))
    await expect(energy_badges).not_to_have_count(0)
    await expect(waste_badges).not_to_have_count(0)
    await expect(
        page.locator(".data-table tbody tr td:nth-child(2) small")
    ).to_have_count(0)
    energy_colors: list[str] = await energy_badges.evaluate_all(
        "(badges) => badges.map((badge) => getComputedStyle(badge).backgroundColor)"
    )
    waste_colors: list[str] = await waste_badges.evaluate_all(
        "(badges) => badges.map((badge) => getComputedStyle(badge).backgroundColor)"
    )
    assert len(set(energy_colors)) == len(set(waste_colors)) == 1
    assert energy_colors[0] != waste_colors[0]
    await page.screenshot(path=str(tmp_path / "bills-desktop.png"))
    await page.emulate_media(color_scheme="dark")
    await page.screenshot(path=str(tmp_path / "bills-dark.png"))
    await page.emulate_media(color_scheme="light")
    await page.set_viewport_size({"width": 390, "height": 844})
    first_row = page.locator(".data-table tbody tr").first
    account_box = await first_row.locator(".account-badge").bounding_box()
    issued_box = await first_row.get_by_role("button").first.bounding_box()
    assert account_box is not None and issued_box is not None
    assert account_box["y"] < issued_box["y"]
    await page.screenshot(path=str(tmp_path / "bills-mobile.png"))
    await page.set_viewport_size({"width": 1280, "height": 720})
    await expect(
        page.get_by_role("status").filter(has_text="25 records shown")
    ).to_be_visible()
    for _ in range(4):
        more = page.get_by_role("button", name="Load more", exact=True)
        if await more.count() == 0:
            break
        previous = await page.locator(".data-table tbody tr").count()
        await more.click()
        await expect(page.locator(".data-table tbody tr")).not_to_have_count(previous)
    await expect(
        page.get_by_role("button", name="Load more", exact=True)
    ).to_be_hidden()
    pages = [
        parse_qs(urlsplit(request.url).query).get("page")
        for request in requests
        if urlsplit(request.url).path == "/v1/bills"
    ]
    assert any(pages), "Load more must follow the real continuation token"
    await page.get_by_label("Account", exact=True).select_option("demo-energy")
    await page.get_by_role("button", name="Apply filters", exact=True).click()
    await expect(page.locator("#content")).to_have_attribute("aria-busy", "false")
    rows = page.locator(".data-table tbody tr")
    assert await rows.count() > 0
    await expect(rows.first.locator("td").nth(4)).to_contain_text("Synced from Bayou")
    first_bill = rows.first.get_by_role("button")
    await first_bill.click()
    dialog = page.get_by_role("dialog", name="Bill details")
    await expect(dialog.locator(".account-badge")).to_have_css(
        "background-color", energy_colors[0]
    )
    await expect(dialog.get_by_text("unlocked", exact=True)).to_be_visible()
    await expect(
        dialog.get_by_text("Previous-cycle balance", exact=True)
    ).to_be_visible()
    await expect(
        dialog.get_by_text(re.compile("not a live account balance"))
    ).to_be_visible()
    await expect(
        dialog.get_by_text(re.compile("not whether a payment settled"))
    ).to_be_visible()
    await expect(
        dialog.get_by_role("button", name=re.compile("pay", re.I))
    ).to_have_count(0)
    await page.keyboard.press("Escape")
    await expect(dialog).to_be_hidden()
    await expect(first_bill).to_be_focused()
    await page.get_by_label("From", exact=True).fill("2100-01-01")
    await page.get_by_label("To", exact=True).fill("2100-01-02")
    await page.get_by_role("button", name="Apply filters", exact=True).click()
    await expect(page.get_by_role("heading", name="No bills reported")).to_be_visible()
    await expect(
        page.get_by_text("No saved records match these filters.", exact=True)
    ).to_be_visible()
    assert not [request for request in requests if request.method != "GET"]


@pytest.mark.parametrize("web_backend", ["vendor_prefix"], indirect=True)
async def test_imported_vendor_bills_include_construction_across_pages(
    utility_page: Page, web_backend: WebBackend
) -> None:
    """Use real imported accounts with distinct names and a shared provider."""
    today = datetime.now(UTC).date()
    source = b"Fictional Spectrum billing evidence."
    digest = sha256(source).hexdigest()
    async with ClientSession(
        headers={"Authorization": f"Bearer {web_backend.token}"}
    ) as client:
        async with client.put(
            f"{web_backend.origin}/v1/documents/{digest}", data=source
        ) as response:
            response.raise_for_status()
        for account, name, service, service_name, offsets in [
            (
                "fixture-internet",
                "Home internet",
                "internet",
                "Internet service",
                range(7, 32),
            ),
            (
                "fixture-construction",
                "Construction account",
                "installation",
                "Internet construction",
                [32],
            ),
        ]:
            records = [
                {
                    "kind": "bill",
                    "reference": {
                        "kind": "invoice_number",
                        "value": f"{account}-{offset}",
                    },
                    "issued_on": (today - timedelta(days=offset)).isoformat(),
                    "due_on": None
                    if offset == 32
                    else (
                        today - timedelta(days=offset) + timedelta(days=14)
                    ).isoformat(),
                    "totals": {"new_charges_minor": 200000 if offset == 32 else 7500},
                    "sources": [{"sha256": digest, "role": "invoice"}],
                    "charges": [
                        {
                            "kind": "installation",
                            "label": "Aerial construction",
                            "amount_minor": 200000,
                            "service_id": service,
                        }
                    ]
                    if offset == 32
                    else [],
                }
                for offset in offsets
            ]
            async with client.post(
                f"{web_backend.origin}/v1/imports",
                json={
                    "account": {
                        "id": account,
                        "name": name,
                        "provider": "spectrum",
                        "start_on": (today - timedelta(days=60)).isoformat(),
                        "services": [
                            {"id": service, "name": service_name, "kind": "internet"}
                        ],
                    },
                    "currency": "USD",
                    "records": records,
                },
            ) as response:
                response.raise_for_status()

    page = utility_page
    requests: list[Request] = []
    page.on("request", lambda request: requests.append(request))
    await open_app(page, web_backend)
    await expect(
        page.get_by_role("heading", name="Imported vendors", exact=True)
    ).to_have_count(0)
    await page.get_by_role("button", name="Bills", exact=True).click()
    filters = page.get_by_role("form", name="Filter bills")
    await filters.get_by_label("Account or vendor").select_option(
        "import_provider/spectrum"
    )
    await filters.get_by_role("button", name="Apply filters", exact=True).click()
    await expect(page.locator("#content")).to_have_attribute("aria-busy", "false")
    await expect(filters.get_by_label("Account or vendor")).to_have_value(
        "import_provider/spectrum"
    )
    await expect(
        filters.get_by_role("option", name="Spectrum", exact=True)
    ).to_have_count(1)
    rows = page.get_by_role(
        "table", name="Bills matching the selected filters"
    ).get_by_role("row")
    await expect(rows).to_have_count(26)
    vendor_color = await rows.locator(".account-badge").first.evaluate(
        "(badge) => getComputedStyle(badge).backgroundColor"
    )
    await expect(
        page.get_by_role("columnheader", name="Source", exact=True)
    ).to_be_visible()
    await expect(rows.nth(1)).to_contain_text("Original documents")
    await page.get_by_role("button", name="Load more", exact=True).click()
    await expect(rows).to_have_count(27)
    await expect(rows.last).to_contain_text("Construction account")
    await expect(rows.last.locator(".account-badge")).to_have_css(
        "background-color", vendor_color
    )
    await expect(rows.last).to_contain_text("Date unavailable")
    await page.set_viewport_size({"width": 390, "height": 844})
    due = today + timedelta(days=7)
    await expect(rows.nth(1).locator("td").nth(2)).to_have_text(
        f"{due:%b} {due.day}, {due:%Y}"
    )
    for cell in [rows.nth(1).locator("td").nth(index) for index in [1, 2, 3, 4]]:
        await assert_horizontal_visibility(page, cell)
    assert not any(
        urlsplit(request.url).path.endswith("/details") for request in requests
    )
    await rows.last.get_by_role(
        "button", name=re.compile("^View original documents")
    ).click()
    dialog = page.get_by_role("dialog", name="Bill details")
    await expect(
        dialog.get_by_role("heading", name="Spectrum", exact=True)
    ).to_be_visible()
    await expect(dialog.get_by_text("Construction account", exact=True)).to_be_visible()
    await expect(dialog.locator(".account-badge")).to_have_css(
        "background-color", vendor_color
    )
    await expect(dialog.get_by_text("Aerial construction", exact=True)).to_be_visible()
    await expect(
        dialog.get_by_role("link", name="Download invoice", exact=True)
    ).to_have_attribute("href", f"v1/documents/{digest}")
    await expect(
        dialog.get_by_role("link", name="Download invoice", exact=True)
    ).to_be_focused()
    await expect(dialog.get_by_text("Date unavailable", exact=True)).to_be_visible()
    await page.keyboard.press("Escape")
    await filters.get_by_label("Account or vendor").select_option(
        "import_provider:spectrum"
    )
    await filters.get_by_role("button", name="Apply filters").click()
    await expect(rows.last).to_contain_text("Energy service")
    await expect(rows.get_by_text("Spectrum", exact=True)).to_have_count(0)

    await page.get_by_role("button", name="Settings", exact=True).click()
    await expect(
        page.get_by_role("heading", name="Utility accounts", exact=True)
    ).to_be_visible()
    card = page.locator(".account-grid article").filter(
        has=page.get_by_role("heading", name="Spectrum", exact=True)
    )
    await expect(card).to_have_count(1)
    await expect(card.locator(".badge")).to_have_text(["Manual import", "Internet"])
    for name in ("Home internet", "Construction account"):
        await expect(page.get_by_role("heading", name=name, exact=True)).to_have_count(
            0
        )
    await expect(page.get_by_role("region", name="Utility providers")).to_be_hidden()
    await page.set_viewport_size({"width": 390, "height": 844})
    await expect(card).to_be_visible()
    assert not await page.evaluate("document.documentElement.scrollWidth > innerWidth")
    await page.get_by_label("Account to sync", exact=True).select_option("demo-wm")
    await expect(
        page.get_by_role("button", name="Sync account", exact=True)
    ).to_be_disabled()


async def test_usage_includes_billed_water_periods_and_source_documents(
    utility_page: Page, web_backend: WebBackend, tmp_path: Path
) -> None:
    today = datetime.now(UTC).date()
    source = b"Fictional water billing evidence."
    digest = sha256(source).hexdigest()
    records = []
    for index in range(32):
        issued = today - timedelta(days=14 + index * 7)
        start = issued - timedelta(days=10)
        end = issued - timedelta(days=2)
        records.append(
            {
                "kind": "bill",
                "reference": {"kind": "invoice_number", "value": f"WATER-{index}"},
                "issued_on": issued.isoformat(),
                "period": {
                    "start": start.isoformat(),
                    "end_exclusive": end.isoformat(),
                },
                "totals": {"new_charges_minor": 2345},
                "services": [
                    {
                        "kind": "water",
                        "service_id": "fixture-water-meter",
                        "billed_volume": {
                            "value": "0"
                            if index == 0
                            else "12.75"
                            if index == 31
                            else "23.5",
                            "unit": "cubic_meter"
                            if index == 31
                            else "hundred_cubic_feet",
                        },
                        "meter_reads": [
                            {
                                "on": start.isoformat(),
                                "register": "100",
                                "quality": "unknown",
                            },
                            {
                                "on": end.isoformat(),
                                "register": "100",
                                "quality": "unknown",
                            },
                        ]
                        if index == 0
                        else [],
                    }
                ],
                "sources": [{"sha256": digest, "role": "invoice", "pages": [1]}],
            }
        )
    async with ClientSession(
        headers={"Authorization": f"Bearer {web_backend.token}"}
    ) as client:
        async with client.put(
            f"{web_backend.origin}/v1/documents/{digest}", data=source
        ) as response:
            response.raise_for_status()
        async with client.post(
            f"{web_backend.origin}/v1/imports",
            json={
                "account": {
                    "id": "fixture-water",
                    "name": "Water service",
                    "provider": "lvmwd",
                    "start_on": (today - timedelta(days=365)).isoformat(),
                    "services": [
                        {
                            "id": "fixture-water-meter",
                            "name": "Water meter",
                            "kind": "water",
                        }
                    ],
                },
                "currency": "USD",
                "records": records,
            },
        ) as response:
            response.raise_for_status()

    page = utility_page
    await open_app(page, web_backend)
    await expect(
        page.get_by_role("region", name="Account freshness")
    ).not_to_contain_text("LVMWD")
    await page.get_by_role("button", name="Usage", exact=True).click()
    billed = page.get_by_role("region", name="Billed usage", exact=True)
    rows = billed.get_by_role("table").locator("tbody tr")
    await expect(rows).to_have_count(32)
    await expect(rows.first).to_contain_text("0 CCF")
    await expect(rows.last).to_contain_text("12.75 m³")
    await expect(rows.first).to_contain_text("Water meter")
    start = today - timedelta(days=24)
    through = today - timedelta(days=17)
    await expect(rows.first.locator("td").first).to_have_text(
        f"{start:%b} {start.day}, {start:%Y} – {through:%b} {through.day}, {through:%Y}"
    )
    await page.screenshot(path=str(tmp_path / "usage-desktop.png"))
    await page.emulate_media(color_scheme="dark")
    await page.screenshot(path=str(tmp_path / "usage-dark.png"))
    await page.emulate_media(color_scheme="light")
    await rows.first.get_by_role("button").click()
    dialog = page.get_by_role("dialog", name="Bill details")
    await expect(dialog.get_by_text("0 CCF", exact=True)).to_be_visible()
    await expect(
        dialog.get_by_role("table", name="Water meter readings")
    ).to_be_visible()
    await expect(
        dialog.get_by_role("link", name="Download invoice", exact=True)
    ).to_have_attribute("href", f"v1/documents/{digest}")
    await page.keyboard.press("Escape")
    intervals = page.get_by_role("region", name="Meter intervals", exact=True)
    await intervals.get_by_label("Service", exact=True).select_option("demo-electric")
    await intervals.get_by_role("button", name="Apply filters", exact=True).click()
    interval_rows = intervals.get_by_role(
        "table", name="Usage matching the selected filters"
    ).locator("tbody tr")
    await expect(interval_rows).to_have_count(25)
    await expect(interval_rows.first.locator("td").last).to_have_text("Wh")
    await expect(rows).to_have_count(32)
    await intervals.get_by_label("From", exact=True).fill("2100-01-01")
    await intervals.get_by_label("To", exact=True).fill("2100-01-02")
    await intervals.get_by_role("button", name="Apply filters", exact=True).click()
    await expect(
        page.get_by_role("heading", name="No meter intervals reported")
    ).to_be_visible()
    await expect(billed.get_by_role("table").locator("tbody tr")).to_have_count(32)
    await page.set_viewport_size({"width": 390, "height": 844})
    await assert_horizontal_visibility(page, rows.first.locator("td.numeric"))
    await assert_horizontal_visibility(page, rows.first.get_by_role("button"))
    assert not await page.evaluate("document.documentElement.scrollWidth > innerWidth")
    await page.evaluate("window.scrollTo(0, 0)")
    await page.screenshot(path=str(tmp_path / "usage-mobile.png"))


async def test_sync_warnings_name_the_affected_provider_and_account(
    utility_page: Page, web_backend: WebBackend, tmp_path: Path
) -> None:
    await utility_page.clock.install()

    def seed_history() -> None:
        with sqlite3.connect(web_backend.database) as database:
            database.execute(
                "UPDATE sync_jobs SET state='complete' WHERE account_id=?",
                ("demo-wm",),
            )
            database.execute(
                "UPDATE coverage SET payload="
                "json_set(payload,'$.state','complete','$.reason',NULL) "
                "WHERE account_id=? AND json_extract(payload,'$.state')!='unsupported'",
                ("demo-wm",),
            )
            database.execute(
                "DELETE FROM sync_jobs WHERE account_id=?", ("demo-energy",)
            )

    await asyncio.to_thread(seed_history)
    page = utility_page
    await open_app(page, web_backend)
    await expect(page.locator("#freshness-badge")).to_contain_text(
        "Bayou · Energy service"
    )
    freshness = page.get_by_role("region", name="Account freshness", exact=True)
    await expect(freshness).to_contain_text("Bayou · Energy service")
    await expect(freshness).to_contain_text("No successful sync yet.")
    await expect(freshness).not_to_contain_text("Waste service")
    await expect(page.locator("#freshness-badge")).not_to_contain_text("Waste service")
    await expect(
        page.get_by_role("contentinfo").get_by_role("status")
    ).not_to_contain_text("Bayou · Energy service")
    await page.screenshot(path=str(tmp_path / "overview-desktop.png"))
    await page.set_viewport_size({"width": 390, "height": 844})
    await page.screenshot(path=str(tmp_path / "overview-mobile.png"))
    await page.set_viewport_size({"width": 1280, "height": 720})
    async with ClientSession(
        headers={"Authorization": f"Bearer {web_backend.token}"}
    ) as client:
        async with client.post(
            f"{web_backend.origin}/v1/sync",
            json={"account": "demo-energy", "from": "2025-01-01", "to": "2025-01-02"},
        ) as response:
            response.raise_for_status()
            job = await response.json()
        async with asyncio.timeout(10):
            while True:
                async with client.get(
                    f"{web_backend.origin}/v1/sync/{job['id']}"
                ) as response:
                    response.raise_for_status()
                    current = await response.json()
                if current["state"] != "running":
                    assert current["state"] == "failed"
                    break
                await asyncio.sleep(0.05)

    await page.get_by_role("button", name="Refresh local utility data").click()
    await expect(freshness).to_contain_text("Sync failed.")
    await expect(freshness).to_contain_text("Bayou · Energy service")
    await expect(freshness).not_to_contain_text("Waste service")
    await expect(page.locator("#sync-status")).to_have_count(0)
    await page.get_by_role("button", name="Settings", exact=True).click()
    await expect(
        page.locator("#sync-status p").filter(has_text="Sync failed")
    ).to_contain_text("Bayou · Energy service")
    await page.get_by_label("Account to sync", exact=True).select_option("demo-energy")
    await expect(page.locator("#sync-availability")).to_have_text(
        "Bayou · Energy service · Provider credentials are not configured."
    )

    async def fail_status(route: Route) -> None:
        await route.abort()

    await page.route("**/v1/sync", fail_status)
    await page.clock.run_for(15_001)
    await expect(page.locator("#sync-status")).to_contain_text(
        "Bayou · Energy service · Sync status could not be checked. "
        "The last saved state was failed."
    )
    await expect(page.locator("#sync-status")).to_contain_text(
        "Waste Management · Waste service · Sync status could not be checked. "
        "The last saved state was complete."
    )
    await page.set_viewport_size({"width": 390, "height": 844})
    assert not await page.evaluate("document.documentElement.scrollWidth > innerWidth")


async def test_collections_and_usage_filters_keep_units_and_report_exceptions(
    utility_page: Page, web_backend: WebBackend
) -> None:
    page = utility_page
    await open_app(page, web_backend)
    await page.get_by_role("button", name="Trash collections", exact=True).click()
    await expect(page.locator(".data-table tbody tr").first).to_be_visible()
    await page.get_by_label("Service", exact=True).select_option("demo-waste")
    await page.get_by_label("From", exact=True).fill(
        (datetime.now(UTC) - timedelta(days=30)).date().isoformat()
    )
    await page.get_by_role("button", name="Apply filters", exact=True).click()
    await expect(
        page.locator(".data-table tbody .badge").filter(
            has_text=re.compile("^Exception$")
        )
    ).to_be_visible()
    await expect(
        page.locator(".exception-text").filter(has_text="None reported")
    ).not_to_have_count(await page.locator(".exception-text").count())
    await page.get_by_role("button", name="Usage", exact=True).click()
    await page.get_by_label("Service", exact=True).select_option("demo-electric")
    await page.get_by_role("button", name="Apply filters", exact=True).click()
    await expect(page.locator(".data-table tbody tr")).to_have_count(25)
    await expect(
        page.locator(".data-table tbody tr").first.locator("td").last
    ).to_have_text("Wh")
    for _ in range(4):
        more = page.get_by_role("button", name="Load more", exact=True)
        if await more.count() == 0:
            break
        previous = await page.locator(".data-table tbody tr").count()
        await more.click()
        await expect(page.locator(".data-table tbody tr")).not_to_have_count(previous)
    await expect(
        page.get_by_role("button", name="Load more", exact=True)
    ).to_be_hidden()
    await expect(page.locator(".coverage-panel")).to_have_count(0)
    await expect(page.locator(".list-meta [role=status]")).to_be_focused()


async def test_failed_refresh_preserves_saved_results_and_recovers(
    utility_page: Page, web_backend: WebBackend
) -> None:
    page = utility_page
    await open_app(page, web_backend)
    metrics = await page.locator(".metrics").inner_text()

    async def fail_summary(route: Route) -> None:
        await route.abort()

    await page.route("**/v1/summary", fail_summary)
    await page.get_by_role("button", name="Refresh local utility data").click()
    await expect(page.get_by_role("alert")).to_contain_text("could not load")
    await expect(page.locator("#freshness-badge")).to_have_text("Connection lost")
    await expect(page.get_by_role("contentinfo").get_by_role("status")).to_have_text(
        "App connection lost · showing saved data"
    )
    await expect(page.locator(".metrics")).to_have_text(metrics, use_inner_text=True)
    await page.unroute("**/v1/summary", fail_summary)
    await page.get_by_role("button", name="Try again", exact=True).click()
    await expect(page.locator("#notice")).to_have_text("")
    await expect(page.get_by_role("alert")).to_have_text("")
    await expect(
        page.get_by_role("contentinfo").get_by_role("status")
    ).not_to_contain_text("connection lost")
    await expect(page.get_by_role("region", name="Saved utility data")).to_be_visible()


async def test_unconfigured_sync_requires_an_explicit_account_without_sending_requests(
    utility_page: Page, web_backend: WebBackend
) -> None:
    page = utility_page
    requests: list[Request] = []
    page.on("request", lambda request: requests.append(request))
    await open_app(page, web_backend)
    await page.get_by_role("button", name="Settings", exact=True).click()
    await expect(page.get_by_role("heading", name="Sync provider data")).to_be_visible()
    await expect(page.get_by_label("Account to sync")).to_have_value("")
    await expect(
        page.get_by_role("button", name="Sync account", exact=True)
    ).to_be_disabled()
    await expect(
        page.get_by_text("Choose an account to sync.", exact=True)
    ).to_be_visible()
    await page.get_by_label("Account to sync").select_option("demo-wm")
    await expect(
        page.get_by_role("button", name="Sync account", exact=True)
    ).to_be_disabled()
    await expect(
        page.get_by_text(
            "Waste Management · Waste service · "
            "Provider credentials are not configured.",
            exact=True,
        )
    ).to_be_visible()
    assert not [request for request in requests if request.method != "GET"]
    diagnostics = page.locator("details.diagnostics")
    await expect(diagnostics).to_have_js_property("open", False)
    await expect(page.get_by_role("heading", name="Data coverage")).to_be_hidden()
    await expect(page.get_by_text("Not configured", exact=True).first).to_be_hidden()
    await diagnostics.locator("summary").click()
    await expect(page.get_by_text("Not configured", exact=True).first).to_be_visible()
    await expect(page.get_by_role("heading", name="Data coverage")).to_be_visible()
    await expect(
        page.get_by_role("button", name=re.compile("book|pay|cancel service", re.I))
    ).to_have_count(0)


@pytest.mark.parametrize(
    ("width", "theme"), [(320, "dark"), (390, "light"), (1200, "light")]
)
async def test_mobile_dark_keyboard_and_empty_state_layout(
    web_backend: WebBackend,
    tmp_path: Path,
    width: int,
    theme: Literal["light", "dark"],
) -> None:
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            context = await browser.new_context(
                viewport={"width": width, "height": 900}, color_scheme=theme
            )
            await context.add_cookies(
                [
                    {
                        "name": "utilities_session",
                        "value": web_backend.token,
                        "url": web_backend.origin,
                    }
                ]
            )
            page = await context.new_page()
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            await open_app(page, web_backend)
            for name in ["Overview", "Bills", "Trash collections", "Usage", "Settings"]:
                await page.get_by_role("button", name=name, exact=True).click()
                await expect(
                    page.get_by_role("heading", name=name, exact=True)
                ).to_be_visible()
                await expect(page.locator("#content")).to_have_attribute(
                    "aria-busy", "false"
                )
                assert not await page.evaluate(
                    "document.documentElement.scrollWidth > innerWidth"
                ), f"{name} overflows the viewport"
                for control in await page.locator(
                    "button:visible, input:visible, select:visible"
                ).all():
                    box = await control.bounding_box()
                    assert (
                        box is not None and box["width"] >= 44 and box["height"] >= 44
                    )
                if name == "Bills":
                    if width <= 390:
                        row = page.locator(".bills-table tbody tr").first
                        for index in [1, 2, 3]:
                            await assert_horizontal_visibility(
                                page, row.locator("td").nth(index)
                            )
                    await (
                        page.locator(".data-table tbody tr")
                        .first.get_by_role("button")
                        .click()
                    )
                    await expect(page.get_by_role("dialog")).to_be_visible()
                    assert not await page.evaluate(
                        "document.documentElement.scrollWidth > innerWidth"
                    )
                    await page.keyboard.press("Escape")
            await expect(page.locator(".wordmark")).to_have_accessible_name(
                "Utilities home"
            )
            await expect(
                page.locator(".app-footer").get_by_role("link")
            ).to_have_attribute("rel", "noopener noreferrer")
            await page.screenshot(
                path=str(tmp_path / f"utilities-{width}-{theme}.png"), full_page=True
            )
            assert not errors
        finally:
            await browser.close()
