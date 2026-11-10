"""The native transport never leaks credentials across origins or oversized bodies."""

import pytest
from aiohttp import ClientSession

from custom_components.utilities.client import (
    InvalidBackendURL,
    ResponseTooLarge,
    UtilitiesClient,
    normalize_backend_url,
)
from custom_components.utilities.const import MAX_RESPONSE_BYTES

from .conftest import TOKEN, Backend


@pytest.mark.parametrize(
    "url",
    [
        "ftp://example.invalid",
        "https://user:pass@example.invalid",
        "https://example.invalid/path",
        "https://example.invalid?token=bad",
        "https://example.invalid#bad",
        "https://example.invalid:0",
        "https://example.invalid\\@evil.invalid",
        "https://example.invalid\n",
    ],
)
def test_explicit_service_origin_rejects_embedded_credentials_and_paths(
    url: str,
) -> None:
    with pytest.raises(InvalidBackendURL):
        normalize_backend_url(url)


async def test_transport_refuses_redirect_and_over_budget_response(
    backend: Backend,
) -> None:
    async with ClientSession() as session:
        client = UtilitiesClient(session, backend.origin, TOKEN)
        backend.responses["/v1/system"] = (
            302,
            b"",
            {"Location": "https://example.invalid"},
        )
        response = await client.request("GET", "/v1/system")
        assert response.status == 302
        assert len(backend.requests) == 1
        assert "Location" not in response.headers
        backend.responses["/v1/system"] = (200, b"x" * (MAX_RESPONSE_BYTES + 1), {})
        with pytest.raises(ResponseTooLarge):
            await client.request("GET", "/v1/system")
        with pytest.raises(ValueError):
            await client.request("GET", "//example.invalid")
