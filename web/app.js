import { mountAppShell, setNotice } from './shell.js';
import { createConnections } from './connections.js';

const byId = (id) => document.getElementById(id);
const content = byId('content');
const notice = byId('notice');
const errorNotice = byId('error');
const signInAgain = byId('sign-in-again');
const isIngress = document.documentElement.dataset.ingress === 'true';
const retry = byId('retry');
const detail = byId('bill-detail');
const views = {
  overview: ['Overview', 'Bills and service data from your utilities.'],
  bills: ['Bills', 'Billing history, charges, and source documents.'],
  collections: ['Trash collections', 'Scheduled pickups, completion reports, and exceptions.'],
  usage: ['Usage', 'Billed quantities and meter intervals from your utilities.'],
  settings: ['Settings', 'Utility connections and sync.'],
};
const names = {
  waste_management: 'Waste Management', bayou: 'Bayou',
  lvmwd: 'LVMWD', superior_propane: 'Superior Propane', starlink: 'Starlink', spectrum: 'Spectrum',
  bills: 'Bills', balances: 'Balances', payments: 'Payments', services: 'Services',
  collections: 'Trash collections', usage: 'Usage',
  complete: 'Available', partial: 'Partial coverage', unavailable: 'Unavailable',
  unsupported: 'Not supported', not_configured: 'Not configured',
  scheduled: 'Scheduled', completed: 'Completed', exception: 'Exception', unknown: 'Unknown',
  running: 'Syncing', failed: 'Sync failed', interrupted: 'Sync interrupted',
  waste: 'Waste', recycling: 'Recycling', electricity: 'Electricity', gas: 'Gas', water: 'Water',
  internet: 'Internet', propane: 'Propane', other: 'Other',
};
const billingUnits = {
  hundred_cubic_feet: 'CCF', us_gallon: 'US gal', cubic_meter: 'm³', byte: 'B',
  gigabyte: 'GB', terabyte: 'TB', count: 'unit', cubic_yard: 'yd³', day: 'day', month: 'month',
};
const reportedQuantityNames = { billed_water: 'Billed water', propane_delivered: 'Propane delivered' };
const sourceNames = {
  invoice: 'Invoice', statement: 'Statement', supporting: 'Supporting document',
  payment_confirmation: 'Payment confirmation', delivery_ticket: 'Delivery ticket', usage_export: 'Usage export',
};
const reasonLabels = {
  not_configured: 'Provider credentials are not configured.',
  credentials_missing: 'Provider credentials are not configured.',
  credentials_not_configured: 'Provider credentials are not configured.',
  not_synced: 'This account has not been synced yet.',
  authentication: 'Provider authentication needs attention.',
  authentication_required: 'Provider authentication needs attention.',
  access_denied: 'Provider access has not been granted.',
  rate_limited: 'The provider has limited requests. Try syncing later.',
  unavailable: 'The provider could not be reached.',
  provider_unavailable: 'The provider could not be reached.',
  provider_preparing_history: 'The provider is still preparing your history. Try again later.',
  invalid_response: 'The provider response could not be verified.',
  binding_mismatch: 'Provider data did not match the configured account or service.',
  unsupported: 'This provider does not report this data through the supported API.',
  provider_not_supported: 'This provider does not report this data through the supported API.',
  no_documented_payment_history: 'The supported API does not document payment history. Payment or settlement cannot be established here.',
  not_a_collection_provider: 'This provider does not report waste collections.',
  previous_cycle_balance_only: 'Bills include the previous-cycle balance, not a live account balance.',
  invoice_associated_payments_only: 'Only payments attached to returned invoices are reported, not a complete payment ledger.',
  bounded_history_and_next_pickup: 'Reported history and the next pickup are available. A complete future collection calendar is not provided.',
  provider_reauthentication_required: 'Reauthentication is needed through the provider. Utilities cannot change authentication factors.',
  bills_not_ready_or_locked: 'Some bills are still processing or unavailable to the API. Utilities does not unlock bills.',
  utility_has_no_intervals: 'This utility does not provide interval data through Bayou.',
  account_has_no_intervals: 'No interval data is available for this account.',
  no_bound_services: 'No service is explicitly bound to this account.',
  interval_gaps_or_data_not_ready: 'Intervals have gaps or are still processing. They are not continuous energy statistics.',
  document_import: 'Additional history may be available from the utility.',
  more_pages: 'The sync has more provider data to read.',
  sync_in_progress: 'The sync is still reading provider data.',
  process_interrupted: 'The last sync was interrupted. Saved observations remain available.',
  fictional_demo: 'Fictional example data. No utility provider is connected.',
};

let view = readView();
let accounts = [];
let providers = [];
let summary = null;
let health = null;
let ready = false;
let generation = 0;
let viewController = null;
let detailController = null;
let syncTimer = null;
let syncPollController = null;
let syncJobs = [];
let updateSyncForm = null;
let loadController = null;
const listState = new Map();
const importProviderPrefix = 'import_provider/';
const refresh = node('button', 'Refresh');
refresh.type = 'button';
refresh.className = 'quiet';
refresh.setAttribute('aria-label', 'Refresh local utility data');
const shell = mountAppShell({
  name: 'Utilities', header: byId('header'), footer: byId('footer'),
  sourceUrl: 'https://github.com/jessfraz/ha-utilities',
  actions: [refresh], status: 'Connecting',
});
const connectionManager = createConnections({
  api, node, providers: () => providers, onError: showError,
  updated: load,
  connected: async () => { location.hash = 'settings'; view = 'settings'; await renderView(); byId('main').focus(); },
});

function node(tag, text, className) {
  const value = document.createElement(tag);
  if (text !== undefined) value.textContent = text;
  if (className) value.className = className;
  return value;
}

