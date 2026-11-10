"""Exercise real read-model views, paging, filters, and recoverable failures."""

import re
from datetime import UTC, datetime, timedelta
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
