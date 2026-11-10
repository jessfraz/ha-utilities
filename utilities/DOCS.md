# Configuration

`config_file` selects a private runtime TOML file. The repository includes a
fictional format example, not a household configuration. Leave the runtime file
absent to start without linked providers; the dashboard reports unconfigured
coverage. Add explicit account/service/deployment/currency bindings and
1Password-managed credential-file paths before requesting synchronization.

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
