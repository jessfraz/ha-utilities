# Configuration

Use the app's native Configuration options. Set `deployment`, then add each
account with its explicit local ID, deployment, provider, customer ID,
provider account, currency, and service bindings. Providers are only
`waste_management` and `bayou`. Each account's deployment must match the global
deployment. No first-account or first-meter selection is performed.

WM requires both `wm_token` (without the Bearer prefix) and `wm_client_id`.
Bayou uses `bayou_api_key`. These are password fields; the app keeps derived
credential files owner-only and never returns their values through its API.
Configuration options alone suffice; no SSH, file editor, or backup modification
is needed. Empty defaults start without bound providers. Installing does not
enroll, link, unlock, or refresh a provider account.

Native installations may still use TOML plus credential-file paths. The optional
`config_file` option selects that mode explicitly and cannot be mixed with
nonempty inline bindings or credentials. The repository's example is fictional,
not a household configuration.

The app generates `/data/api-token` with private permissions. Supervisor
discovery transfers it to the integration through its authenticated channel;
the ingress dashboard never receives it. Keep the API port unexposed unless a
trusted client needs it. Native HTTP clients should use a trusted local network
or a TLS reverse proxy.

Home Assistant cold backups include the app's database and runtime files.
Take a native backup before an upgrade. Restore through Home Assistant, not
by replacing an active SQLite database. No HA configuration copies are tracked.

Energy import is append-only and bounded to seven days per refresh. It accepts
only complete, continuous hours in Wh or kWh; already imported historical
corrections are not rewritten in Recorder. The Usage view retains the current
provider observations.
