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

## Bayou

[Authentication](https://docs.bayou.energy/docs/quickstart) uses the API key as
the Basic-auth username with an empty password. Configure `[bayou]` with
`api_key_file`. Only already-linked customers can be read.

The adapter reads [customer identity](https://docs.bayou.energy/reference/get_customers-id),
[bills](https://docs.bayou.energy/reference/get_customers-customer-id-bills), and
[intervals](https://docs.bayou.energy/reference/get_customers-customer-id-intervals).
Every continuation revalidates the exact customer, account number, and meter
bindings. Date windows bound the documented unpaged arrays without inventing
an offset API. Overlapping fetch windows preserve daily intervals that cross
UTC midnight; local filtering selects interval start dates.

Amounts are integer cents in the configured currency. `outstanding_balance`
means the previous billing cycle's balance, not current account balance.
`unlocked` is data-access status, never payment status. Signed document URLs
and remote account identifiers are not stored in the normalized read model.
Locked bills, source-auth failures, missing intervals, gaps, and truncation
remain explicit coverage. Unlocking, refreshing, linking, MFA changes, and
other provider writes are unavailable.