function label(value) { return names[value] ?? String(value).replaceAll('_', ' '); }
function accountName(id) { return accounts.find((account) => account.id === id)?.name ?? 'Unknown account'; }
function providerName(id) { return providers.find((provider) => provider.id === id)?.name ?? label(id); }
function accountLabel(id) {
  return accountDisplay(id).label;
}
function importedVendors(bindings = accounts) {
  const vendors = new Map();
  for (const account of bindings) {
    if (account.source.kind !== 'manual_import') continue;
    if (!vendors.has(account.provider)) vendors.set(account.provider, []);
    vendors.get(account.provider).push(account);
  }
  return vendors;
}
function accountDisplay(id) {
  const account = accounts.find((item) => item.id === id);
  if (!account) return { name: 'Unknown account', label: 'Unknown account' };
  const provider = providerName(account.provider);
  const manual = account.source.kind === 'manual_import';
  const distinct = provider !== account.name;
  return {
    name: manual ? provider : account.name,
    context: distinct ? manual ? account.name : provider : undefined,
    label: distinct ? `${provider} · ${account.name}` : account.name,
  };
}
function accountBadge(id) {
  const account = accounts.find((item) => item.id === id);
  const identity = account?.source.kind === 'manual_import' ? account.provider : id;
  let color = 0;
  for (const character of identity) color = (color * 31 + character.codePointAt(0)) % 6;
  return node('span', accountDisplay(id).name, `badge account-badge account-color-${color}`);
}
function accountCell(id) {
  const { context } = accountDisplay(id);
  const result = cell();
  result.append(accountBadge(id));
  if (context && accounts.find((account) => account.id === id)?.source.kind === 'manual_import') result.append(node('small', context));
  return result;
}
function serviceName(id) {
  for (const account of accounts) {
    const service = account.services.find((item) => item.id === id);
    if (service) return service.name;
  }
  return 'Unknown service';
}
function calendarDate(value) {
  if (!value) return 'Date unavailable';
  const date = new Date(`${value}T12:00:00`);
  return Number.isNaN(date.valueOf()) ? 'Invalid date' : date.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
}
function timestamp(value) {
  if (!value) return 'Not reported';
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? 'Invalid date' : date.toLocaleString(undefined, { year: 'numeric', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit', timeZoneName: 'short' });
}
function money(value) {
  if (!value) return 'Not reported';
  try {
    return new Intl.NumberFormat(undefined, { style: 'currency', currency: value.currency }).format(value.minor_units / 100);
  } catch {
    return `${value.minor_units} minor units (${value.currency})`;
  }
}
function count(value) { return new Intl.NumberFormat().format(value); }
function dateInputValue(date) {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
}
function dateOffset(days) {
  const date = new Date();
  date.setDate(date.getDate() + days);
  return dateInputValue(date);
}
function defaultSyncDates() {
  const today = new Date();
  const from = new Date(today);
  from.setDate(1);
  from.setMonth(from.getMonth() - 3);
  const lastDay = new Date(from.getFullYear(), from.getMonth() + 1, 0).getDate();
  from.setDate(Math.min(today.getDate(), lastDay));
  return [dateInputValue(from), dateInputValue(today)];
}
function badge(value, text = label(value)) {
  return node('span', text, `badge${['complete', 'completed'].includes(value) ? ' good' : ['partial', 'unavailable', 'exception', 'failed', 'interrupted'].includes(value) ? ' attention' : ''}`);
}
function readView() {
  const requested = location.hash.slice(1);
  return Object.hasOwn(views, requested) ? requested : 'overview';
}
function navigate(next) {
  if (next === view) return;
  location.hash = next;
}

function accountSupports(account, capability) {
  if (account.source.kind === 'manual_import') {
    return !!summary?.coverage.some((item) => item.account_id === account.id && item.capability === capability && item.state !== 'unsupported');
  }
  const provider = providers.find((provider) => provider.id === account.provider);
  return !!provider?.capabilities.includes(capability)
    && !summary?.coverage.some((item) => item.account_id === account.id && item.capability === capability && item.state === 'unsupported');
}

function accountsFor(capability) {
  return accounts.filter((account) => accountSupports(account, capability));
}

class ApiError extends Error {
  constructor(status, code) {
    super(status === 401 ? 'This session has expired. Sign in again to refresh saved data.'
      : status === 403 ? 'This user does not have permission for this action.'
      : status === 404 ? 'This record is no longer available.'
      : status === 409 ? 'A sync is already running for this account. Wait for it to finish before starting another.'
      : status === 400 || status === 422 ? 'These filters or dates could not be accepted. Check the selected range.'
      : status === 429 ? 'Too many requests. Try again in a moment.'
      : 'Utilities could not load this data. Previously saved records have not changed.');
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

async function api(path, { signal, body } = {}) {
  let response;
  try {
    response = await fetch(path, {
      method: body === undefined ? 'GET' : 'POST', signal,
      credentials: 'same-origin', headers: body === undefined ? { Accept: 'application/json' } : { Accept: 'application/json', 'Content-Type': 'application/json', 'X-Utilities-Request': '1' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch (error) {
    if (error.name === 'AbortError') throw error;
    throw new ApiError(0);
  }
  if (!response.ok) {
    let code;
    if (response.status === 503) {
      try {
        const payload = await response.json();
        if (typeof payload?.error_code === 'string') code = payload.error_code;
      } catch (error) {
        if (error.name === 'AbortError') throw error;
      }
    }
    throw new ApiError(response.status, code);
  }
  try { return await response.json(); }
  catch { throw new ApiError(502); }
}

function requirePage(value) {
  if (!value || !Array.isArray(value.items) || !Array.isArray(value.coverage)
      || (value.next_page !== null && typeof value.next_page !== 'string')) throw new ApiError(502);
  return value;
}

async function allBindings(path, signal, filters) {
  const items = [];
  const seen = new Set();
  let cursor = null;
  do {
    const query = new URLSearchParams({ limit: '100', ...filters });
    if (cursor) query.set('page', cursor);
    const page = requirePage(await api(`${path}?${query}`, { signal }));
    items.push(...page.items);
    cursor = page.next_page;
    if (cursor && (seen.has(cursor) || seen.size >= 100)) throw new ApiError(502);
    if (cursor) seen.add(cursor);
  } while (cursor);
  return items;
}

function showError(error, accountId) {
  if (error.name === 'AbortError') return;
  if (error instanceof ApiError && error.status === 401) {
    const value = byId('freshness-badge');
    value.textContent = isIngress ? 'Home Assistant session expired' : 'Session expired';
    value.className = 'badge attention';
    shell.setStatus(`${isIngress ? 'Home Assistant sign-in' : 'Sign-in'} required · showing saved data`);
    signInAgain.href = `login#${view}`;
    signInAgain.hidden = isIngress;
    showNotice(isIngress ? 'Your Home Assistant session has expired. Reopen Utilities from Home Assistant.' : error.message, 'error');
    retry.hidden = true;
    return;
  }
  const message = error instanceof ApiError ? error.message : 'Utilities received an unexpected response. Try refreshing.';
  showNotice(accountId ? `${accountLabel(accountId)} · ${message}` : message, 'error');
  retry.hidden = false;
}

function showNotice(message, kind = 'status') {
  errorNotice.textContent = kind === 'error' ? message : '';
  setNotice(notice, kind === 'error' ? '' : message);
}

function importStatus() {
  if (!summary?.import_only_accounts || summary.import_only_accounts !== summary.accounts) return null;
  return summary.last_imported_at ? `Last imported ${timestamp(summary.last_imported_at)}` : 'Imported records';
}

function accountWarnings() {
  const warnings = [];
  for (const account of accounts.filter((item) => item.source.kind === 'live')) {
    const job = syncJobs.find((item) => item.account_id === account.id);
    const manual = summary?.stale_manual_accounts?.find((item) => item.account_id === account.id);
    const automatic = summary?.stale_automatic_accounts?.find((item) => item.account_id === account.id);
    const gaps = (summary?.coverage ?? []).filter((item) => item.account_id === account.id
      && ['partial', 'unavailable', 'not_configured'].includes(item.state) && item.reason !== 'document_import');
    const credentialsMissing = gaps.some((item) => item.state === 'not_configured');
    const messages = [];
    if (job && ['failed', 'interrupted', 'partial'].includes(job.state)) {
      if (job.state !== 'partial' || !credentialsMissing) messages.push(job.state === 'partial' ? 'The latest sync finished with coverage gaps.' : `${label(job.state)}.`);
      if (job.error) messages.push(reasonLabels[job.error] ?? 'Check this account’s provider coverage and configuration.');
    }
    for (const gap of gaps) {
      if (gap.reason === 'not_synced' && (!job || manual)) continue;
      const reason = reasonLabels[gap.reason] ?? `${label(gap.capability)} coverage needs attention.`;
      if (!messages.includes(reason)) messages.push(reason);
    }
    if (manual) messages.push(manual.last_synced_at
      ? `Last successful sync ${timestamp(manual.last_synced_at)}. Saved data may be out of date.`
      : 'No successful sync yet.');
    if (automatic) messages.push(automatic.last_data_at
      ? `Latest provider data ${timestamp(automatic.last_data_at)}. Newer data has not been reported.`
      : 'No provider data reported yet.');
    if (!job && !manual && !automatic) messages.push('Awaiting the first provider sync.');
    if (messages.length) warnings.push({ accountId: account.id, description: messages.join(' ') });
  }
  return warnings;
}

function updateFreshness() {
  const value = byId('freshness-badge');
  const imported = importStatus();
  if (imported) {
    value.textContent = 'Imported records';
    value.className = 'badge';
    shell.setStatus(imported);
    return;
  }
  const warnings = accountWarnings();
  if (warnings.length > 1) {
    value.replaceChildren(...warnings.map((item) => node('span', `${accountLabel(item.accountId)} · Needs attention`, 'badge attention')));
    value.className = 'freshness-badges';
  } else {
    value.textContent = warnings.length ? `${accountLabel(warnings[0].accountId)} · Needs attention` : summary?.last_synced_at ? 'Synced' : 'Connected';
    value.className = `badge${warnings.length ? ' attention' : summary?.last_synced_at ? ' good' : ''}`;
  }
  shell.setStatus(summary?.last_synced_at ? `Most recent sync ${timestamp(summary.last_synced_at)}` : 'Connected to Utilities');
}

function markConnectionLost() {
  const value = byId('freshness-badge');
  value.textContent = 'Connection lost';
  value.className = 'badge attention';
  shell.setStatus('App connection lost · showing saved data');
}

async function load() {
  loadController?.abort();
  loadController = new AbortController();
  const controller = loadController;
  refresh.disabled = true;
  retry.hidden = true;
  showNotice(ready ? 'Refreshing local data…' : 'Connecting to Utilities…');
  try {
    const [system, nextHealth, nextSummary, nextAccounts, nextProviders, nextJobs] = await Promise.all([
      api('v1/system', { signal: controller.signal }), api('health', { signal: controller.signal }),
      api('v1/summary', { signal: controller.signal }),
      allBindings('v1/accounts', controller.signal), allBindings('v1/providers', controller.signal),
      api('v1/sync', { signal: controller.signal }),
    ]);
    if (controller.signal.aborted) return;
    accounts = nextAccounts;
    providers = nextProviders;
    summary = nextSummary;
    health = nextHealth;
    syncJobs = nextJobs;
    if (!Array.isArray(summary.coverage) || !Array.isArray(health.coverage)) throw new ApiError(502);
    ready = true;
    signInAgain.hidden = true;
    shell.setVersion(system.version ?? '');
    updateFreshness();
    showNotice('');
    await renderView();
    pollSync();
  } catch (error) {
    if (error.name !== 'AbortError' && ready && !(error instanceof ApiError && [401, 403].includes(error.status))) markConnectionLost();
    showError(error);
    if (!ready) content.replaceChildren(empty('Utilities is not available', 'Check the app connection, then try again. Provider data has not been changed.'));
  } finally {
    if (loadController === controller) refresh.disabled = false;
    if (!ready) content.setAttribute('aria-busy', 'false');
  }
}

function empty(title, description) {
  const section = node('section', undefined, 'empty');
  section.append(node('h2', title), node('p', description));
  return section;
}
function panel(title, action) {
  const section = node('section', undefined, 'panel');
  const heading = node('div', undefined, 'section-heading');
  heading.append(node('h2', title));
  if (action) {
    const button = node('button', action[0], 'text-action');
    button.type = 'button';
    button.addEventListener('click', () => navigate(action[1]));
    heading.append(button);
  }
  section.append(heading);
  return section;
}
function table(headers, caption, className = '') {
  const wrapper = node('div', undefined, 'table-scroll');
  wrapper.tabIndex = 0;
  wrapper.setAttribute('role', 'region');
  wrapper.setAttribute('aria-label', caption);
  const value = node('table', undefined, className);
  const title = node('caption', caption, 'screen-reader-only');
  const head = node('thead');
  const row = node('tr');
  for (const heading of headers) {
    const cell = node('th', heading.text ?? heading, heading.numeric ? 'numeric' : undefined);
    cell.scope = 'col';
    row.append(cell);
  }
  head.append(row);
  const body = node('tbody');
  value.append(title, head, body);
  wrapper.append(value);
  return { wrapper, body };
}
function cell(text, numeric = false, heading) {
  const value = node('td', text, numeric ? 'numeric' : undefined);
  if (heading) value.dataset.label = heading;
  return value;
}

function coveragePanel(items) {
  const section = panel('Data coverage');
  section.classList.add('coverage-panel');
  const note = node('p', 'Available means the provider returned the requested data, not that all history is present. Missing coverage is never treated as a zero balance or a completed pickup.', 'coverage-note');
  section.append(note);
  const irrelevant = new Set(['provider_not_supported', 'no_documented_payment_history', 'not_a_collection_provider']);
  const relevant = items.filter((item) => item.state !== 'unsupported' || !irrelevant.has(item.reason));
  if (!relevant.length) {
    section.append(node('p', 'No provider coverage has been checked yet. Configure an account and run a sync to establish what is available.', 'panel-empty'));
    return section;
  }
  const { wrapper, body } = table(['Account', 'Data', 'Coverage', 'Range checked', 'Checked'], 'Provider coverage by account and data type', 'coverage-table');
  for (const item of relevant) {
    const row = node('tr');
    const status = cell();
    if (item.reason === 'not_synced') {
      status.append(node('span', 'Awaiting first sync', 'badge'));
    } else {
      status.append(badge(item.state, item.reason === 'document_import' ? 'Imported documents' : label(item.state)));
      if (item.reason) status.append(node('small', reasonLabels[item.reason] ?? 'The provider reported a coverage limitation. Check provider configuration and try syncing again.'));
    }
    row.append(accountCell(item.account_id), cell(label(item.capability)), status,
      cell(item.from && item.to ? `${timestamp(item.from)} to ${timestamp(item.to)} (end exclusive)` : 'Not reported'), cell(timestamp(item.checked_at)));
    body.append(row);
  }
  section.append(wrapper);
  return section;
}

async function renderView() {
  const current = ++generation;
  viewController?.abort();
  viewController = new AbortController();
  const controller = viewController;
  const trash = document.querySelector('[data-view="collections"]');
  const hasCollections = accountsFor('collections').length > 0;
  trash.hidden = !hasCollections;
  if (ready && view === 'collections' && !hasCollections) {
    view = 'overview';
    history.replaceState(null, '', '#overview');
  }
  for (const button of document.querySelectorAll('[data-view]')) button.setAttribute('aria-pressed', String(button.dataset.view === view));
  byId('page-title').textContent = views[view][0];
  byId('page-description').textContent = views[view][1];
  document.title = `${views[view][0]} · Utilities`;
  if (!ready) return;
  content.setAttribute('aria-busy', 'true');
  retry.hidden = true;
  showNotice('');
  try {
    if (view === 'overview') await renderOverview(controller.signal, current);
    else if (view === 'settings') renderSettings();
    else if (view === 'usage') await renderUsage(controller.signal, current);
    else await renderList(view, controller.signal, current);
  } catch (error) {
    if (current === generation) showError(error);
  } finally {
    if (current === generation) content.setAttribute('aria-busy', 'false');
  }
}

async function renderOverview(signal, current) {
  if (!accounts.length) {
    const setup = empty('Connect your first utility', 'Add your utility connection to see bills and usage here.');
    const connect = node('button', 'Connect utility');
    connect.type = 'button';
    connect.disabled = !providers.some((provider) => provider.id === 'bayou' && provider.configured);
    connect.addEventListener('click', () => connectionManager.open());
    setup.append(connect);
    if (connect.disabled) setup.append(node('p', 'Add a Bayou API key in the app configuration to get started.', 'connection-note'));
    content.replaceChildren(setup, connectionManager.section({ pendingOnly: true }));
    return;
  }
  const hasCollections = accountsFor('collections').length > 0;
  const upcoming = new URLSearchParams({ from: dateOffset(0), to: dateOffset(30), limit: '5' });
  const [balances, collections, bills] = await Promise.all([
    api('v1/balances?limit=10', { signal }).then(requirePage),
    hasCollections ? api(`v1/collections?${upcoming}`, { signal }).then(requirePage) : null,
    summary.bills ? allBindings('v1/bills', signal, { from: dateOffset(-365), to: dateOffset(0) }) : [],
  ]);
  if (current !== generation) return;
  const fragment = document.createDocumentFragment();
  const metrics = node('section', undefined, 'metrics');
  metrics.setAttribute('aria-label', 'Saved utility data');
  const vendors = importedVendors();
  const utilityCount = vendors.size + accounts.filter((account) => account.source.kind === 'live').length;
  const values = [[utilityCount, vendors.size ? 'Utilities' : 'Utility accounts'], [summary.bills, 'Saved bills'], ...(hasCollections ? [[summary.collections, 'Saved trash collections']] : []), [summary.usage_intervals, 'Usage intervals']];
  for (const [value, title] of values) {
    const metric = node('div', undefined, 'metric');
    metric.append(node('span', count(value), 'metric-value'), node('span', title, 'metric-label'));
    metrics.append(metric);
  }
  const imported = importStatus();
  const warnings = accountWarnings();
  const freshness = node('section', undefined, `freshness${warnings.length ? ' attention' : ''}`);
  freshness.setAttribute('aria-label', 'Account freshness');
  if (imported) freshness.append(node('strong', imported));
  else {
    const freshnessText = node('div', undefined, 'freshness-accounts');
    for (const warning of warnings) {
      const item = node('div');
      item.append(node('strong', accountLabel(warning.accountId)), node('p', warning.description));
      freshnessText.append(item);
    }
    if (!warnings.length) freshnessText.append(node('strong', summary.last_synced_at ? `Most recent sync ${timestamp(summary.last_synced_at)}` : 'Connected to Utilities'));
    const syncLink = node('button', 'Sync settings', 'text-action');
    syncLink.type = 'button';
    syncLink.addEventListener('click', () => navigate('settings'));
    freshness.append(freshnessText, syncLink);
  }
  const columns = node('div', undefined, 'two-column');
  const balancePanel = panel('Reported balances', ['View bills', 'bills']);
  if (!balances.items.length) balancePanel.append(node('p', 'No balances reported. This does not mean nothing is owed.', 'panel-empty'));
  else {
    const list = node('ul', undefined, 'record-list');
    for (const balance of balances.items) {
      const item = node('li');
      const description = node('div');
      const { name, context } = accountDisplay(balance.account_id);
      description.append(node('strong', name));
      if (context) description.append(node('p', context));
      description.append(node('p', `As of ${timestamp(balance.as_of)}`));
      item.append(description, node('strong', money(balance.amount), 'amount'));
      list.append(item);
    }
    balancePanel.append(list);
    balancePanel.append(node('p', 'A missing account balance is not confirmation that nothing is owed.', 'section-note'));
    if (balances.next_page) balancePanel.append(node('p', 'More balances are available through the API or CLI.', 'section-note'));
  }
  columns.append(balancePanel);
  if (collections) {
  const collectionPanel = panel('Next 30 days', ['All trash collections', 'collections']);
  if (!collections.items.length) collectionPanel.append(node('p', 'No collections reported for the next 30 days.', 'panel-empty'));
  else {
    const list = node('ul', undefined, 'record-list');
    for (const collection of collections.items) {
      const item = node('li');
      const description = node('div');
      description.append(node('strong', serviceName(collection.service_id)), node('p', calendarDate(collection.scheduled_date)));
      item.append(description, badge(collection.status));
      list.append(item);
    }
    collectionPanel.append(list);
    if (collections.next_page) collectionPanel.append(node('p', 'Showing the first 5 collections. Open All trash collections for the full paginated schedule.', 'section-note'));
  }
  columns.append(collectionPanel);
  }
  fragment.append(metrics, freshness);
  const cost = billingTrend(bills);
  if (cost) fragment.append(cost);
  const reportedUsage = reportedUsageTrend(bills);
  if (reportedUsage) fragment.append(reportedUsage);
  fragment.append(columns);
  content.replaceChildren(fragment);
}

function billingTrend(bills) {
  const currencies = new Map();
  for (const { issued_on, new_charges } of bills) {
    if (!new_charges) continue;
    const { currency, minor_units } = new_charges;
    if (!currencies.has(currency)) currencies.set(currency, new Map());
    const months = currencies.get(currency);
    const month = issued_on.slice(0, 7);
    months.set(month, (months.get(month) ?? 0) + minor_units);
  }
  if (!currencies.size) return null;

  const section = panel('Monthly utility charges', ['View bills', 'bills']);
  section.classList.add('billing-trend');
  const body = node('div', undefined, 'panel-body');
  body.append(node('p', 'New charges shown on saved bills issued in the past year.', 'muted'));
  for (const [currency, months] of currencies) {
    if (currencies.size > 1) body.append(node('h3', currency));
    body.append(monthlyHistory(months, (minor_units) => money({ currency, minor_units })));
  }
  section.append(body);
  return section;
}

function monthlyHistory(months, formatAmount) {
  const dateFormat = new Intl.DateTimeFormat(undefined, { month: 'short', year: 'numeric', timeZone: 'UTC' });
  const rows = node('ul');
  const scale = Math.max(...Array.from(months.values(), (value) => Math.abs(Number(value)))) || 1;
  for (const [month, amount] of Array.from(months).sort(([a], [b]) => a.localeCompare(b))) {
    const row = node('li');
    const track = node('div', undefined, 'billing-track');
    const bar = node('div', undefined, Number(amount) < 0 ? 'billing-bar credit' : 'billing-bar');
    bar.style.width = `${Math.abs(Number(amount)) / scale * 100}%`;
    track.setAttribute('aria-hidden', 'true');
    track.append(bar);
    row.append(node('span', dateFormat.format(new Date(`${month}-01T00:00:00Z`))), track,
      node('strong', formatAmount(amount)));
    rows.append(row);
  }
  return rows;
}

function addReportedQuantities(total, value) {
  // Reported quantities are nonnegative decimal strings. Preserve their precision in the displayed total.
  const parts = [total, value].map((item) => item.split('.'));
  const places = Math.max(...parts.map(([, fraction = '']) => fraction.length));
  const [left, right] = parts.map(([whole, fraction = '']) => BigInt(whole + fraction.padEnd(places, '0')));
  const digits = (left + right).toString().padStart(places + 1, '0');
  if (!places) return digits;
  const fraction = digits.slice(-places).replace(/0+$/, '');
  const whole = digits.slice(0, -places);
  return fraction ? `${whole}.${fraction}` : whole;
}

function reportedUsageTrend(bills) {
  const groups = new Map();
  for (const { issued_on, reported_quantities = [] } of bills) {
    for (const { kind, quantity: { value, unit } } of reported_quantities) {
      if (!Object.hasOwn(reportedQuantityNames, kind)) continue;
      const key = `${kind}:${unit}`;
      if (!groups.has(key)) groups.set(key, { kind, unit, months: new Map() });
      const { months } = groups.get(key);
      const month = issued_on.slice(0, 7);
      months.set(month, addReportedQuantities(months.get(month) ?? '0', value));
    }
  }
  if (!groups.size) return null;

  const section = panel('Water and propane history');
  section.classList.add('billing-trend');
  const body = node('div', undefined, 'panel-body');
  body.append(node('p', 'Quantities from saved bills issued in the past year, grouped by bill issue month.', 'muted'));
  for (const { kind, unit, months } of groups.values()) {
    body.append(node('h3', `${reportedQuantityNames[kind]} · ${billingUnits[unit] ?? label(unit)}`),
      monthlyHistory(months, (value) => quantity({ value, unit })));
  }
  section.append(body);
  return section;
}

async function renderUsage(signal, current) {
  if (!accounts.some((account) => account.services.some((service) => ['water', 'propane'].includes(service.kind)))) {
    await renderList('usage', signal, current);
    return;
  }
  const billed = panel('Billed usage');
  billed.classList.add('usage-panel');
  billed.setAttribute('aria-label', 'Billed usage');
  const reported = node('div');
  reported.append(node('p', 'Loading billed quantities…', 'panel-empty'));
  billed.append(reported);
  const intervals = panel('Meter intervals');
  intervals.setAttribute('aria-label', 'Meter intervals');
  const intervalResults = node('div', undefined, 'panel-body');
  intervals.append(intervalResults);
  content.replaceChildren(billed, intervals);
  const history = async () => {
    try {
      const bills = await allBindings('v1/bills', signal, { from: dateOffset(-365), to: dateOffset(0) });
      if (current !== generation) return;
      const { wrapper, body } = table([
        'Billing period', 'Utility and service', { text: 'Quantity', numeric: true }, 'Bill',
      ], 'Billed usage from saved bills', 'billed-usage-table responsive-table');
      for (const bill of bills) {
        for (const item of bill.reported_quantities ?? []) {
          if (!Object.hasOwn(reportedQuantityNames, item.kind)) continue;
          const row = node('tr');
          const service = accountCell(bill.account_id);
          service.append(node('small', serviceName(item.service_id)));
          const source = cell();
          const open = node('button', 'View bill and documents', 'record-button');
          open.type = 'button';
          open.setAttribute('aria-label', `View ${accountName(bill.account_id)} bill and documents issued ${calendarDate(bill.issued_on)}`);
          open.addEventListener('click', () => openBill(bill.id));
          source.append(open, node('small', `Issued ${calendarDate(bill.issued_on)}`));
          const period = bill.period_from && bill.period_to
            ? `${calendarDate(bill.period_from)} – ${calendarDate(bill.period_to)}` : 'Date unavailable';
          row.append(cell(period, false, 'Billing period'), service, cell(quantity(item.quantity), true, 'Quantity'), source);
          body.append(row);
        }
      }
      reported.replaceChildren(body.childElementCount ? wrapper : empty('No billed usage reported', 'Saved bills issued in the past year have no reported water use or propane deliveries.'));
      reported.append(node('p', 'Quantities from bills issued in the past year. Open a bill for its original documents.', 'section-note'));
    } catch (error) {
      if (current === generation && error.name !== 'AbortError') {
        reported.replaceChildren(empty('Could not load billed usage', 'Refresh to try reading saved bills again.'));
      }
      throw error;
    }
  };
  await Promise.all([history(), renderList('usage', signal, current, intervalResults)]);
}

function initialFilters(type) {
  return { account: '', service: '', from: dateOffset(type === 'bills' ? -365 : type === 'collections' ? -30 : -7), to: dateOffset(type === 'collections' ? 30 : 0) };
}
function listStateFor(type) {
  if (!listState.has(type)) listState.set(type, { filters: initialFilters(type), items: [], next: null });
  return listState.get(type);
}
function selectField(title, name, options, value = '', required = false) {
  const field = node('div', undefined, 'form-field');
  const caption = node('label', title);
  const select = node('select');
  select.id = `utility-${name}`;
  caption.htmlFor = select.id;
  select.name = name;
  select.required = required;
  for (const [id, text] of options) {
    const option = node('option', text);
    option.value = id;
    select.append(option);
  }
  select.value = value;
  field.append(caption, select);
  return { field, select };
}
function dateField(title, name, value) {
  const field = node('div', undefined, 'form-field');
  const caption = node('label', title);
  const input = node('input');
  input.id = `utility-${name}`;
  caption.htmlFor = input.id;
  input.type = 'date'; input.name = name; input.value = value; input.required = true;
  field.append(caption, input);
  return { field, input };
}
function setDateValidity(from, to) {
  to.setCustomValidity(from.value && to.value && from.value > to.value ? 'End date must be on or after start date.' : '');
}

function filterForm(type, state, onSubmit) {
  const form = node('form', undefined, 'toolbar');
  form.setAttribute('aria-label', `Filter ${type}`);
  const matchingAccounts = accountsFor(type);
  const vendors = type === 'bills' ? importedVendors(matchingAccounts) : new Map();
  let selected = state.filters.import_provider ? importProviderPrefix + state.filters.import_provider : state.filters.account;
  const options = [
    ['', 'All accounts'],
    ...matchingAccounts.filter((item) => type !== 'bills' || item.source.kind === 'live').map((item) => [item.id, item.name]),
    ...Array.from(vendors.keys(), (id) => [importProviderPrefix + id, providerName(id)]),
  ];
  if (!options.some(([id]) => id === selected)) {
    selected = '';
    state.filters.account = '';
    delete state.filters.import_provider;
  }
  const account = selectField(vendors.size ? 'Account or vendor' : 'Account', 'account', options, selected);
  form.append(account.field);
  let service = null;
  if (type !== 'bills') {
    service = selectField('Service', 'service', serviceOptions(state.filters.account, type), state.filters.service);
    form.append(service.field);
  }
  const from = dateField('From', 'from', state.filters.from);
  const to = dateField('To', 'to', state.filters.to);
  const apply = node('button', 'Apply filters');
  apply.type = 'submit';
  form.append(from.field, to.field, apply);
  from.input.addEventListener('input', () => setDateValidity(from.input, to.input));
  to.input.addEventListener('input', () => setDateValidity(from.input, to.input));
  account.select.addEventListener('change', () => {
    if (!service) return;
    const previous = service.select.value;
    service.select.replaceChildren(...serviceOptions(account.select.value, type).map(([id, text]) => {
      const option = node('option', text); option.value = id; return option;
    }));
    service.select.value = serviceOptions(account.select.value, type).some(([id]) => id === previous) ? previous : '';
  });
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    setDateValidity(from.input, to.input);
    if (!form.reportValidity()) return;
    const selected = account.select.value;
    const vendor = type === 'bills' && selected.startsWith(importProviderPrefix);
    state.filters = {
      account: vendor ? '' : selected,
      ...(vendor ? { import_provider: selected.slice(importProviderPrefix.length) } : {}),
      service: service?.select.value ?? '', from: from.input.value, to: to.input.value,
    };
    onSubmit();
  });
  return form;
}
function serviceOptions(account, capability) {
  return [['', 'All services'], ...accountsFor(capability).filter((item) => !account || item.id === account)
    .flatMap((item) => item.services.map((service) => [service.id, `${service.name} · ${item.name}`]))];
}

async function renderList(type, signal, current, target = content) {
  const state = listStateFor(type);
  const filter = filterForm(type, state, () => renderView());
  const results = node('section');
  results.setAttribute('aria-label', `${label(type)} results`);
  results.append(node('p', 'Loading saved records…', 'muted'));
  target.replaceChildren(filter, results);
  async function page(append) {
    const query = new URLSearchParams({ limit: '25' });
    for (const [key, value] of Object.entries(state.filters)) if (value) query.set(key, value);
    if (append && state.next) query.set('page', state.next);
    const response = requirePage(await api(`v1/${type}?${query}`, { signal }));
    if (current !== generation) return;
    state.items = append ? [...state.items, ...response.items] : response.items;
    state.next = response.next_page;
    draw();
  }
  function draw() {
    results.replaceChildren();
    if (!state.items.length) results.append(empty(`No ${type === 'usage' ? 'meter intervals' : type} reported`, 'No saved records match these filters.'));
    else results.append(recordTable(type, state.items));
    const meta = node('div', undefined, 'list-meta');
    const amount = node('p', `${count(state.items.length)} ${state.items.length === 1 ? 'record' : 'records'} shown${state.next ? ' · more available' : ' · end of saved results'}`);
    amount.setAttribute('role', 'status');
    amount.tabIndex = -1;
    meta.append(amount);
    if (state.next) {
      const more = node('button', 'Load more', 'quiet');
      more.type = 'button';
      more.addEventListener('click', async () => {
        more.disabled = true; more.textContent = 'Loading…';
        retry.hidden = true; showNotice('');
        try {
          await page(true);
          if (current === generation) (results.querySelector('.list-meta button') ?? results.querySelector('.list-meta [role="status"]'))?.focus();
        } catch (error) {
          if (current === generation) {
            showError(error); more.disabled = false; more.textContent = 'Load more';
          }
        }
      });
      meta.append(more);
    }
    results.append(meta);
  }
  try { await page(false); }
  catch (error) {
    if (current === generation && error.name !== 'AbortError') {
      results.replaceChildren(empty(`Could not load ${type}`, 'Try again to read saved records. No provider changes were made.'));
    }
    throw error;
  }
}

function recordTable(type, items) {
  const headers = type === 'bills' ? ['Issued', 'Account', 'Due', { text: 'Total', numeric: true }, 'Source']
    : type === 'collections' ? ['Scheduled', 'Service', 'Status', 'ETA / completion', 'Exception']
    : ['Interval start', 'Interval end', 'Service', { text: 'Quantity', numeric: true }, 'Unit'];
  const { wrapper, body } = table(headers, `${label(type)} matching the selected filters`, type === 'bills' ? 'data-table bills-table responsive-table' : 'data-table');
  for (const item of items) {
    const row = node('tr');
    if (type === 'bills') {
      const issued = cell(undefined, false, 'Issued');
      const open = node('button', calendarDate(item.issued_on), 'record-button');
      open.type = 'button';
      const { name, context } = accountDisplay(item.account_id);
      open.setAttribute('aria-label', `View ${context ? `${name}, ${context}` : name} bill issued ${calendarDate(item.issued_on)}`);
      open.addEventListener('click', () => openBill(item.id));
      issued.append(open);
      const source = cell();
      if (accounts.find((account) => account.id === item.account_id)?.source.kind === 'manual_import') {
        const documents = node('button', 'Original documents', 'record-button');
        documents.type = 'button';
        documents.setAttribute('aria-label', `View original documents for ${accountName(item.account_id)} bill issued ${calendarDate(item.issued_on)}`);
        documents.addEventListener('click', () => openBill(item.id, { showDocuments: true }));
        source.append(documents);
      } else source.append(node('span', `Synced from ${providerName(item.provider)}`));
      row.append(issued, accountCell(item.account_id), cell(calendarDate(item.due_on), false, 'Due'), cell(money(item.total), true, 'Total'), source);
    } else if (type === 'collections') {
      const service = cell(serviceName(item.service_id));
      service.append(node('small', accountName(item.account_id)));
      const status = cell(); status.append(badge(item.status));
      const execution = cell(item.completed_at ? `Completed ${timestamp(item.completed_at)}` : item.eta ? `ETA ${timestamp(item.eta)}` : 'Not reported');
      row.append(cell(calendarDate(item.scheduled_date)), service, status, execution, node('td', item.exception ?? 'None reported', 'exception-text'));
    } else {
      const service = cell(serviceName(item.service_id));
      service.append(node('small', accountName(item.account_id)));
      row.append(cell(timestamp(item.start)), cell(timestamp(item.end)), service, cell(item.quantity, true), cell(item.unit));
    }
    body.append(row);
  }
  return wrapper;
}

async function openBill(id, { showDocuments = false } = {}) {
  detailController?.abort();
  detailController = new AbortController();
  const controller = detailController;
  const body = byId('bill-detail-content');
  body.replaceChildren(node('p', 'Loading bill…', 'muted'));
  if (!detail.open) detail.showModal();
  try {
    const path = `v1/bills/${encodeURIComponent(id)}`;
    const options = { signal: controller.signal };
    const [bill, imported] = await Promise.all([
      api(path, options),
      api(`${path}/details`, options).catch((error) => {
        if (error instanceof ApiError && error.status === 404) return null;
        throw error;
      }),
    ]);
    if (controller.signal.aborted || !detail.open) return;
    if (imported) {
      body.replaceChildren(importedBill(bill, imported));
      if (showDocuments) body.querySelector('.billing-originals a')?.focus();
      return;
    }
    const heading = node('p');
    heading.append(accountBadge(bill.account_id));
    const total = node('p', money(bill.total), 'detail-total');
    const fields = node('dl');
    for (const [title, value] of [
      ['Issued', calendarDate(bill.issued_on)], ['Due', calendarDate(bill.due_on)],
      ['Billing period', bill.period_from && bill.period_to ? `${calendarDate(bill.period_from)} – ${calendarDate(bill.period_to)}` : 'Date unavailable'],
      [bill.provider === 'bayou' ? 'Previous-cycle balance' : 'Outstanding balance', money(bill.outstanding_balance)], ['Provider status', bill.source_status ?? 'Not reported'],
      ['Provider', label(bill.provider)], ['Observed', timestamp(bill.observed_at)],
    ]) fields.append(node('dt', title), node('dd', value));
    const balanceMeaning = bill.provider === 'bayou' ? 'Bill totals cover the entire selected utility account, including meters not selected for usage. Bayou reports outstanding balance from the previous billing cycle.' : 'Outstanding balance is the source-reported value on this bill.';
    body.replaceChildren(heading, total, fields, node('p', `${balanceMeaning} It is not a live account balance or confirmation of settlement. Provider status describes the provider record, not whether a payment settled. No payment actions are available in Utilities.`, 'detail-note'));
  } catch (error) {
    if (error.name === 'AbortError') return;
    if (error instanceof ApiError && error.status === 401) {
      detail.close();
      showError(error);
      return;
    }
    const message = node('p');
    message.className = 'notice error';
    message.setAttribute('role', 'alert');
    message.textContent = error instanceof ApiError ? error.message : 'This bill could not be loaded.';
    body.replaceChildren(message);
  }
}

function billingPeriod(period) {
  if (!period) return null;
  const end = new Date(`${period.end_exclusive}T00:00:00Z`);
  end.setUTCDate(end.getUTCDate() - 1);
  return `${calendarDate(period.start)} – ${calendarDate(end.toISOString().slice(0, 10))}`;
}

function quantity(value) {
  return value ? `${value.value} ${billingUnits[value.unit] ?? label(value.unit)}` : null;
}

function detailFields(entries) {
  const fields = node('dl', undefined, 'billing-fields');
  for (const [name, value] of entries) {
    if (value != null) fields.append(node('dt', name), node('dd', value));
  }
  return fields;
}

function documentLinks(sources) {
  const list = node('ul', undefined, 'billing-documents');
  for (const source of sources) {
    if (!/^[a-f0-9]{64}$/.test(source.sha256)) continue;
    const name = sourceNames[source.role] ?? 'Source document';
    const item = node('li');
    const link = node('a', `Download ${name.toLowerCase()}`);
    link.href = `v1/documents/${source.sha256}`;
    link.download = '';
    item.append(link);
    if (source.pages.length) {
      item.append(node('span', `${source.pages.length === 1 ? 'Page' : 'Pages'} ${source.pages.join(', ')}`));
    }
    list.append(item);
  }
  return list;
}

function billService(service) {
  const titles = { water: 'Water', internet: 'Internet', propane: 'Propane', waste: 'Waste', recycling: 'Recycling' };
  const facts = {
    water: [
      ['Billed use', quantity(service.billed_volume)], ['Current budget', quantity(service.current_budget)],
      ['Next estimated budget', quantity(service.next_estimated_budget)], ['Meter size', service.meter_size],
      ['Reference evapotranspiration', service.eto_inches == null ? null : `${service.eto_inches} in`],
    ],
    internet: [
      ['Plan', service.plan], ['Service period', billingPeriod(service.period)],
      ['Allowance', quantity(service.allowance)], ['Overage', quantity(service.overage)],
    ],
    propane: [
      ['Delivery date', service.delivery_on ? calendarDate(service.delivery_on) : null],
      ['Delivered volume', quantity(service.delivered_volume)],
    ],
    waste: [
      ['Container size', service.container_size], ['Pickup frequency', service.pickup_frequency],
      ['Service period', billingPeriod(service.period)],
    ],
  };
  const fields = detailFields(facts[service.kind === 'recycling' ? 'waste' : service.kind] ?? []);
  const servicePanel = panel(titles[service.kind] ?? 'Service');
  if (fields.childElementCount) servicePanel.append(fields);
  if (service.meter_reads?.length) {
    const { wrapper, body } = table(['Reading date', { text: 'Meter reading', numeric: true }, 'Quality'], 'Water meter readings');
    for (const read of service.meter_reads) {
      const row = node('tr');
      row.append(cell(calendarDate(read.on)), cell(read.register, true), cell(label(read.quality)));
      body.append(row);
    }
    servicePanel.append(wrapper);
  }
  return servicePanel.childElementCount > 1 ? servicePanel : null;
}

function importedBill(bill, { currency, bill: { record }, related }) {
  const amount = (minor_units) => minor_units == null ? null : money({ currency, minor_units });
  const selectedServices = record.scope === 'selected_services';
  const sheet = node('article', undefined, 'imported-bill');
  const { context } = accountDisplay(bill.account_id);
  const account = node('h3', undefined, 'billing-account');
  account.append(accountBadge(bill.account_id));
  sheet.append(account);
  if (context) sheet.append(node('p', context, 'billing-account-context'));
  const totals = node('dl', undefined, 'billing-totals');
  for (const [title, value] of [
    ['Amount due', record.totals.amount_due_minor],
    [selectedServices ? 'New charges for selected services' : 'New charges', record.totals.new_charges_minor],
  ]) {
    if (value == null) continue;
    const metric = node('div');
    metric.append(node('dt', title), node('dd', amount(value)));
    totals.append(metric);
  }
  sheet.append(totals);
  if (selectedServices) {
    sheet.append(node('p', 'This bill omits account-wide totals and payments.', 'muted'));
  }
  sheet.append(detailFields([
    ['Issued', calendarDate(record.issued_on)], ['Due', calendarDate(record.due_on)],
    ['Billing period', billingPeriod(record.period)], ['Previous balance', amount(record.totals.previous_balance_minor)],
    ['Payments received', amount(record.totals.payments_received_minor)], ['Adjustments', amount(record.totals.adjustments_minor)],
    ['Provider status', bill.source_status],
  ]));

  if (record.charges.length) {
    const section = panel('Charges');
    const { wrapper, body } = table([
      'Description', { text: 'Quantity', numeric: true }, { text: 'Rate', numeric: true }, { text: 'Amount', numeric: true },
    ], 'Bill charges', 'billing-charges');
    for (const line of record.charges) {
      const row = node('tr');
      const description = cell(line.label);
      if (line.period) description.append(node('small', billingPeriod(line.period)));
      const rate = line.rate ? `${currency} ${line.rate.amount} / ${billingUnits[line.rate.per] ?? label(line.rate.per)}` : '—';
      row.append(description, cell(quantity(line.quantity) ?? '—', true), cell(rate, true), cell(amount(line.amount_minor), true));
      body.append(row);
    }
    section.append(wrapper);
    sheet.append(section);
  }
  for (const service of record.services) {
    const section = billService(service);
    if (section) sheet.append(section);
  }

  for (const [kind, title] of [['payment', 'Related payments'], ['propane_delivery', 'Related deliveries']]) {
    const items = related.map((item) => item.record).filter((item) => item.kind === kind);
    if (!items.length) continue;
    const section = panel(title);
    const list = node('ul', undefined, 'record-list billing-related');
    for (const item of items) {
      const isPayment = kind === 'payment';
      const row = node('li');
      const description = node('div');
      description.append(node('strong', calendarDate(isPayment ? item.paid_on : item.delivered_on)));
      if (isPayment) description.append(node('p', label(item.status)));
      description.append(documentLinks(item.sources));
      row.append(description, node('strong', isPayment ? amount(item.amount_minor) : quantity(item.volume), 'amount'));
      list.append(row);
    }
    section.append(list);
    sheet.append(section);
  }

  const originals = documentLinks(record.sources);
  if (originals.childElementCount) {
    const section = panel('Original documents');
    section.classList.add('billing-originals');
    section.append(originals);
    sheet.append(section);
  }
  return sheet;
}

function renderSettings() {
  const fragment = document.createDocumentFragment();
  fragment.append(connectionManager.section());
  const syncAccounts = accounts.filter((account) => account.source.kind === 'live');
  const providerGrid = node('section', undefined, 'provider-grid');
  providerGrid.setAttribute('aria-label', 'Utility providers');
  for (const provider of providers) {
    const card = node('article', undefined, 'panel');
    const heading = node('div', undefined, 'section-heading');
    heading.append(node('h2', provider.name));
    if (provider.id === 'waste_management' || provider.id === 'bayou') {
      heading.append(badge(provider.configured ? 'complete' : 'not_configured', provider.configured ? provider.id === 'bayou' ? 'API key configured' : 'Credentials configured' : 'Not configured'));
      const description = provider.id === 'bayou'
        ? provider.configured
          ? 'The API key is configured. Connect your utility above, sign in through Bayou, and choose its accounts and meters. A key alone does not connect a utility account.'
          : 'Add a Bayou API key in the app configuration. Then connect your utility above to choose the accounts and meters to use.'
        : provider.configured
          ? 'Credentials are present. A successful sync is still required to verify account access and data coverage.'
          : 'A WM-issued token and ClientId are required. Residential eligibility and API pricing must be confirmed with WM.';
      card.append(heading, node('p', description));
    } else {
      heading.append(badge('complete', 'Document imports'));
      card.append(heading);
    }
    const capabilities = node('div', undefined, 'capabilities');
    capabilities.setAttribute('aria-label', 'Supported data');
    for (const capability of provider.capabilities) capabilities.append(node('span', label(capability)));
    card.append(capabilities);
    providerGrid.append(card);
  }
  if (!providers.length) providerGrid.append(empty('No providers available', 'Check the app configuration.'));
  const bindings = panel('Utility accounts');
  bindings.classList.add('settings-section');
  if (!accounts.length) bindings.append(node('p', 'Connect a utility and choose its meters before syncing. Waste Management accounts use explicit bindings in the app configuration.', 'panel-empty'));
  else {
    const grid = node('div', undefined, 'panel-body');
    grid.classList.add('account-grid');
    const apiSources = { bayou: 'Bayou', waste_management: 'Waste Management API' };
    const displayAccounts = [
      ...syncAccounts,
      ...Array.from(importedVendors(), ([provider, bindings]) => ({
        name: providerName(provider), source: { kind: 'manual_import' },
        services: bindings.flatMap((account) => account.services),
      })),
    ];
    for (const account of displayAccounts) {
      const card = node('article');
      card.append(node('h3', account.name));
      const source = account.source.kind === 'manual_import' ? 'Manual import' : apiSources[account.source.provider];
      const categories = node('div', undefined, 'capabilities');
      categories.setAttribute('role', 'group');
      categories.setAttribute('aria-label', 'Data source and service categories');
      if (source) categories.append(node('span', source, 'badge'));
      for (const kind of new Set(account.services.map((service) => service.kind))) {
        categories.append(node('span', label(kind), 'badge'));
      }
      card.append(categories);
      if (!account.services.length) card.append(node('p', 'No services connected.'));
      const serviceNames = new Set(account.services.filter((service) => service.name !== account.name && service.name !== label(service.kind)).map((service) => service.name));
      if (account.services.length > 1 && serviceNames.size) {
        const details = node('details', undefined, 'account-services');
        const services = node('div', undefined, 'capabilities');
        for (const name of serviceNames) {
          services.append(node('span', name));
        }
        details.append(node('summary', 'Services'), services);
        card.append(details);
      }
      grid.append(card);
    }
    bindings.append(grid);
  }
  const sync = panel('Sync provider data');
  sync.classList.add('settings-section');
  const body = node('div', undefined, 'panel-body');
  if (syncAccounts.length) {
    body.append(node('p', 'Fetch bills, collections, and usage for one bound account. This only reads provider data. It cannot pay a bill, book a pickup, or change a service.', 'muted'),
      syncForm(syncAccounts));
  } else {
    body.append(node('p', 'Connect an account to a utility provider to use sync.', 'muted'));
  }
  body.append(syncStatus());
  sync.append(body);
  const diagnostics = node('details', undefined, 'panel settings-section diagnostics');
  diagnostics.append(node('summary', 'Diagnostics'), providerGrid, coveragePanel(summary.coverage));
  fragment.append(bindings, sync, diagnostics);
  content.replaceChildren(fragment);
}

function syncStatus() {
  const status = node('div', undefined, 'sync-status');
  status.id = 'sync-status';
  status.setAttribute('role', 'status');
  renderSyncStatus(status);
  return status;
}

function renderSyncStatus(status) {
  status.replaceChildren(...syncJobs.map((job) => node('p', syncDescription(job))));
  status.hidden = syncJobs.length === 0;
}

function syncForm(syncAccounts) {
  const wrapper = node('div');
  const form = node('form', undefined, 'sync-form');
  const account = selectField('Account to sync', 'account', [['', 'Choose an account'], ...syncAccounts.map((item) => [item.id, item.name])], '', true);
  const [defaultFrom, defaultTo] = defaultSyncDates();
  const from = dateField('From', 'from', defaultFrom);
  const to = dateField('To', 'to', defaultTo);
  const submit = node('button', 'Sync account');
  submit.type = 'submit';
  const all = node('button', 'Sync all available history', 'quiet');
  all.type = 'button';
  form.append(account.field, from.field, to.field, submit);
  const availability = node('p', '', 'sync-availability');
  availability.id = 'sync-availability';
  availability.setAttribute('role', 'status');
  availability.setAttribute('aria-live', 'polite');
  submit.setAttribute('aria-describedby', availability.id);
  all.setAttribute('aria-describedby', availability.id);
  let submitting = false;
  function canSync() {
    const binding = syncAccounts.find((item) => item.id === account.select.value);
    return !!binding && providers.some((provider) => provider.id === binding.provider && provider.configured);
  }
  function updateAvailability() {
    const binding = syncAccounts.find((item) => item.id === account.select.value);
    const running = syncJobs.some((job) => job.account_id === binding?.id && job.state === 'running');
    submit.disabled = submitting || running || !canSync();
    all.disabled = submit.disabled;
    account.select.disabled = submitting;
    const context = binding ? accountLabel(binding.id) : '';
    availability.textContent = submitting ? `${context} · Starting sync…` : running ? `${context} · Already syncing.`
      : !binding ? 'Choose an account to sync.'
      : !canSync() ? `${context} · Provider credentials are not configured.` : '';
  }
  account.select.addEventListener('change', updateAvailability);
  updateSyncForm = updateAvailability;
  updateAvailability();
  from.input.addEventListener('input', () => setDateValidity(from.input, to.input));
  to.input.addEventListener('input', () => setDateValidity(from.input, to.input));
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    setDateValidity(from.input, to.input);
    if (form.reportValidity()) startSync({ from: from.input.value, to: to.input.value });
  });
  all.addEventListener('click', () => startSync({ range: 'all' }));
  async function startSync(range) {
    if (submit.disabled || !canSync()) return;
    const accountId = account.select.value;
    submitting = true;
    updateAvailability();
    showNotice(''); retry.hidden = true;
    try {
      const job = await api('v1/sync', { body: { account: accountId, ...range } });
      syncJobs = [...syncJobs.filter((item) => item.account_id !== job.account_id), job];
      const status = byId('sync-status');
      if (status) renderSyncStatus(status);
      pollSync();
    } catch (error) {
      showError(error, accountId);
    } finally {
      submitting = false;
      updateAvailability();
    }
  }
  const history = node('div', undefined, 'history-sync');
  history.append(all, node('p', 'Reads the history your provider already makes available.', 'muted'));
  wrapper.append(form, history, availability);
  return wrapper;
}
function syncDescription(job) {
  if (job.state === 'running') return `Syncing ${accountLabel(job.account_id)}…`;
  const state = job.state === 'complete' ? 'Sync complete' : job.state === 'partial' ? 'Sync finished with coverage gaps' : label(job.state);
  const records = `${count(job.observations)} ${job.observations === 1 ? 'observation' : 'observations'} processed`;
  return `${state} · ${accountLabel(job.account_id)} · ${records}${job.error ? '. Check this account’s provider coverage and configuration.' : '.'}`;
}
function pollSync() {
  clearTimeout(syncTimer);
  syncPollController?.abort();
  if (!ready || document.hidden) return;
  const delay = syncJobs.some((job) => job.state === 'running') ? 1500 : 15000;
  syncTimer = setTimeout(async () => {
    const controller = new AbortController();
    syncPollController = controller;
    try {
      const next = await api('v1/sync', { signal: controller.signal });
      if (controller.signal.aborted) return;
      const completed = next.some((job) => job.state !== 'running' && !syncJobs.some((prior) => prior.id === job.id && prior.state === job.state));
      syncJobs = next;
      const status = byId('sync-status');
      if (status) renderSyncStatus(status);
      if (view === 'settings') updateSyncForm?.();
      if (completed) {
        const options = { signal: controller.signal };
        const [nextSummary, nextHealth] = await Promise.all([api('v1/summary', options), api('health', options)]);
        if (controller.signal.aborted) return;
        summary = nextSummary; health = nextHealth; updateFreshness();
        if (view === 'settings') document.querySelector('.coverage-panel')?.replaceWith(coveragePanel(summary.coverage));
        else await renderView();
      }
      pollSync();
    } catch (error) {
      if (controller.signal.aborted) return;
      const status = byId('sync-status');
      if (status) status.textContent = syncJobs.map((job) => `${accountLabel(job.account_id)} · Sync status could not be checked. The last saved state was ${job.state}.`).join(' ');
      showError(error);
    }
  }, delay);
}

document.addEventListener('visibilitychange', pollSync);
for (const button of document.querySelectorAll('[data-view]')) button.addEventListener('click', () => navigate(button.dataset.view));
window.addEventListener('hashchange', () => {
  const requested = location.hash.slice(1);
  if (requested && !Object.hasOwn(views, requested)) return;
  const next = readView();
  if (next === view) return;
  const detailWasOpen = detail.open;
  if (detailWasOpen) detail.close();
  view = next;
  signInAgain.href = `login#${view}`;
  renderView();
  // The old bill row is removed by navigation; keep focus in the new main view.
  if (detailWasOpen) byId('main').focus();
});
document.querySelector('.skip-link').addEventListener('click', (event) => {
  event.preventDefault();
  const main = byId('main');
  main.focus();
  main.scrollIntoView({ block: 'start' });
});
refresh.addEventListener('click', () => load());
retry.addEventListener('click', () => load());
byId('close-detail').addEventListener('click', () => detail.close());
detail.addEventListener('close', () => detailController?.abort());
window.addEventListener('pagehide', () => {
  clearTimeout(syncTimer); viewController?.abort(); detailController?.abort(); loadController?.abort();
});
load();
