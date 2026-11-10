"""Hosted setup through the real UI, Rust worker, SQLite, and provider HTTP boundary."""

import asyncio
import json
import os
import re
from collections.abc import AsyncIterator, Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

import pytest
from aiohttp import ClientError, ClientSession, web
from playwright.async_api import Page, async_playwright, expect
from pytest_aiohttp.plugin import AiohttpServer

from tests_web.conftest import ROOT, TOKEN, WebBackend


@dataclass
class BayouFixture:
    """Only documented provider shapes, with entirely fictional identifiers."""

    external_id: str | None = None
    authenticated: bool = False
    posts: list[dict[str, object]] = field(default_factory=list)
    fail_lookup: bool = False
    lose_post_response: bool = False
    post_received: asyncio.Event = field(default_factory=asyncio.Event)

    async def handle(self, request: web.Request) -> web.Response:
        if request.path == "/api/v2/utilities":
            return web.json_response(
                {
                    "utilities": [
                        {
                            "id": "speculoos_power",
                            "name": "Speculoos Power",
                            "support": "full",
                        }
                    ]
                }
            )
        if request.method == "POST":
            value = await request.json()
            assert set(value) == {"external_id", "utility"}
            assert value["utility"] == "speculoos_power"
            assert value["external_id"].startswith("ha_utilities_")
            self.posts.append(value)
            self.post_received.set()
            if self.external_id is not None:
                return web.json_response({}, status=409)
            self.external_id = value["external_id"]
            return web.json_response({}, status=503 if self.lose_post_response else 201)
        if self.fail_lookup:
            return web.json_response({}, status=503)
        if request.path.endswith("/bills"):
            assert request.match_info["customer"] == "71001"
            return web.json_response([])
        if self.external_id is None:
            return web.json_response({}, status=404)
        assert request.match_info["customer"] in {
            "71001",
            f"external_id:{self.external_id}",
        }
        return web.json_response(
            {
                "id": 71001,
                "external_id": self.external_id,
                "utility": "speculoos_power",
                "has_filled_credentials": self.authenticated,
                "is_currently_authenticated": self.authenticated,
                "auth_status": "authenticated"
                if self.authenticated
                else "onboarding_credentials_needed",
                "bills_are_ready": False,
                "intervals_are_ready": False,
                "utility_has_intervals": False,
                "account_has_intervals": False,
                "account_numbers": [
                    {
                        "id": "fictional-account-1234",
                        "meters": [
                            {"id": "fictional-electric-5678", "type": "electric"},
                            {"id": "fictional-gas-9012", "type": "gas"},
                        ],
                    }
                ]
                if self.authenticated
                else [],
                "onboarding_link": "https://bayou.energy/companies/12345/sign_up?key=fictional-private-link",
                "onboarding_token": "fictional-token-never-store",
                "private_metadata": "fictional-private-extra",
            }
        )


@dataclass
class ConnectionBackend:
    backend: WebBackend
    provider: BayouFixture


@pytest.fixture
async def connection_backend(
    tmp_path: Path,
    socket_enabled: None,
    aiohttp_server: AiohttpServer,
    unused_tcp_port_factory: Callable[[], int],
) -> AsyncIterator[ConnectionBackend]:
    provider = BayouFixture()
    app = web.Application()
    app.router.add_get("/api/v2/utilities", provider.handle)
    app.router.add_post("/api/v2/customers", provider.handle)
    app.router.add_get("/api/v2/customers/{customer}", provider.handle)
    app.router.add_get("/api/v2/customers/{customer}/bills", provider.handle)
    server = await aiohttp_server(app)
    binary = Path(
        os.environ.get(
            "UTILITIES_CONNECTION_FIXTURE",
            ROOT / "target/debug/examples/connection_fixture",
        )
    )
    assert await asyncio.to_thread(binary.is_file), (
        "Build the native connection fixture example first"
    )
    database = tmp_path / "connections.sqlite3"
    key = tmp_path / "fictional-key"
    await asyncio.to_thread(key.write_text, "fictional-bayou-browser-key")
    port = unused_tcp_port_factory()
    origin = f"http://127.0.0.1:{port}"
    with (tmp_path / "connection-service.log").open("wb") as log:
        process = await asyncio.create_subprocess_exec(
            str(binary),
            str(database),
            str(key),
            f"127.0.0.1:{port}",
            str(server.make_url("/api/v2/")),
            stdout=log,
            stderr=log,
        )
        try:
            async with ClientSession() as client:
                for _ in range(150):
                    assert process.returncode is None
                    try:
                        async with client.get(
                            f"{origin}/v1/system",
                            headers={"Authorization": f"Bearer {TOKEN}"},
                        ) as response:
                            if response.status == 200:
                                break
                    except ClientError:
                        pass
                    await asyncio.sleep(0.05)
                else:
                    raise AssertionError("Native connection API did not start")
            yield ConnectionBackend(WebBackend(origin, database, key, origin), provider)
        finally:
            if process.returncode is None:
                process.terminate()
                try:
                    await asyncio.wait_for(process.wait(), 10)
                except TimeoutError:
                    process.kill()
                    await process.wait()


