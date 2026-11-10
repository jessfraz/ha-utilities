"""Regressions demonstrated by the independent adversarial UI review."""

import re

from aiohttp import ClientSession, web
from playwright.async_api import Page, async_playwright, expect
from pytest_aiohttp.plugin import AiohttpServer

from tests_web.conftest import WebBackend
from tests_web.test_app import open_app


async def test_skip_link_preserves_the_active_view_and_focuses_main(
    utility_page: Page, web_backend: WebBackend
) -> None:
    page = utility_page
    await open_app(page, web_backend)
    await page.get_by_role("button", name="Bills", exact=True).click()
    await expect(page.locator(".data-table tbody tr")).to_have_count(25)
    await page.get_by_role("link", name="Skip to content", exact=True).focus()
    await page.keyboard.press("Enter")
    await expect(page).to_have_url(re.compile(r"/#bills$"))
    await expect(page.get_by_role("heading", name="Bills", exact=True)).to_be_visible()
    await expect(page.get_by_role("main")).to_be_focused()


async def test_default_collections_include_recent_exceptions(
    utility_page: Page, web_backend: WebBackend
) -> None:
    page = utility_page
    await open_app(page, web_backend)
    await page.get_by_role("button", name="Trash collections", exact=True).click()
    await expect(
        page.locator(".data-table tbody .badge").filter(
            has_text=re.compile("^Exception$")
        )
    ).to_be_visible()
    await expect(
        page.get_by_text("Container not accessible", exact=True)
    ).to_be_visible()


async def test_sync_is_disabled_with_a_reason_for_each_unconfigured_provider(
    utility_page: Page, web_backend: WebBackend
) -> None:
    page = utility_page
    await open_app(page, web_backend)
    await page.get_by_role("button", name="Settings", exact=True).click()
    submit = page.get_by_role("button", name="Sync account", exact=True)
    await page.get_by_label("Account to sync", exact=True).select_option("demo-wm")
    await expect(submit).to_be_disabled()
    await expect(
        page.get_by_text("Waste Management credentials are not configured.", exact=True)
    ).to_be_visible()
    await page.get_by_label("Account to sync", exact=True).select_option("demo-energy")
    await expect(submit).to_be_disabled()
    await expect(
        page.get_by_text("Bayou credentials are not configured.", exact=True)
    ).to_be_visible()


async def test_expired_native_session_preserves_saved_bills_and_returns_to_the_view(
    utility_page: Page, web_backend: WebBackend
) -> None:
    page = utility_page
    await open_app(page, web_backend)
    await page.get_by_role("button", name="Bills", exact=True).click()
    await expect(page.locator(".data-table tbody tr")).to_have_count(25)
    await page.context.clear_cookies()
    await page.get_by_role("button", name="Refresh local utility data").click()
    await expect(page.locator("#freshness-badge")).to_have_text("Session expired")
    await expect(page.locator(".data-table tbody tr")).to_have_count(25)
    link = page.get_by_role("link", name="Sign in again", exact=True)
    await expect(link).to_have_attribute("href", "login#bills")
    await expect(
        page.get_by_role("button", name="Try again", exact=True)
    ).to_be_hidden()
    await link.click()
    await expect(page).to_have_url(re.compile(r"/login#bills$"))
    await page.get_by_label("API token", exact=True).fill(web_backend.token)
    await page.get_by_role("button", name="Sign in", exact=True).click()
    await expect(page.get_by_role("heading", name="Bills", exact=True)).to_be_visible()
    await expect(page.locator(".data-table tbody tr")).to_have_count(25)
    await expect(
        page.get_by_role("link", name="Sign in again", exact=True)
    ).to_be_hidden()


async def test_expired_ingress_session_points_back_to_home_assistant_not_native_login(
    web_backend: WebBackend, aiohttp_server: AiohttpServer
) -> None:
    authenticated = True
    async with ClientSession() as client:

        async def proxy(request: web.Request) -> web.Response:
            headers = {
                key: value
                for key, value in request.headers.items()
                if key.lower() not in {"host", "content-length", "x-remote-user-id"}
            }
            if authenticated:
                headers["X-Remote-User-Id"] = "fictional-review-user"
            async with client.request(
                request.method,
                f"{web_backend.ingress_origin}/{request.match_info['path']}",
                params=request.query,
                headers=headers,
                data=await request.read(),
                allow_redirects=False,
            ) as response:
                return web.Response(
                    status=response.status,
                    body=await response.read(),
                    headers={
                        key: value
                        for key, value in response.headers.items()
                        if key.lower() not in {"transfer-encoding", "content-length"}
                    },
                )

        app = web.Application()
        app.router.add_route("*", "/example-ingress/{path:.*}", proxy)
        server = await aiohttp_server(app)
        async with async_playwright() as playwright:
            browser = await playwright.chromium.launch()
            try:
                page = await browser.new_page()
                await page.goto(str(server.make_url("/example-ingress/")))
                await expect(
                    page.get_by_role("heading", name="Data coverage")
                ).to_be_visible()
                authenticated = False
                await page.get_by_role(
                    "button", name="Refresh local utility data"
                ).click()
                await expect(page.locator("#freshness-badge")).to_have_text(
                    "Home Assistant session expired"
                )
                await expect(page.get_by_role("alert")).to_contain_text(
                    "Reopen Utilities from Home Assistant"
                )
                await expect(
                    page.get_by_role("link", name="Sign in again", exact=True)
                ).to_be_hidden()
            finally:
                await browser.close()


async def test_browser_back_closes_the_bill_dialog_on_a_real_view_change(
    utility_page: Page, web_backend: WebBackend
) -> None:
    page = utility_page
    await open_app(page, web_backend)
    await page.get_by_role("button", name="Bills", exact=True).click()
    await expect(page.locator(".data-table tbody tr")).to_have_count(25)
    await page.locator(".data-table tbody tr").first.get_by_role("button").click()
    await expect(page.get_by_role("dialog", name="Bill details")).to_be_visible()
    await page.go_back()
    await expect(page.get_by_role("dialog", name="Bill details")).to_be_hidden()
    await expect(
        page.get_by_role("heading", name="Overview", exact=True)
    ).to_be_visible()


async def test_overview_and_bill_table_explain_missing_balances_and_provider_status(
    utility_page: Page, web_backend: WebBackend
) -> None:
    page = utility_page
    await open_app(page, web_backend)
    await expect(
        page.get_by_text(
            "A missing account balance is not confirmation that nothing is owed.",
            exact=True,
        )
    ).to_be_visible()
    await page.get_by_role("button", name="Bills", exact=True).click()
    await page.get_by_label("Account", exact=True).select_option("demo-energy")
    await page.get_by_role("button", name="Apply filters", exact=True).click()
    await expect(page.locator(".data-table tbody tr")).to_have_count(25)
    await expect(
        page.get_by_text("Data access status, not payment", exact=True).first
    ).to_be_visible()
