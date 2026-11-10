import { mountAppShell, setNotice } from './shell.js';

const form = document.getElementById('login-form');
const token = document.getElementById('token');
const button = form.querySelector('button');
const notice = document.getElementById('notice');
const errorNotice = document.getElementById('error');

mountAppShell({
  name: 'Utilities',
  header: document.getElementById('header'),
  footer: document.getElementById('footer'),
  sourceUrl: 'https://github.com/jessfraz/ha-utilities',
  status: 'Native sign-in',
});

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  if (!form.reportValidity() || button.disabled) return;
  button.disabled = true;
  button.textContent = 'Signing in…';
  setNotice(notice, '');
  errorNotice.textContent = '';
  try {
    const response = await fetch('login', {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json', 'X-Utilities-Request': '1' },
      body: JSON.stringify({ token: token.value }),
    });
    if (!response.ok) {
      errorNotice.textContent = response.status === 401 ? 'This token was not accepted. Check it and try again.' : 'Sign-in could not be completed. Try again.';
      token.focus();
      return;
    }
    form.reset();
    const view = /^#(?:overview|bills|collections|usage|settings)$/.test(location.hash) ? location.hash : '';
    location.replace(`./${view}`);
  } catch {
    errorNotice.textContent = 'Utilities could not be reached. Check the connection and try again.';
  } finally {
    button.disabled = false;
    button.textContent = 'Sign in';
  }
});