async def sign_in(page: Page, fixture: ConnectionBackend) -> None:
    await page.context.add_cookies(
        [
            {
                "name": "utilities_session",
                "value": TOKEN,
                "url": fixture.backend.origin,
                "httpOnly": True,
                "sameSite": "Strict",
            }
        ]
    )
    await page.goto(fixture.backend.origin)
    await expect(page.locator("#content")).to_have_attribute("aria-busy", "false")


async def start_connection(
    page: Page, provider: BayouFixture, screenshot: Path | None = None
) -> None:
    await page.get_by_role("button", name="Settings", exact=True).click()
    await expect(
        page.get_by_role("heading", name="Settings", exact=True)
    ).to_be_visible()
    await page.get_by_role("button", name="Connect utility", exact=True).click()
    dialog = page.get_by_role("dialog", name="Connect utility", exact=True)
    await dialog.get_by_label("Utility", exact=True).select_option("speculoos_power")
    await dialog.get_by_label("Display name (optional)").fill("Example electricity")
    if screenshot is not None:
        await page.screenshot(path=str(screenshot), full_page=True)
    consent = dialog.get_by_role("checkbox")
    await expect(consent).not_to_be_checked()
    submit = dialog.get_by_role("button", name="Continue to sign-in")
    await expect(submit).to_be_in_viewport(ratio=1)
    await expect(submit).to_be_disabled()
    assert provider.posts == []
    pricing = dialog.get_by_text(
        re.compile("then all unlocked data is billed, including the first 10")
    )
    await pricing.scroll_into_view_if_needed()
    await expect(pricing).to_be_visible()
    await consent.scroll_into_view_if_needed()
    await expect(submit).to_be_in_viewport(ratio=1)
    await expect(
        dialog.get_by_role("button", name="Close utility connection")
    ).to_be_in_viewport(ratio=1)
    if screenshot is not None:
        await page.screenshot(
            path=str(screenshot.with_stem(screenshot.stem + "-policy")), full_page=True
        )
    await consent.check()
    await dialog.get_by_role("button", name="Continue to sign-in").click()
    await expect(page.get_by_role("dialog").get_by_role("status")).to_have_text(
        "Sign-in needed", timeout=15000
    )


