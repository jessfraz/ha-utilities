"""Own the real server and fictional seed for each browser test."""

import asyncio
import os
import shutil
import signal
import socket
from collections.abc import AsyncIterator
from dataclasses import dataclass
from pathlib import Path

import pytest
from aiohttp import ClientError, ClientSession
from playwright.async_api import Page, async_playwright

ROOT = Path(__file__).resolve().parents[1]
TOKEN = "fictional-utilities-browser-token-not-a-real-credential"


@dataclass(frozen=True)
class WebBackend:
    """A live native service with temporary, fictional observations."""

    origin: str
    database: Path
    config: Path
    ingress_origin: str
    token: str = TOKEN


@pytest.fixture
async def web_backend(
    tmp_path: Path, socket_enabled: None, request: pytest.FixtureRequest
) -> AsyncIterator[WebBackend]:
    """Build first with cargo build --bins --examples, then run the real producer."""
    binary = Path(
        os.environ.get("UTILITIES_TEST_BINARY", ROOT / "target/debug/utilities")
    )
    seed = Path(
        os.environ.get("UTILITIES_TEST_SEED", ROOT / "target/debug/examples/seed")
    )
    assert await asyncio.to_thread(binary.is_file), (
        "Build the Utilities server before browser tests"
    )
    assert await asyncio.to_thread(seed.is_file), (
        "Build the fictional seed example before browser tests"
    )
    database = tmp_path / "utilities.sqlite3"
    config = tmp_path / "utilities.toml"
    await asyncio.to_thread(shutil.copyfile, ROOT / "config/example.toml", config)
    if getattr(request, "param", None) == "bayou_only":
        await asyncio.to_thread(
            config.write_text,
            """deployment = "demo"
[[accounts]]
id = "demo-energy"
name = "Energy service"
deployment = "demo"
provider = "bayou"
customer_id = "900000002"
provider_account = "DEMO-ACCOUNT"
currency = "USD"
[[accounts.services]]
id = "demo-electric"
name = "Electricity"
provider_service = "DEMO-METER"
kind = "electricity"
""",
        )
    if getattr(request, "param", None) == "vendor_prefix":
        original = await asyncio.to_thread(config.read_text)
        await asyncio.to_thread(
            config.write_text,
            original.replace('id = "demo-energy"', 'id = "import_provider:spectrum"'),
        )
    seed_process = await asyncio.create_subprocess_exec(
        str(seed),
        str(database),
        str(config),
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    seed_out, seed_error = await seed_process.communicate()
    assert seed_process.returncode == 0, (seed_out + seed_error).decode()[:2000]
    assert await asyncio.to_thread(config.is_file), (
        "The seed requires explicit fictional account bindings"
    )
    token_file = tmp_path / "token"
    await asyncio.to_thread(token_file.write_text, TOKEN)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        ingress_port = listener.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    with (tmp_path / "service.log").open("wb") as log:
        process = await asyncio.create_subprocess_exec(
            str(binary),
            "--config",
            str(config),
            "--database",
            str(database),
            "--token-file",
            str(token_file),
            "--bind",
            f"127.0.0.1:{port}",
            "--ingress-bind",
            f"127.0.0.1:{ingress_port}",
            "--ingress-peer",
            "127.0.0.1",
            stdout=log,
            stderr=asyncio.subprocess.STDOUT,
        )
        try:
            async with ClientSession() as client:
                for _ in range(100):
                    if process.returncode is not None:
                        raise AssertionError(
                            (tmp_path / "service.log").read_text()[:2000]
                        )
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
                    raise AssertionError("Utilities did not become ready")
            yield WebBackend(
                origin, database, config, f"http://127.0.0.1:{ingress_port}"
            )
        finally:
            if process.returncode is None:
                process.send_signal(signal.SIGTERM)
                try:
                    await asyncio.wait_for(process.wait(), timeout=10)
                except TimeoutError:
                    process.kill()
                    await process.wait()


@pytest.fixture
async def utility_page(web_backend: WebBackend) -> AsyncIterator[Page]:
    """Use the server's real authenticated browser session, never a token URL."""
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            context = await browser.new_context()
            await context.add_cookies(
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
            page = await context.new_page()
            yield page
        finally:
            await browser.close()
