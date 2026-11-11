"""Exercise real read-model views, paging, filters, and recoverable failures."""

import re
from datetime import UTC, datetime, timedelta
from hashlib import sha256
from pathlib import Path
from typing import Literal
from urllib.parse import parse_qs, urlsplit

import pytest
from aiohttp import ClientSession, web
from playwright.async_api import Page, Request, Route, async_playwright, expect
from pytest_aiohttp.plugin import AiohttpServer

from tests_web.conftest import WebBackend


async def open_app(page: Page, backend: WebBackend) -> None:
    """Wait for the visible application, not an implementation-only readiness flag."""
    await page.goto(backend.origin)
    await expect(
        page.get_by_role("heading", name="Overview", exact=True)
    ).to_be_visible()
    await expect(page.get_by_role("heading", name="Data coverage")).to_be_visible()
    await expect(page.locator("#content")).to_have_attribute("aria-busy", "false")


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
            await expect(
                page.get_by_role("heading", name="Data coverage")
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
                await expect(
                    page.get_by_role("heading", name="Data coverage")
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
    utility_page: Page, web_backend: WebBackend
) -> None:
    """Pagination reads the real producer, and provider processing is not payment."""
    page = utility_page
    requests: list[Request] = []
    page.on("request", lambda request: requests.append(request))
    await open_app(page, web_backend)
    await page.get_by_role("button", name="Bills", exact=True).click()
    await expect(page.locator(".data-table tbody tr")).to_have_count(25)
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
    await expect(rows.first.locator("td").nth(4)).to_contain_text("unlocked")
    first_bill = rows.first.get_by_role("button")
    await first_bill.click()
    dialog = page.get_by_role("dialog", name="Bill details")
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
        page.get_by_text(re.compile("not confirmation.*complete coverage"))
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
    await open_app(page, web_backend)
    vendor = page.get_by_role("region", name="Imported vendors")
    await expect(vendor.get_by_text("Spectrum", exact=True)).to_have_count(1)
    await vendor.get_by_role("button", name="View Spectrum bills").click()
    filters = page.get_by_role("form", name="Filter bills")
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
    await expect(
        page.get_by_role("columnheader", name="Source status", exact=True)
    ).to_be_visible()
    await expect(rows.nth(1)).to_contain_text("Document imported")
    await page.get_by_role("button", name="Load more", exact=True).click()
    await expect(rows).to_have_count(27)
    await expect(rows.last).to_contain_text("Construction account")
    await rows.last.get_by_role("button").click()
    dialog = page.get_by_role("dialog", name="Bill details")
    await expect(
        dialog.get_by_role("heading", name="Spectrum", exact=True)
    ).to_be_visible()
    await expect(dialog.get_by_text("Construction account", exact=True)).to_be_visible()
    await expect(dialog.get_by_text("Aerial construction", exact=True)).to_be_visible()
    await expect(
        dialog.get_by_role("link", name="Invoice", exact=True)
    ).to_have_attribute("href", f"v1/documents/{digest}")
    await page.keyboard.press("Escape")
    await filters.get_by_label("Account or vendor").select_option(
        "import_provider:spectrum"
    )
    await filters.get_by_role("button", name="Apply filters").click()
    await expect(rows.last).to_contain_text("Energy service")
    await expect(rows.get_by_text("Spectrum", exact=True)).to_have_count(0)

    await page.get_by_role("button", name="Settings", exact=True).click()
    settings = page.get_by_role("region", name="Utility providers")
    await expect(
        settings.get_by_role("heading", name="Spectrum", exact=True)
    ).to_have_count(1)
    card = settings.get_by_role("article").filter(
        has=page.get_by_role("heading", name="Spectrum", exact=True)
    )
    await expect(card.get_by_text("Home internet", exact=True)).to_be_visible()
    await expect(card.get_by_text("Construction account", exact=True)).to_be_visible()
    await page.set_viewport_size({"width": 390, "height": 844})
    await expect(card.get_by_text("Construction account", exact=True)).to_be_visible()
    assert not await page.evaluate("document.documentElement.scrollWidth > innerWidth")
    await page.get_by_label("Account to sync", exact=True).select_option("demo-wm")
    await expect(
        page.get_by_role("button", name="Sync account", exact=True)
    ).to_be_disabled()


async def test_usage_includes_billed_water_periods_and_source_documents(
    utility_page: Page, web_backend: WebBackend
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
    await rows.first.get_by_role("button").click()
    dialog = page.get_by_role("dialog", name="Bill details")
    await expect(dialog.get_by_text("0 CCF", exact=True)).to_be_visible()
    await expect(
        dialog.get_by_role("table", name="Water meter readings")
    ).to_be_visible()
    await expect(
        dialog.get_by_role("link", name="Invoice", exact=True)
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
    await expect(page.get_by_role("heading", name="Data coverage")).to_be_visible()
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
    await expect(page.get_by_role("heading", name="Data coverage")).to_be_visible()


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
        page.get_by_text("Waste Management credentials are not configured.", exact=True)
    ).to_be_visible()
    assert not [request for request in requests if request.method != "GET"]
    await expect(page.get_by_text("Not configured", exact=True).first).to_be_visible()
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
                        scroll_region = page.get_by_role(
                            "region",
                            name="Bills matching the selected filters",
                            exact=True,
                        )
                        await scroll_region.focus()
                        await expect(scroll_region).to_be_focused()
                        await page.keyboard.press("ArrowRight")
                        await expect(scroll_region).not_to_have_js_property(
                            "scrollLeft", 0
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