@pytest.mark.parametrize(
    ("width", "theme"), [(390, "light"), (390, "dark"), (1200, "light"), (1200, "dark")]
)
async def test_hosted_link_meter_selection_and_saved_connection(
    connection_backend: ConnectionBackend,
    tmp_path: Path,
    width: int,
    theme: Literal["light", "dark"],
) -> None:
    fixture = connection_backend
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": width, "height": 900}, color_scheme=theme
            )
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            await sign_in(page, fixture)
            await start_connection(
                page,
                fixture.provider,
                tmp_path / f"connections-{width}-{theme}-setup.png",
            )
            dialog = page.get_by_role("dialog")
            await page.screenshot(
                path=str(tmp_path / f"connections-{width}-{theme}-signin.png"),
                full_page=True,
            )
            assert len(fixture.provider.posts) == 1
            await dialog.get_by_role("button", name="Get secure sign-in link").click()
            link = dialog.get_by_role("link", name="Continue on Bayou")
            await expect(link).to_have_attribute("rel", "noopener noreferrer")
            await expect(link).to_have_attribute("referrerpolicy", "no-referrer")
            await expect(link).to_have_attribute(
                "href",
                re.compile(r"^https://bayou\.energy/companies/12345/sign_up\?key="),
            )
            await page.keyboard.press("Escape")
            await expect(dialog).to_be_hidden()
            await expect(
                page.get_by_role("button", name="Connect utility", exact=True)
            ).to_be_focused()
            await page.reload()
            await page.get_by_role("button", name="Continue setup", exact=True).click()
            await expect(dialog.get_by_role("status")).to_have_text("Sign-in needed")
            assert len(fixture.provider.posts) == 1
            fixture.provider.authenticated = True
            await dialog.get_by_role("button", name="Check status", exact=True).click()
            await expect(dialog.get_by_role("status")).to_have_text(
                "Choose accounts and meters"
            )
            await page.screenshot(
                path=str(tmp_path / f"connections-{width}-{theme}-meters.png"),
                full_page=True,
            )
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
            submit = dialog.get_by_role("button", name="Connect selected meters")
            await expect(submit).to_be_disabled()
            choice = dialog.get_by_role("checkbox", name="Electricity")
            await choice.focus()
            await page.keyboard.press("Space")
            await expect(choice).to_be_checked()
            await submit.click()
            await expect(dialog.get_by_role("status")).to_have_text("Connected")
            await expect(page.locator(".account-grid h3")).to_have_text(
                "Example electricity"
            )
            await expect(page.locator(".sync-form select option")).to_have_count(2)
            await page.screenshot(
                path=str(tmp_path / f"connections-{width}-{theme}-connected.png"),
                full_page=True,
            )
            await dialog.get_by_role("button", name="Go to sync settings").click()
            await expect(
                page.get_by_label("Account to sync").locator("option")
            ).to_have_count(2)
            await expect(
                page.get_by_role(
                    "button", name=re.compile(r"^(Trash )?[Cc]ollections$")
                )
            ).to_have_count(0)
            await page.reload()
            await expect(
                page.get_by_role("button", name="Details", exact=True)
            ).to_be_visible()
            await expect(
                page.get_by_label("Account to sync").locator("option")
            ).to_have_count(2)
            assert len(fixture.provider.posts) == 1
            raw = await asyncio.to_thread(fixture.backend.database.read_bytes)
            assert (
                b"fictional-private-link" not in raw
                and b"fictional-token-never-store" not in raw
                and b"fictional-private-extra" not in raw
            )
            assert not errors
        finally:
            await browser.close()


async def test_bayou_only_hides_trash_even_on_deep_link(
    connection_backend: ConnectionBackend,
) -> None:
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            requests: list[str] = []
            page.on("request", lambda request: requests.append(request.url))
            await sign_in(page, connection_backend)
            await page.goto(connection_backend.backend.origin + "/#collections")
            await expect(
                page.get_by_role("heading", name="Overview", exact=True)
            ).to_be_visible()
            await expect(
                page.get_by_role(
                    "button", name=re.compile(r"^(Trash )?[Cc]ollections$")
                )
            ).to_have_count(0)
            assert not any("/v1/collections" in url for url in requests)
        finally:
            await browser.close()


