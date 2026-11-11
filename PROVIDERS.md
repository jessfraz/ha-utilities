# Provider contracts

Credentials, eligibility, pricing, and real-account refresh behavior have not
been established. Adapters are implemented and HTTP-tested against fictional
responses from the official schemas, not certified through live accounts.

## Waste Management

[Access](https://api.wm.com/authentication/index.html) requires WM-issued JWT
and ClientId files. Configure `[wm]` with `token_file` and `client_id_file`.
`customer_id` and `provider_account` must identify the same explicit WM account.

The adapter reads [invoices](https://api.wm.com/InvoiceS/index.html) under `/v2`
and [balances](https://api.wm.com/balance/index.html),
[services](https://api.wm.com/Services/index.html),
[history](https://api.wm.com/servicestat/index.html),
[today's plan](https://api.wm.com/serviceplan/index.html),
[next pickup](https://api.wm.com/servicenextpickup/index.html), and
[ETA](https://api.wm.com/ETA/index.html) under `/v1`. Page headings sometimes
add an inconsistent `/api`; the adapter follows their sample request URLs.

Collection coverage is partial: bounded history, today's plan, and next pickup
do not establish a complete arbitrary future calendar. A name-only next-pickup
record must uniquely match the validated service catalog. Invoice-associated
payments are not a complete payment ledger or proof of settlement. Currency
must match the explicit account binding. No service request is submitted.
All-history sync omits WM's invoice date filters. Collection history still
uses the bounded lookback and the current/next service endpoints.

## Bayou

[Authentication](https://docs.bayou.energy/docs/quickstart) uses the API key as
the Basic-auth username with an empty password. Configure `[bayou]` with
`api_key_file`. An API key gives access to the Bayou workspace; the utility
still needs its own hosted sign-in and explicit account and meter selection.

Hosted setup follows Bayou's [customer lifecycle](https://docs.bayou.energy/docs/customer-lifecycle).
The supported utility catalogue comes from `GET /utilities`. After consent,
`POST /customers` sends only a random external ID and the chosen utility ID.
The saved intent precedes that request. Status reads use the same external ID
to reconcile timeouts and restarts before another create is considered.
Confirmed customer identity cannot silently change or be recreated.

Bayou's [workspace settings](https://docs.bayou.energy/docs/controlling-unlocked-data)
default to the last 12 billing periods, future bills, and electric and gas meters.
The actual workspace settings govern. Its documented
[free tier](https://docs.bayou.energy/docs/pricing-overview) allows 10 meters.
Unlocking an 11th requires billing setup, then all unlocked data becomes
billable, including the first 10, at $2 per meter per billing period.
Local account and meter selection happens afterward and does not limit those
workspace charges.
The UI and CLI require explicit acceptance before creating a customer.
Utilities never calls an unlock or payment endpoint.

The private installation polls documented customer status, so no public
callback or port forwarding is needed. Hosted links are fetched on request,
restricted to Bayou's documented HTTPS sign-up URL, and never saved to SQLite.
The database keeps opaque request IDs, verified customer/account/meter
bindings, setup status, and timestamps. Back it up with the app's other data.

The adapter reads [customer identity](https://docs.bayou.energy/reference/get_customers-id),
[bills](https://docs.bayou.energy/reference/get_customers-customer-id-bills), and
[intervals](https://docs.bayou.energy/reference/get_customers-customer-id-intervals).
Every continuation revalidates the exact customer, account number, and meter
bindings. Date windows bound the documented unpaged arrays without inventing
an offset API. Overlapping fetch windows preserve daily intervals that cross
UTC midnight; local filtering selects interval start dates.
All-history sync reads undated bills and uses Bayou's discovered interval
bounds to read usage in 28-day windows. It reads already available data;
locked records remain a coverage limitation.

Amounts are integer cents in the configured currency. `outstanding_balance`
means the previous billing cycle's balance, not current account balance.
`unlocked` is data-access status, never payment status. Signed document URLs
are not stored in the normalized read model. Remote account and meter IDs are
stored only in explicit connection bindings, never diagnostics.
Bill totals cover the whole selected provider account, including its unselected
meters. Each bill's meters are verified against that provider account. Usage
intervals are fetched only for meters explicitly selected for this deployment.
Locked bills, source-auth failures, missing intervals, gaps, and truncation
remain explicit coverage. Hosted sign-in can request utility authentication
or MFA on Bayou's page. Those credentials never pass through Utilities.
