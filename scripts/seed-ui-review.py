"""Capture only the fictional seed UI for a read-only adversarial review."""

import argparse
import asyncio
from pathlib import Path
from typing import Literal
from urllib.parse import urlsplit

from aiohttp import ClientSession
from playwright.async_api import async_playwright, expect


async def capture(origin: str, token_file: Path, output: Path) -> None:
    """Keep review screenshots local; never enroll or contact a utility provider."""
    location = urlsplit(origin)
    if (
        location.scheme != "http"
        or location.hostname not in {"127.0.0.1", "localhost", "::1"}
        or location.username
        or location.password
        or location.query
        or location.fragment
    ):
        raise ValueError("Review capture requires a loopback fictional fixture server")
    token = (await asyncio.to_thread(token_file.read_text)).strip()
    async with ClientSession() as client:
        async with client.get(
            f"{origin.rstrip('/')}/v1/accounts",
            headers={"Authorization": f"Bearer {token}"},
        ) as response:
            if response.status != 200:
                raise ValueError("The fictional fixture server is unavailable")
            accounts = await response.json()
            if not isinstance(accounts, dict) or {
                item.get("id")
                for item in accounts.get("items", [])
                if isinstance(item, dict)
            } != {"demo-wm", "demo-energy"}:
                raise ValueError(
                    "Review capture is limited to the fictional seed accounts"
                )
    await asyncio.to_thread(output.mkdir, parents=True, exist_ok=True)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            scenarios: list[tuple[int, Literal["light", "dark"]]] = [
                (1200, "light"),
                (390, "dark"),
            ]
            for width, theme in scenarios:
                context = await browser.new_context(
                    viewport={"width": width, "height": 900}, color_scheme=theme
                )
                try:
                    await context.add_cookies(
                        [{"name": "utilities_session", "value": token, "url": origin}]
                    )
                    page = await context.new_page()
                    await page.goto(origin)
                    await expect(
                        page.get_by_role("heading", name="Data coverage")
                    ).to_be_visible()
                    for name in [
                        "Overview",
                        "Bills",
                        "Collections",
                        "Usage",
                        "Settings",
                    ]:
                        await page.get_by_role("button", name=name, exact=True).click()
                        await expect(
                            page.get_by_role("heading", name=name, exact=True)
                        ).to_be_visible()
                        await expect(page.locator("#content")).to_have_attribute(
                            "aria-busy", "false"
                        )
                        await page.screenshot(
                            path=str(output / f"{name.lower()}-{width}-{theme}.png"),
                            full_page=True,
                        )
                        if name == "Bills":
                            await page.get_by_label(
                                "Account", exact=True
                            ).select_option("demo-energy")
                            await page.get_by_role(
                                "button", name="Apply filters", exact=True
                            ).click()
                            await (
                                page.locator(".data-table tbody tr")
                                .first.get_by_role("button")
                                .click()
                            )
                            await expect(
                                page.get_by_text("Previous-cycle balance", exact=True)
                            ).to_be_visible()
                            await page.screenshot(
                                path=str(output / f"bill-detail-{width}-{theme}.png")
                            )
                            await page.keyboard.press("Escape")
                finally:
                    await context.close()
            context = await browser.new_context()
            try:
                page = await context.new_page()
                await page.goto(origin)
                await expect(
                    page.get_by_role("heading", name="Sign in to Utilities")
                ).to_be_visible()
                await page.screenshot(path=str(output / "native-sign-in.png"))
            finally:
                await context.close()
        finally:
            await browser.close()


def main() -> None:
    """The token is read from a file, not an argument or environment dump."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("origin")
    parser.add_argument("--token-file", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    asyncio.run(capture(args.origin, args.token_file, args.output_dir))


if __name__ == "__main__":
    main()