async def test_native_api_requires_consent_and_reuses_request_id(
    connection_backend: ConnectionBackend,
) -> None:
    fixture = connection_backend
    async with ClientSession(headers={"Authorization": f"Bearer {TOKEN}"}) as client:
        body = {
            "provider": "bayou",
            "request_id": "01900000-0000-7000-8000-000000000001",
            "utility_id": "speculoos_power",
            "name": None,
            "currency": "USD",
            "accept_provider_settings": False,
        }
        async with client.post(
            fixture.backend.origin + "/v1/connections", json=body
        ) as response:
            assert response.status == 400
        assert fixture.provider.posts == []
        body["accept_provider_settings"] = True
        records = []
        for _ in range(2):
            async with client.post(
                fixture.backend.origin + "/v1/connections", json=body
            ) as response:
                assert response.status == 202
                records.append(await response.json())
        assert records[0]["id"] == records[1]["id"]
        async with client.get(fixture.backend.origin + "/v1/connections") as response:
            assert response.headers["Cache-Control"] == "no-store"
            data = await response.text()
            assert (
                "onboarding_link" not in data
                and "private-link" not in data
                and "external_id" not in data
            )
            assert len(json.loads(data)["items"]) == 1

        token = fixture.backend.database.parent / "owner-token"
        await asyncio.to_thread(token.write_text, TOKEN)
        binary = Path(
            os.environ.get(
                "HA_UTILITIES_TEST_BINARY", ROOT / "target/debug/ha-utilities"
            )
        )
        for arguments in [
            ["utilities"],
            ["list"],
            ["get", records[0]["id"]],
            [
                "start",
                "--utility",
                "speculoos_power",
                "--accept-provider-settings",
                "--request-id",
                body["request_id"],
            ],
        ]:
            process = await asyncio.create_subprocess_exec(
                str(binary),
                "--url",
                fixture.backend.origin,
                "--token-file",
                str(token),
                "--json",
                "connections",
                *arguments,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await process.communicate()
            assert process.returncode == 0, stderr.decode()
            result = json.loads(stdout)
            if arguments[0] in {"get", "start"}:
                assert result["id"] == records[0]["id"]
            else:
                assert len(result["items"]) == 1


@pytest.mark.parametrize("uncertain", [False, True])
async def test_worker_reconciles_lost_responses_and_never_posts_when_lookup_fails(
    connection_backend: ConnectionBackend,
    uncertain: bool,
) -> None:
    fixture = connection_backend
    fixture.provider.lose_post_response = uncertain
    fixture.provider.fail_lookup = not uncertain
    async with ClientSession(headers={"Authorization": f"Bearer {TOKEN}"}) as client:
        body = {
            "provider": "bayou",
            "request_id": "01900000-0000-7000-8000-000000000002",
            "utility_id": "speculoos_power",
            "currency": "USD",
            "accept_provider_settings": True,
        }
        async with client.post(
            fixture.backend.origin + "/v1/connections", json=body
        ) as response:
            assert response.status == 202
            saved = await response.json()
        url = fixture.backend.origin + "/v1/connections/" + saved["id"]
        async with asyncio.timeout(5):
            while True:
                async with client.get(url) as response:
                    result = await response.json()
                if result["state"] not in {"queued", "creating"}:
                    break
                await asyncio.sleep(0.02)
        if uncertain:
            assert result["state"] == "awaiting_authorization"
            assert result["customer_id"] == "71001"
            assert len(fixture.provider.posts) == 1

        else:
            assert result["state"] == "unconfirmed"
            assert fixture.provider.posts == []
            async with client.post(url + "/resume", json={}) as response:
                assert response.status == 503
            assert fixture.provider.posts == []
            fixture.provider.fail_lookup = False
            async with client.post(url + "/resume", json={}) as response:
                assert response.status == 200
            await asyncio.wait_for(fixture.provider.post_received.wait(), 5)
            assert len(fixture.provider.posts) == 1


async def test_hosted_sync_failure_is_terminal_and_next_sync_can_succeed(
    connection_backend: ConnectionBackend,
) -> None:
    fixture = connection_backend
    fixture.provider.authenticated = True
    async with ClientSession(headers={"Authorization": f"Bearer {TOKEN}"}) as client:

        async def call(
            method: str, path: str, body: dict[str, object] | None = None
        ) -> dict[str, object]:
            async with client.request(
                method, fixture.backend.origin + path, json=body
            ) as response:
                assert response.status in (200, 202)
                return await response.json()

        created = await call(
            "POST",
            "/v1/connections",
            {
                "provider": "bayou",
                "request_id": "01900000-0000-7000-8000-000000000003",
                "utility_id": "speculoos_power",
                "currency": "USD",
                "accept_provider_settings": True,
            },
        )
        for _ in range(100):
            saved = await call("GET", f"/v1/connections/{created['id']}")
            if saved["state"] == "ready":
                break
            await asyncio.sleep(0.02)
        assert saved["state"] == "ready"
        await call(
            "POST",
            f"/v1/connections/{created['id']}/bind",
            {
                "accounts": [
                    {
                        "account": "fictional-account-1234",
                        "meters": ["fictional-electric-5678"],
                    }
                ]
            },
        )
        accounts = await call("GET", "/v1/accounts")
        rows = accounts["items"]
        assert isinstance(rows, list) and rows
        row = rows[0]
        assert isinstance(row, dict) and isinstance(row["id"], str)
        account = row["id"]
        fixture.provider.fail_lookup = True

        async def sync() -> dict[str, object]:
            job = await call(
                "POST",
                "/v1/sync",
                {"account": account, "from": "2026-10-01", "to": "2026-10-01"},
            )
            for _ in range(100):
                result = await call("GET", f"/v1/sync/{job['id']}")
                if result["state"] != "running":
                    return result
                await asyncio.sleep(0.02)
            raise AssertionError(
                f"A provider failure left the accepted sync running: {result}"
            )

        failed = await sync()
        assert failed["state"] == "failed" and failed["error"] == "provider_unavailable"
        assert failed["finished_at"] is not None
        fixture.provider.fail_lookup = False
        next_job = await sync()
        assert next_job["id"] != failed["id"]
        assert next_job["state"] == "partial" and next_job["error"] is None
        assert next_job["observations"] == 1
