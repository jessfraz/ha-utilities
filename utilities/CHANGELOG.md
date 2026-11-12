# 0.1.15

Finish initialization after a successful manual full-history sync, including
partial coverage. Refresh saved connection status when opening Settings, keep
Check status available, and keep the dialog header and actions visible.

# 0.1.14

Keep the collection calendar available when a provider explicitly reports no
collection support. Preserve errors for missing, stale or incomplete coverage.

# 0.1.13

Keep sync controls and progress in each live connection's Settings dialog.
Remove individual imported bills with origin and revision checks, durable
replay exclusions, and optional retirement of unused services. Preserve
surviving records and shared source documents.
Keep version Repairs actionable when the first data refresh fails, using the
shared HA App Kit 0.1.6 startup check.

# 0.1.12

Use canonical utility companies and typed service categories throughout the app.
Remove custom connection, account, and service names and the rename endpoint.
Keep account IDs, invoices, original documents, amounts, and import history intact.

# 0.1.11

Keep active sync progress visible on every page, including after closing setup
or reloading. Keep other accounts’ warnings and leave completed job details in
Settings.

# 0.1.10

Read Bayou interval windows in the UTC format its API accepts. Keep manual
imports separate from API accounts, group manual vendors, and show source and
service tags. Put coverage in collapsed Diagnostics, name stale accounts,
and keep bill totals, due dates, and quantities visible on mobile. Update the
shared Home Assistant repair flow and its retry messages.
Project reported water intervals and recover eligible legacy rows on replay.
Keep estimates and overlapping windows out of usage.

# 0.1.9

Show billed water use and propane deliveries in Usage, with billing periods
and original source documents. Show the import state for saved documents.

# 0.1.8

Allow the owner to remove an import-only account and its exclusive source
documents without changing provider-linked accounts.
Group imported billing accounts by utility provider in the dashboard.
Allow the owner to rename a linked utility's display label.

# 0.1.7

Read available history when an account connects, then recheck seven days once
a day. Show a Repair after 40 days without new provider data. Match Settings
and Marketplace icons to the sidebar, and read SCE intervals with Bayou's
meter ID format.

# 0.1.6

Import typed billing records with their source documents. View itemized
charges, rates, and linked payments on each bill, plus monthly costs,
billed water, and propane deliveries in the overview.

# 0.1.5

Open Bayou sign-in in one click and explain why Bayou saves the utility login.
Keep manual sync progress visible after reload. The editable dates default
to the last three calendar months.

# 0.1.4

Repair app and integration version mismatches from Home Assistant Repairs.
Open Bayou's hosted sign-in links and distinguish link problems from a rejected
key or an outage.

# 0.1.3

Connect utility accounts through Bayou's hosted login and retain connections
across restarts. Scope usage to the selected meters and keep account-wide bills
clearly labeled. Show Trash collections only for providers that support it.

Use the shared App Kit select styling and track its releases with Dependabot.

# 0.1.0

Native WM and Bayou reporting adapters, shared local read model, dashboard,
CLI, collection calendar, Energy statistics, and guarded read-only Assist tools.
