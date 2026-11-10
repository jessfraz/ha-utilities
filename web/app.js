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
  bills: ['Bills', 'Whole-account bills. Meter selection limits usage, not bill totals. A bill is not proof of payment.'],
  collections: ['Trash collections', 'Scheduled pickups, completion reports, and exceptions.'],
  usage: ['Usage', 'Meter intervals, with the units and time ranges reported by the provider.'],
  settings: ['Settings', 'Utility connections, provider coverage, and sync.'],
};
const names = {
  waste_management: 'Waste Management', bayou: 'Bayou',
  bills: 'Bills', balances: 'Balances', payments: 'Payments', services: 'Services',
  collections: 'Trash collections', usage: 'Usage',
  complete: 'Available', partial: 'Partial coverage', unavailable: 'Unavailable',
  unsupported: 'Not supported', not_configured: 'Not configured',
  scheduled: 'Scheduled', completed: 'Completed', exception: 'Exception', unknown: 'Unknown',
  running: 'Syncing', failed: 'Sync failed', interrupted: 'Sync interrupted',
  waste: 'Waste', recycling: 'Recycling', electricity: 'Electricity', gas: 'Gas', water: 'Water', other: 'Other',
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
let activeSync = null;
let loadController = null;
const listState = new Map();
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
function serviceName(id) {
  for (const account of accounts) {
    const service = account.services.find((item) => item.id === id);
    if (service) return service.name;
  }
  return 'Unknown service';
}
function calendarDate(value) {
  if (!value) return 'Not reported';
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
function dateOffset(days) {
  const date = new Date();
  date.setDate(date.getDate() + days);
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
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
  const provider = providers.find((provider) => provider.id === account.provider);
  return !!provider?.capabilities.includes(capability)
    && !summary?.coverage.some((item) => item.account_id === account.id && item.capability === capability && item.state === 'unsupported');
}

function accountsFor(capability) {
  return accounts.filter((account) => accountSupports(account, capability));
}

class ApiError extends Error {
  constructor(status) {
    super(status === 401 ? 'This session has expired. Sign in again to refresh saved data.'
      : status === 403 ? 'This user does not have permission for this action.'
      : status === 404 ? 'This record is no longer available.'
      : status === 409 ? 'A sync is already running for this account. Wait for it to finish before starting another.'
      : status === 400 || status === 422 ? 'These filters or dates could not be accepted. Check the selected range.'
      : status === 429 ? 'Too many requests. Try again in a moment.'
      : 'Utilities could not load this data. Previously saved records have not changed.');
    this.name = 'ApiError';
    this.status = status;
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
  if (!response.ok) throw new ApiError(response.status);
  try { return await response.json(); }
  catch { throw new ApiError(502); }
}

function requirePage(value) {
  if (!value || !Array.isArray(value.items) || !Array.isArray(value.coverage)
      || (value.next_page !== null && typeof value.next_page !== 'string')) throw new ApiError(502);
  return value;
}

async function allBindings(path, signal) {
  const items = [];
  const seen = new Set();
  let cursor = null;
  do {
    const query = new URLSearchParams({ limit: '100' });
    if (cursor) query.set('page', cursor);
    const page = requirePage(await api(`${path}?${query}`, { signal }));
    items.push(...page.items);
    cursor = page.next_page;
    if (cursor && (seen.has(cursor) || seen.size >= 100)) throw new ApiError(502);
    if (cursor) seen.add(cursor);
  } while (cursor);
  return items;
}

function showError(error) {
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
  showNotice(error instanceof ApiError ? error.message : 'Utilities received an unexpected response. Try refreshing.', 'error');
  retry.hidden = false;
}

function showNotice(message, kind = 'status') {
  errorNotice.textContent = kind === 'error' ? message : '';
  setNotice(notice, kind === 'error' ? '' : message);
}

function updateFreshness() {
  const value = byId('freshness-badge');
  const covered = summary?.coverage ?? [];
  const gaps = covered.some((item) => ['partial', 'unavailable', 'not_configured'].includes(item.state));
  const state = !summary?.last_synced_at ? 'No sync yet' : summary.stale ? 'Data is stale' : gaps ? 'Coverage gaps' : 'Synced';
  value.textContent = state;
  value.className = `badge${summary?.stale || gaps ? ' attention' : summary?.last_synced_at ? ' good' : ''}`;
  shell.setStatus(summary?.last_synced_at ? `Last synced ${timestamp(summary.last_synced_at)}` : 'No successful sync yet');
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
    const [system, nextHealth, nextSummary, nextAccounts, nextProviders] = await Promise.all([
      api('v1/system', { signal: controller.signal }), api('health', { signal: controller.signal }),
      api('v1/summary', { signal: controller.signal }),
      allBindings('v1/accounts', controller.signal), allBindings('v1/providers', controller.signal),
    ]);
    if (controller.signal.aborted) return;
    accounts = nextAccounts;
    providers = nextProviders;
    summary = nextSummary;
    health = nextHealth;
    if (!Array.isArray(summary.coverage) || !Array.isArray(health.coverage)) throw new ApiError(502);
    ready = true;
    signInAgain.hidden = true;
    shell.setVersion(system.version ?? '');
    updateFreshness();
    showNotice('');
    await renderView();
    if (activeSync?.state === 'running') pollSync(activeSync.id);
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
function cell(text, numeric = false) { return node('td', text, numeric ? 'numeric' : undefined); }

function coveragePanel(items) {
  const section = panel('Data coverage');
  section.classList.add('coverage-panel');
  const note = node('p', 'Available means the provider returned the requested data, not that all history is present. Missing coverage is never treated as a zero balance or a completed pickup.', 'coverage-note');
  section.append(note);
  if (!items.length) {
    section.append(node('p', 'No provider coverage has been checked yet. Configure an account and run a sync to establish what is available.', 'panel-empty'));
    return section;
  }
  const { wrapper, body } = table(['Account', 'Data', 'Coverage', 'Range checked', 'Checked'], 'Provider coverage by account and data type', 'coverage-table');
  for (const item of items) {
    const row = node('tr');
    const status = cell();
    status.append(badge(item.state));
    if (item.reason) status.append(node('small', reasonLabels[item.reason] ?? 'The provider reported a coverage limitation. Check provider configuration and try syncing again.'));
    row.append(cell(accountName(item.account_id)), cell(label(item.capability)), status,
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
    content.replaceChildren(setup, connectionManager.section({ pendingOnly: true }), coveragePanel(summary.coverage));
    return;
  }
  const hasCollections = accountsFor('collections').length > 0;
  const upcoming = new URLSearchParams({ from: dateOffset(0), to: dateOffset(30), limit: '5' });
  const [balances, collections] = await Promise.all([
    api('v1/balances?limit=10', { signal }).then(requirePage),
    hasCollections ? api(`v1/collections?${upcoming}`, { signal }).then(requirePage) : null,
  ]);
  if (current !== generation) return;
  const fragment = document.createDocumentFragment();
  const metrics = node('section', undefined, 'metrics');
  metrics.setAttribute('aria-label', 'Saved utility data');
  const values = [[summary.accounts, 'Connected accounts'], [summary.bills, 'Saved bills'], ...(hasCollections ? [[summary.collections, 'Saved trash collections']] : []), [summary.usage_intervals, 'Usage intervals']];
  for (const [value, title] of values) {
    const metric = node('div', undefined, 'metric');
    metric.append(node('span', count(value), 'metric-value'), node('span', title, 'metric-label'));
    metrics.append(metric);
  }
  const freshness = node('section', undefined, `freshness${summary.stale ? ' attention' : ''}`);
  const freshnessText = node('div');
  freshnessText.append(node('strong', summary.last_synced_at ? `Last synced ${timestamp(summary.last_synced_at)}` : 'No successful sync yet'),
    node('p', summary.stale ? 'Saved data may be out of date. Coverage below shows which provider results can be relied on.' : 'These are saved provider observations. Coverage and timestamps describe what is known.'));
  const syncLink = node('button', 'Sync settings', 'text-action');
  syncLink.type = 'button';
  syncLink.addEventListener('click', () => navigate('settings'));
  freshness.append(freshnessText, syncLink);
  const columns = node('div', undefined, 'two-column');
  const balancePanel = panel('Reported balances', ['View bills', 'bills']);
  if (!balances.items.length) balancePanel.append(node('p', 'No balances reported. This does not mean nothing is owed.', 'panel-empty'));
  else {
    const list = node('ul', undefined, 'record-list');
    for (const balance of balances.items) {
      const item = node('li');
      const description = node('div');
      description.append(node('strong', accountName(balance.account_id)), node('p', `As of ${timestamp(balance.as_of)}`));
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
  if (!collections.items.length) collectionPanel.append(node('p', 'No collections reported for the next 30 days. Check coverage before relying on an empty schedule.', 'panel-empty'));
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
  fragment.append(metrics, freshness, columns, coveragePanel(summary.coverage));
  content.replaceChildren(fragment);
}

function initialFilters(type) {
  return { account: '', service: '', from: dateOffset(type === 'bills' ? -365 : type === 'collections' ? -30 : -7), to: dateOffset(type === 'collections' ? 30 : 0) };
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
  const account = selectField('Account', 'account', [['', 'All accounts'], ...accountsFor(type).map((item) => [item.id, item.name])], state.filters.account);
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
    state.filters = { account: account.select.value, service: service?.select.value ?? '', from: from.input.value, to: to.input.value };
    onSubmit();
  });
  return form;
}
function serviceOptions(account, capability) {
  return [['', 'All services'], ...accountsFor(capability).filter((item) => !account || item.id === account)
    .flatMap((item) => item.services.map((service) => [service.id, `${service.name} · ${item.name}`]))];
}

async function renderList(type, signal, current) {
  if (!listState.has(type)) listState.set(type, { filters: initialFilters(type), items: [], next: null, coverage: [] });
  const state = listState.get(type);
  const filter = filterForm(type, state, () => renderView());
  const results = node('section');
  results.setAttribute('aria-label', `${label(type)} results`);
  results.append(node('p', 'Loading saved records…', 'muted'));
  content.replaceChildren(filter, results);
  async function page(append) {
    const query = new URLSearchParams({ limit: '25' });
    for (const [key, value] of Object.entries(state.filters)) if (value) query.set(key, value);
    if (append && state.next) query.set('page', state.next);
    const response = requirePage(await api(`v1/${type}?${query}`, { signal }));
    if (current !== generation) return;
    state.items = append ? [...state.items, ...response.items] : response.items;
    state.next = response.next_page;
    state.coverage = response.coverage;
    draw();
  }
  function draw() {
    results.replaceChildren();
    if (!state.items.length) results.append(empty(`No ${type} reported`, 'No saved records match these filters. This is not confirmation that the provider has complete coverage for this period.'));
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
    results.append(meta, coveragePanel(state.coverage));
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
  const headers = type === 'bills' ? ['Issued', 'Account', 'Due', { text: 'Total', numeric: true }, 'Provider status']
    : type === 'collections' ? ['Scheduled', 'Service', 'Status', 'ETA / completion', 'Exception']
    : ['Interval start', 'Interval end', 'Service', { text: 'Quantity', numeric: true }, 'Unit'];
  const { wrapper, body } = table(headers, `${label(type)} matching the selected filters`, 'data-table');
  for (const item of items) {
    const row = node('tr');
    if (type === 'bills') {
      const issued = cell();
      const open = node('button', calendarDate(item.issued_on), 'record-button');
      open.type = 'button';
      open.setAttribute('aria-label', `View ${accountName(item.account_id)} bill issued ${calendarDate(item.issued_on)}`);
      open.addEventListener('click', () => openBill(item.id));
      issued.append(open);
      const providerStatus = cell(item.source_status ?? 'Not reported');
      if (item.provider === 'bayou' && item.source_status) providerStatus.append(node('small', 'Data access status, not payment'));
      row.append(issued, cell(accountName(item.account_id)), cell(calendarDate(item.due_on)), cell(money(item.total), true), providerStatus);
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

async function openBill(id) {
  detailController?.abort();
  detailController = new AbortController();
  const controller = detailController;
  const body = byId('bill-detail-content');
  body.replaceChildren(node('p', 'Loading bill…', 'muted'));
  if (!detail.open) detail.showModal();
  try {
    const bill = await api(`v1/bills/${encodeURIComponent(id)}`, { signal: controller.signal });
    if (controller.signal.aborted || !detail.open) return;
    const heading = node('p', accountName(bill.account_id), 'muted');
    const total = node('p', money(bill.total), 'detail-total');
    const fields = node('dl');
    for (const [title, value] of [
      ['Issued', calendarDate(bill.issued_on)], ['Due', calendarDate(bill.due_on)],
      ['Billing period', bill.period_from && bill.period_to ? `${calendarDate(bill.period_from)} – ${calendarDate(bill.period_to)}` : 'Not reported'],
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

function renderSettings() {
  const fragment = document.createDocumentFragment();
  fragment.append(connectionManager.section());
  const providerGrid = node('section', undefined, 'provider-grid');
  providerGrid.setAttribute('aria-label', 'Utility providers');
  for (const provider of providers) {
    const card = node('article', undefined, 'panel');
    const heading = node('div', undefined, 'section-heading');
    heading.append(node('h2', provider.name), badge(provider.configured ? 'complete' : 'not_configured', provider.configured ? provider.id === 'bayou' ? 'API key configured' : 'Credentials configured' : 'Not configured'));
    const description = provider.configured ? 'Credentials are present. A successful sync is still required to verify account access and data coverage.'
      : provider.id === 'waste_management' ? 'A WM-issued token and ClientId are required. Residential eligibility and API pricing must be confirmed with WM.'
      : 'Add a Bayou API key in the app configuration. Then connect your utility above to choose the accounts and meters to use.';
    const providerDescription = provider.id === 'bayou' && provider.configured
      ? 'The API key is configured. Connect your utility above, sign in through Bayou, and choose its accounts and meters. A key alone does not connect a utility account.' : description;
    card.append(heading, node('p', providerDescription));
    const capabilities = node('div', undefined, 'capabilities');
    capabilities.setAttribute('aria-label', 'Supported data');
    for (const capability of provider.capabilities) capabilities.append(node('span', label(capability)));
    card.append(capabilities);
    providerGrid.append(card);
  }
  if (!providers.length) providerGrid.append(empty('No providers available', 'Check the app configuration.'));
  fragment.append(providerGrid);
  const bindings = panel('Bound accounts');
  bindings.classList.add('settings-section');
  if (!accounts.length) bindings.append(node('p', 'Connect a utility and choose its meters before syncing. Waste Management accounts use explicit bindings in the app configuration.', 'panel-empty'));
  else {
    const grid = node('div', undefined, 'panel-body');
    grid.classList.add('account-grid');
    for (const account of accounts) {
      const card = node('article');
      card.append(node('h3', account.name), node('p', label(account.provider)));
      const services = node('ul');
      for (const service of account.services) services.append(node('li', `${service.name} · ${label(service.kind)}`));
      if (!account.services.length) card.append(node('p', 'No services bound.'));
      else card.append(services);
      grid.append(card);
    }
    bindings.append(grid);
  }
  const sync = panel('Sync provider data');
  sync.classList.add('settings-section');
  const body = node('div', undefined, 'panel-body');
  body.append(node('p', 'Fetch bills, collections, and usage for one bound account. This only reads provider data. It cannot pay a bill, book a pickup, or change a service.', 'muted'));
  if (accounts.length) body.append(syncForm());
  else body.append(node('p', 'Sync is unavailable until an account is configured.', 'muted'));
  sync.append(body);
  fragment.append(bindings, sync, coveragePanel(summary.coverage));
  content.replaceChildren(fragment);
}

function syncForm() {
  const wrapper = node('div');
  const form = node('form', undefined, 'sync-form');
  const account = selectField('Account to sync', 'account', [['', 'Choose an account'], ...accounts.map((item) => [item.id, item.name])], '', true);
  const from = dateField('From', 'from', dateOffset(-30));
  const to = dateField('To', 'to', dateOffset(0));
  const submit = node('button', 'Sync account');
  submit.type = 'submit';
  form.append(account.field, from.field, to.field, submit);
  const availability = node('p', '', 'sync-availability');
  availability.id = 'sync-availability';
  availability.setAttribute('role', 'status');
  availability.setAttribute('aria-live', 'polite');
  submit.setAttribute('aria-describedby', availability.id);
  const status = node('p', '', 'sync-status');
  status.id = 'sync-status';
  status.setAttribute('role', 'status'); status.setAttribute('aria-live', 'polite');
  if (activeSync) status.textContent = syncDescription(activeSync);
  function canSync() {
    const binding = accounts.find((item) => item.id === account.select.value);
    return !!binding && providers.some((provider) => provider.id === binding.provider && provider.configured);
  }
  function updateAvailability() {
    const binding = accounts.find((item) => item.id === account.select.value);
    const running = activeSync?.state === 'running';
    submit.disabled = running || !canSync();
    availability.textContent = running ? 'A sync is already running. Wait for it to finish.'
      : !binding ? 'Choose an account to sync.'
      : !canSync() ? `${label(binding.provider)} credentials are not configured.` : '';
  }
  account.select.addEventListener('change', updateAvailability);
  updateAvailability();
  from.input.addEventListener('input', () => setDateValidity(from.input, to.input));
  to.input.addEventListener('input', () => setDateValidity(from.input, to.input));
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    setDateValidity(from.input, to.input);
    if (!form.reportValidity() || activeSync?.state === 'running' || !canSync()) return;
    submit.disabled = true;
    status.textContent = 'Requesting a read-only sync…';
    showNotice(''); retry.hidden = true;
    try {
      activeSync = await api('v1/sync', { body: { account: account.select.value, from: from.input.value, to: to.input.value } });
      status.textContent = syncDescription(activeSync);
      pollSync(activeSync.id);
    } catch (error) {
      status.textContent = 'The sync request could not be confirmed. No automatic retry was sent.';
      showError(error); updateAvailability();
    }
  });
  wrapper.append(form, availability, status);
  return wrapper;
}
function syncDescription(job) {
  const state = job.state === 'complete' ? 'Sync complete' : job.state === 'partial' ? 'Sync finished with coverage gaps' : label(job.state);
  const records = `${count(job.observations)} saved ${job.observations === 1 ? 'observation' : 'observations'}`;
  return `${state} · ${accountName(job.account_id)} · ${records}${job.error ? '. Check provider coverage and configuration.' : '.'}`;
}
function pollSync(id) {
  clearTimeout(syncTimer);
  syncTimer = setTimeout(async () => {
    try {
      activeSync = await api(`v1/sync/${encodeURIComponent(id)}`);
      const status = byId('sync-status');
      if (status) status.textContent = syncDescription(activeSync);
      if (activeSync.state === 'running') pollSync(id);
      else {
        const [nextSummary, nextHealth] = await Promise.all([api('v1/summary'), api('health')]);
        summary = nextSummary; health = nextHealth; updateFreshness();
        await renderView();
      }
    } catch (error) {
      const status = byId('sync-status');
      if (status) status.textContent = 'Sync status could not be checked. The request may still be running; no second sync was sent.';
      showError(error);
    }
  }, 1500);
}

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
