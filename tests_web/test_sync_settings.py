"""Connection-scoped sync controls and fictional device captures."""

import asyncio
import json
from hashlib import sha256
from pathlib import Path

import pytest
from playwright.async_api import Page, async_playwright, expect

from tests_web.conftest import ROOT, TOKEN, WebBackend
from tests_web.test_app import open_app, open_connection_settings
from tests_web.test_connections import (
    ConnectionBackend,
    reopen_connection_settings,
    sign_in,
    start_connection,
)
from tests_web.test_connections import connection_backend as connection_backend

DEVICES = [
    ("iphone", 390, 844, True),
    ("ipad", 820, 1180, True),
    ("desktop", 1440, 1000, False),
]


async def capture(page: Page, file: Path) -> None:
    assert not await page.evaluate("document.documentElement.scrollWidth > innerWidth")
    await page.mouse.move(0, 0)
    await page.screenshot(
        path=str(file), full_page=await page.get_by_role("dialog").count() == 0
    )


async def verify_native_assets(page: Page, backend: WebBackend, file: Path) -> None:
    hashes = {}
    for name in ("app.js", "app.css", "connections.js"):
        response = await page.request.get(f"{backend.origin}/assets/{name}")
        assert response.status == 200
        data = await response.body()
        assert data == await asyncio.to_thread((ROOT / "web" / name).read_bytes)
        hashes[name] = sha256(data).hexdigest()
    await asyncio.to_thread(file.write_text, json.dumps(hashes, indent=2) + "\n")


@pytest.mark.parametrize("web_backend", ["account_cards"], indirect=True)
@pytest.mark.parametrize(("device", "width", "height", "touch"), DEVICES)
async def test_every_tab_and_unconfigured_connection_settings(
    web_backend: WebBackend,
    tmp_path: Path,
    device: str,
    width: int,
    height: int,
    touch: bool,
) -> None:
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": width, "height": height},
                is_mobile=touch,
                has_touch=touch,
                locale="en-US",
                timezone_id="UTC",
            )
            await page.context.add_cookies(
                [
                    {
                        "name": "utilities_session",
                        "value": web_backend.token,
                        "url": web_backend.origin,
                        "httpOnly": True,
                        "sameSite": "Strict",
                    }
                ]
            )
            await open_app(page, web_backend)
            await verify_native_assets(page, web_backend, tmp_path / "assets.json")
            for title, filename in (
                ("Overview", "overview"),
                ("Bills", "bills"),
                ("Trash collections", "trash"),
                ("Usage", "usage"),
                ("Settings", "settings"),
            ):
                await page.get_by_role("button", name=title, exact=True).click()
                await expect(page.locator("#content")).to_have_attribute(
                    "aria-busy", "false"
                )
                await capture(page, tmp_path / f"{device}-{filename}.png")
            await expect(page.locator("#content .sync-form")).to_have_count(0)
            await expect(page.get_by_label("Account to sync")).to_have_count(0)
            for company, filename in (
                ("Waste Management", "wm-settings"),
                ("Speculoos Power", "bayou-settings-disabled"),
            ):
                dialog = await open_connection_settings(page, company)
                await expect(
                    dialog.get_by_role("button", name="Sync account", exact=True)
                ).to_be_disabled()
                await expect(dialog.locator(".sync-availability")).to_have_text(
                    "Provider credentials are not configured."
                )
                await expect(
                    dialog.get_by_text("Updates daily.", exact=False)
                ).to_have_count(0)
                await expect(dialog.get_by_text("Connected", exact=True)).to_have_count(
                    0
                )
                await expect(dialog.locator(".sync-status")).to_contain_text(
                    "Add provider credentials in the app configuration, "
                    "then sync again."
                )
                await expect(
                    dialog.locator(".sync-status").get_by_text(
                        "Provider credentials are not configured.", exact=False
                    )
                ).to_have_count(0)
                await capture(page, tmp_path / f"{device}-{filename}.png")
                await dialog.get_by_role(
                    "button", name="Close utility connection"
                ).click()
        finally:
            await browser.close()


@pytest.mark.parametrize(("device", "width", "height", "touch"), DEVICES)
async def test_connection_setup_and_saved_sync_states(
    connection_backend: ConnectionBackend,
    tmp_path: Path,
    device: str,
    width: int,
    height: int,
    touch: bool,
) -> None:
    fixture = connection_backend
    fixture.provider.bills_ready = True
    gate = asyncio.Event()
    fixture.provider.bills_gate = gate
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": width, "height": height},
                is_mobile=touch,
                has_touch=touch,
                locale="en-US",
                timezone_id="UTC",
            )
            await sign_in(page, fixture)
            await verify_native_assets(page, fixture.backend, tmp_path / "assets.json")
            await start_connection(
                page, fixture.provider, tmp_path / f"{device}-connect.png"
            )
            dialog = page.get_by_role("dialog")
            await capture(page, tmp_path / f"{device}-sign-in.png")
            fixture.provider.authenticated = True
            await dialog.get_by_role("button", name="Check status", exact=True).click()
            await capture(page, tmp_path / f"{device}-choose-meters.png")
            await dialog.get_by_role("checkbox", name="Electricity").check()
            await dialog.get_by_role("button", name="Connect selected meters").click()
            status = dialog.locator(".sync-status")
            submit = dialog.get_by_role("button", name="Sync account", exact=True)
            await expect(status).to_contain_text("Syncing")
            await expect(submit).to_be_disabled()
            await expect(dialog.get_by_role("combobox")).to_have_count(0)
            await capture(page, tmp_path / f"{device}-automatic-sync.png")
            gate.set()
            await expect(status).to_contain_text("Coverage gaps")
            await expect(submit).to_be_enabled()
            await capture(page, tmp_path / f"{device}-sync-ready.png")
            gate.clear()
            await submit.click()
            await expect(status).to_contain_text("Syncing")
            await capture(page, tmp_path / f"{device}-manual-sync.png")
            gate.set()
            await expect(status).to_contain_text("Coverage gaps")
            await expect(submit).to_be_enabled()
            await page.context.clear_cookies()
            await submit.click()
            await expect(
                dialog.locator(".connection-sync [role='alert']")
            ).to_contain_text("This session has expired.")
            await expect(submit).to_be_disabled()
            await expect(
                dialog.get_by_role("button", name="Sync all available history")
            ).to_be_disabled()
            await expect(
                dialog.get_by_role("link", name="Sign in again", exact=True)
            ).to_be_visible()
            await expect(dialog.locator(".sync-status")).to_contain_text(
                "Bills include the previous-cycle balance, not a live account balance."
            )
            await capture(page, tmp_path / f"{device}-session-expired.png")
            await dialog.get_by_role("link", name="Sign in again", exact=True).click()
            await page.get_by_label("API token", exact=True).fill(TOKEN)
            await page.get_by_role("button", name="Sign in", exact=True).click()
            await reopen_connection_settings(page)
            await expect(
                page.get_by_role("dialog").get_by_role(
                    "button", name="Sync account", exact=True
                )
            ).to_be_enabled()
        finally:
            gate.set()
            await browser.close()
