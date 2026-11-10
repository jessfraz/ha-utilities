// Hosted authorization stays with Bayou. Only durable connection status is kept here.
function openBayouTab() {
  let tab;
  let placeholder;
  try {
    tab = window.open('about:blank', '_blank');
    if (!tab) return null;
    tab.opener = null;
    placeholder = tab.document;
  } catch {
    tab?.close();
    return null;
  }
  // Keep ownership only while this is the blank document we opened.
  function ownsPlaceholder() {
    try { return !tab.closed && tab.document === placeholder; }
    catch { return false; }
  }
  function close() {
    if (ownsPlaceholder()) tab.close();
  }
  return {
    navigate(url) {
      if (!ownsPlaceholder()) return false;
      try {
        const link = placeholder.createElement('a');
        link.href = url.href;
        link.rel = 'noreferrer';
        link.referrerPolicy = 'no-referrer';
        (placeholder.body ?? placeholder.documentElement).append(link);
        link.click();
        return true;
      } catch {
        close();
        return false;
      }
    },
    close,
  };
}

export function createConnections({ api, node, providers, updated, connected, onError }) {
  const dialog = document.createElement('dialog');
  dialog.className = 'connection-dialog';
  dialog.setAttribute('aria-labelledby', 'connection-title');
  const header = node('div', undefined, 'dialog-heading');
  const title = node('h2', 'Connect utility');
  title.id = 'connection-title';
  const close = node('button', 'Close', 'quiet');
  close.type = 'button';
  close.setAttribute('aria-label', 'Close utility connection');
  header.append(title, close);
  const body = node('div', undefined, 'connection-body');
  const error = node('p', '', 'notice error');
  error.setAttribute('role', 'alert');
  const footer = node('div', undefined, 'connection-footer');
  dialog.append(header, body, error, footer);
  document.body.append(dialog);
  let controller = null;
  let timer = null;
  let record = null;
  let signature = '';
  let draft = null;
  let panel = null;
  let panelController = null;
  const stateNames = {
    queued: 'Preparing connection', creating: 'Preparing connection',
    unconfirmed: 'Connection needs checking', awaiting_authorization: 'Sign-in needed',
    authenticating: 'Checking sign-in', discovering: 'Finding accounts and meters',
    ready: 'Choose accounts and meters', connected: 'Connected',
    needs_authentication: 'Sign-in needs attention', unavailable: 'Connection unavailable',
  };
  const issues = {
    authentication_required: 'The Bayou API key needs attention in the app configuration.',
    access_denied: 'Bayou has not granted access to this connection.',
    rate_limited: 'Bayou is limiting requests. Wait a moment before checking again.',
    provider_unavailable: 'Bayou could not be reached. Saved connection details are safe.',
    invalid_response: 'Bayou returned details that could not be verified.',
    customer_not_found: 'This customer could not be found in the configured Bayou workspace.',
    process_interrupted: 'Setup was interrupted. Check the existing connection before continuing.',
  };
  const serviceErrors = {
    bayou_not_configured: 'Add a Bayou API key in the app configuration to connect a utility.',
    bayou_authentication_required: 'Bayou rejected the API key. Check it in the app configuration.',
    bayou_access_denied: issues.access_denied,
    bayou_rate_limited: issues.rate_limited,
    bayou_unavailable: issues.provider_unavailable,
    bayou_invalid_response: issues.invalid_response,
    bayou_authorization_link_unavailable: 'Bayou did not return a usable sign-in link. Check status and try again.',
  };
  async function request(path, options = {}) {
    const active = controller;
    if (!active || !dialog.open) throw new DOMException('Connection closed', 'AbortError');
    const result = await api(path, { ...options, signal: active.signal });
    if (active !== controller || active.signal.aborted || !dialog.open) throw new DOMException('Connection closed', 'AbortError');
    return result;
  }
  function endpoint(action = '') {
    return `v1/connections/${encodeURIComponent(record.id)}${action ? `/${action}` : ''}`;
  }
  function report(failure) {
    if (failure.name === 'AbortError') return;
    error.textContent = failure.status === 403 ? 'A Home Assistant administrator is required to manage utility connections.'
      : failure.status === 409 ? 'This connection changed. Check its status before continuing.'
      : failure.status === 400 || failure.status === 422 ? 'These connection details could not be accepted. Check the utility and selected meters.'
      : failure.status === 503 ? (Object.hasOwn(serviceErrors, failure.code)
        ? serviceErrors[failure.code]
        : 'Utility connections are temporarily unavailable. Try again shortly.')
      : failure.message;
    if (failure.status === 401) onError(failure);
  }
  function action(text, handler, quiet = false) {
    const button = node('button', text, quiet ? 'quiet' : undefined);
    button.type = 'button';
    button.addEventListener('click', async () => {
      button.disabled = true;
      error.textContent = '';
      try { await handler(); }
      catch (failure) { report(failure); }
      finally { button.disabled = false; }
    });
    return button;
  }
  function stop() {
    clearTimeout(timer);
    controller?.abort();
    controller = null;
  }
  function poll() {
    clearTimeout(timer);
    if (!dialog.open || document.hidden || !record || !['queued', 'creating', 'awaiting_authorization', 'authenticating', 'discovering'].includes(record.state)) return;
    timer = setTimeout(async () => {
      try {
        const next = await request(endpoint('refresh'), { body: {} });
        if (!dialog.open) return;
        renderRecord(next);
      } catch (failure) { report(failure); }
    }, 3000);
  }
  function renderRecord(next) {
    record = next;
    const nextSignature = JSON.stringify([next.state, next.issue, next.accounts, next.auth_status, next.linked_account_ids]);
    if (nextSignature === signature) { poll(); return; }
    signature = nextSignature;
    footer.replaceChildren();
    title.textContent = next.name;
    const steps = node('ol', undefined, 'connection-steps');
    steps.setAttribute('aria-label', 'Connection progress');
    const step = next.state === 'connected' ? 3 : next.state === 'ready' ? 2 : 1;
    ['Choose utility', 'Sign in', 'Choose meters'].forEach((text, index) => {
      const item = node('li', text, index < step ? 'complete' : '');
      if (step < 3 && index === step) item.setAttribute('aria-current', 'step');
      steps.append(item);
    });
    const status = node('p', stateNames[next.state] ?? 'Checking connection', 'connection-status');
    status.setAttribute('role', 'status');
    const description = node('p', `${next.utility_name} · Bayou`, 'muted');
    body.replaceChildren(steps, status, description);
    if (next.issue) body.append(node('p', issues[next.issue] ?? 'Check the connection status before continuing.', 'connection-note'));
    if (next.state === 'connected') {
      body.append(node('p', 'The selected accounts and meters are saved. Bills and usage will appear after a sync.'));
      footer.append(action('Go to sync settings', async () => { dialog.close(); await connected(); }));
    } else if (next.state === 'ready') {
      renderMeters(next);
    } else {
      if (['awaiting_authorization', 'needs_authentication', 'authenticating'].includes(next.state)) {
        body.append(node('p', 'Bayou stores your utility login to keep data in sync. Home Assistant never receives your password. Return here when finished.'));
        const linkArea = node('div', undefined, 'connection-link');
        linkArea.append(action('Sign in on Bayou', async () => {
          // Open during the click so delayed authorization cannot lose the gesture.
          const tab = openBayouTab();
          try {
            const authorization = await request(endpoint('authorization'));
            const url = new URL(authorization.url);
            if (url.protocol !== 'https:' || url.hostname !== 'bayou.energy' || url.port || url.username || url.password) throw new Error('The sign-in link could not be verified.');
            if (tab?.navigate(url)) return;
            const link = node('a', 'Continue on Bayou ↗', 'connection-primary');
            link.href = url.href;
            link.target = '_blank';
            link.rel = 'noopener noreferrer';
            link.referrerPolicy = 'no-referrer';
            linkArea.replaceChildren(link);
            link.focus();
          } catch (failure) {
            tab?.close();
            throw failure;
          }
        }));
        body.append(linkArea);
      } else if (next.state === 'discovering') {
        body.append(node('p', 'Sign-in succeeded. Bayou is discovering this utility’s accounts and meters. This can take a few minutes.'));
      } else if (next.state === 'unconfirmed') {
        body.append(node('p', 'The previous request may have reached Bayou. Continuing checks the same saved customer before sending any new request.'));
      } else if (['queued', 'creating'].includes(next.state)) {
        body.append(node('p', 'Setup is saved and continues if this window closes.'));
      }
      footer.append(action('Check status', async () => {
        renderRecord(await request(endpoint('refresh'), { body: {} }));
      }, true));
      if (next.state === 'unconfirmed' && !next.customer_id) footer.append(action('Continue saved setup', async () => {
        renderRecord(await request(endpoint('resume'), { body: {} }));
      }));
    }
    poll();
  }
  function renderMeters(value) {
    body.append(node('p', 'Choose accounts for this deployment and the meters to use for usage. Bills cover each selected account in full, including its other meters. Nothing is selected automatically.'));
    const form = node('form', undefined, 'connection-meter-form');
    form.id = 'connection-meter-selection';
    const choices = [];
    for (const account of value.accounts) {
      const group = node('fieldset');
      group.append(node('legend', `Account ending ${account.id.slice(-4)}`));
      for (const meter of account.meters) {
        const label = node('label', undefined, 'connection-choice');
        const input = node('input');
        input.type = 'checkbox';
        const name = meter.kind === 'electricity' ? 'Electricity' : meter.kind === 'gas' ? 'Gas' : 'Utility';
        const text = node('span');
        text.append(node('strong', name), node('small', `Meter ending ${meter.id.slice(-4)}`));
        label.append(input, text);
        group.append(label);
        choices.push({ input, account: account.id, meter: meter.id });
      }
      form.append(group);
    }
    const submit = node('button', 'Connect selected meters');
    submit.type = 'submit';
    submit.setAttribute('form', form.id);
    submit.disabled = true;
    form.addEventListener('change', () => { submit.disabled = !choices.some((choice) => choice.input.checked); });
    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      const selected = new Map();
      for (const choice of choices.filter((choice) => choice.input.checked)) {
        if (!selected.has(choice.account)) selected.set(choice.account, []);
        selected.get(choice.account).push(choice.meter);
      }
      if (!selected.size) return;
      submit.disabled = true;
      error.textContent = '';
      const active = controller;
      try {
        const result = await request(endpoint('bind'), { body: { accounts: [...selected].map(([account, meters]) => ({ account, meters })) } });
        if (!dialog.open) return;
        await updated();
        if (controller !== active || !dialog.open) return;
        renderRecord(result);
        refreshPanel();
      } catch (failure) { report(failure); submit.disabled = false; }
    });
    footer.replaceChildren(submit);
    body.append(form);
  }
  async function renderStart() {
    title.textContent = 'Connect utility';
    body.replaceChildren(node('p', 'Loading supported utilities…', 'muted'));
    const values = await request('v1/providers/bayou/utilities', {});
    if (!dialog.open) return;
    const form = node('form', undefined, 'connection-start-form');
    form.id = 'connection-start';
    const provider = node('div', undefined, 'connection-provider');
    provider.append(node('strong', 'Bayou'), node('span', 'API key configured', 'badge good'));
    form.append(provider, node('p', 'Connect electricity or gas using your utility’s own sign-in, hosted by Bayou.', 'muted'));
    const utilityLabel = node('div', undefined, 'form-field');
    const utility = node('select');
    utility.id = 'connection-utility';
    const utilityCaption = node('label', 'Utility');
    utilityCaption.htmlFor = utility.id;
    utility.required = true;
    utility.append(new Option('Choose your utility', ''));
    for (const item of values.items) utility.append(new Option(item.name, item.id));
    utilityLabel.append(utilityCaption, utility);
    const nameLabel = node('div', undefined, 'form-field');
    const name = node('input');
    name.id = 'connection-display-name';
    const nameCaption = node('label', 'Display name (optional)');
    nameCaption.htmlFor = name.id;
    name.type = 'text'; name.maxLength = 100; name.autocomplete = 'off';
    name.placeholder = 'For example, Electricity';
    nameLabel.append(nameCaption, name);
    const currencyLabel = node('div', undefined, 'form-field');
    const currency = node('select');
    currency.id = 'connection-currency';
    const currencyCaption = node('label', 'Billing currency');
    currencyCaption.htmlFor = currency.id;
    currency.append(new Option('USD · US dollar', 'USD'), new Option('CAD · Canadian dollar', 'CAD'));
    currencyLabel.append(currencyCaption, currency);
    const policy = node('div', undefined, 'connection-policy');
    policy.append(node('strong', 'Uses your Bayou workspace settings'), node('p', 'Bayou defaults unlock the last 12 billing periods, future bills, and both electric and gas meters. Your workspace settings govern.'), node('p', 'Up to 10 meters can be unlocked free. Unlocking an 11th requires billing setup, then all unlocked data is billed, including the first 10, at $2 per meter per billing period. Local account and meter selection happens afterward and does not limit Bayou’s charges.'));
    const pricing = node('a', 'Review Bayou pricing and data settings ↗');
    pricing.href = 'https://docs.bayou.energy/docs/pricing-overview';
    pricing.target = '_blank'; pricing.rel = 'noopener noreferrer'; pricing.referrerPolicy = 'no-referrer';
    policy.append(pricing);
    const consentLabel = node('label', undefined, 'connection-choice');
    const consent = node('input'); consent.type = 'checkbox'; consent.required = true;
    consentLabel.append(consent, node('span', 'Use my Bayou workspace’s data and billing settings for this connection.'));
    const submit = node('button', draft ? 'Check saved setup' : 'Continue to sign-in');
    submit.type = 'submit';
    submit.setAttribute('form', form.id);
    const updateReadiness = () => { submit.disabled = !draft && (!utility.value || !consent.checked); };
    utility.addEventListener('change', updateReadiness);
    consent.addEventListener('change', updateReadiness);
    if (draft) {
      utility.value = draft.utility_id; name.value = draft.name ?? ''; currency.value = draft.currency; consent.checked = true;
      for (const control of [utility, name, currency, consent]) control.disabled = true;
      form.append(node('p', 'A previous setup request was not confirmed. This checks the same request, so retrying cannot create another customer.', 'connection-note'));
    }
    form.append(utilityLabel, nameLabel, currencyLabel, policy, consentLabel);
    footer.replaceChildren(submit);
    updateReadiness();
    form.addEventListener('submit', async (event) => {
      event.preventDefault();
      if (!form.reportValidity()) return;
      if (!draft) draft = { provider: 'bayou', request_id: crypto.randomUUID(), utility_id: utility.value, name: name.value.trim() || null, currency: currency.value, accept_provider_settings: true };
      submit.disabled = true;
      for (const control of [utility, name, currency, consent]) control.disabled = true;
      error.textContent = '';
      try {
        const result = await request('v1/connections', { body: draft });
        draft = null;
        if (!dialog.open) return;
        renderRecord(result);
        refreshPanel();
      } catch (failure) { report(failure); submit.textContent = 'Check saved setup'; submit.disabled = false; }
    });
    body.replaceChildren(form);
    (draft ? submit : utility).focus();
  }
  async function open(id) {
    if (dialog.open) return;
    stop();
    controller = new AbortController();
    record = null; signature = ''; error.textContent = '';
    title.textContent = 'Connect utility';
    footer.replaceChildren();
    body.replaceChildren(node('p', 'Loading connection…', 'muted'));
    dialog.showModal();
    try {
      if (id) renderRecord(await request(`v1/connections/${encodeURIComponent(id)}`, {}));
      else {
        if (draft) {
          const saved = await request('v1/connections', {});
          const existing = saved.items.find((item) => item.request_id === draft.request_id);
          if (existing) { draft = null; renderRecord(existing); return; }
        }
        await renderStart();
      }
    } catch (failure) { report(failure); }
  }
  async function refreshPanel() {
    if (!panel?.isConnected) return;
    panelController?.abort();
    panelController = new AbortController();
    const current = panel;
    try {
      const result = await api('v1/connections', { signal: panelController.signal });
      if (!current.isConnected) return;
      const list = node('ul', undefined, 'record-list');
      for (const item of result.items) {
        const row = node('li');
        const description = node('div');
        description.append(node('strong', item.name), node('p', `${item.utility_name} · ${stateNames[item.state] ?? 'Check connection'}`));
        row.append(description, action(item.state === 'connected' ? 'Details' : 'Continue setup', () => open(item.id), true));
        list.append(row);
      }
      current.replaceChildren(result.items.length ? list : node('p', 'No hosted utility connections yet.', 'panel-empty'));
    } catch (failure) {
      if (failure.name === 'AbortError' || !current.isConnected) return;
      current.replaceChildren(node('p', failure.status === 403 ? 'A Home Assistant administrator can manage utility connections.' : 'Saved connections could not be loaded. Refresh before starting another setup.', 'panel-empty'));
    }
  }
  function section({ pendingOnly = false } = {}) {
    panelController?.abort();
    const section = node('section', undefined, 'panel settings-section');
    const heading = node('div', undefined, 'section-heading');
    heading.append(node('h2', 'Utility connections'));
    const start = action('Connect utility', () => open());
    start.disabled = !providers().some((provider) => provider.id === 'bayou' && provider.configured);
    if (!pendingOnly) heading.append(start);
    panel = node('div');
    panel.append(node('p', start.disabled ? 'Add a Bayou API key in the app configuration to connect a utility.' : 'Loading saved connections…', 'panel-empty'));
    section.append(heading, panel);
    // The caller mounts synchronously; defer the owned request until then.
    queueMicrotask(refreshPanel);
    return section;
  }
  close.addEventListener('click', () => dialog.close());
  dialog.addEventListener('close', () => { stop(); body.replaceChildren(); footer.replaceChildren(); refreshPanel(); });
  document.addEventListener('visibilitychange', () => {
    clearTimeout(timer);
    if (!document.hidden) poll();
  });
  window.addEventListener('pagehide', () => { stop(); panelController?.abort(); });
  return { open, section };
}
