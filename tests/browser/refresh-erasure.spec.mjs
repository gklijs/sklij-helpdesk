import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { login, trigger } from './helpers.mjs';
const seed = () => JSON.parse(readFileSync(new URL('.stack.json', import.meta.url)));
async function create(request, ticket, requester, title) {
  await trigger(request, seed().acme.createTicket, {
    ticket_id: ticket, company_id: 'acme', requester_id: requester,
    logged_by_staff_id: null, title, description: `Description for ${title}`, priority: 'low',
  });
}
async function macro(request, ticket, id, priority, message, status = null) {
  const { acme } = seed();
  await trigger(request, acme.createMacro, { macro_id: id, company_id: 'acme', name: id,
    staff_id: 'browser-staff', body: message, status, priority });
  await trigger(request, acme.applyMacro, { ticket_id: ticket, macro_id: id,
    staff_id: 'browser-staff', company_id: 'acme' });
}

test('late refreshes and stale failures cannot revert newer queue or conversation', async ({ page, request }) => {
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  const ticket = `race-${Date.now()}`;
  const title = `Refresh race ${ticket}`;
  await create(request, ticket, `requester-${ticket}`, title);
  await trigger(request, seed().acme.assignTicket, { ticket_id: ticket, staff_id: 'browser-staff' });
  await login(page, 'staff');
  const row = page.getByRole('row').filter({ has: page.getByRole('cell', { name: title, exact: true }) });
  await expect(row).toContainText('low');
  await row.locator('xpath=following-sibling::tr[1]').getByRole('button', { name: 'Show details' }).click();
  // Hold a real old response, then commit a new priority and conversation.
  let held;
  let release;
  let completed;
  const captured = new Promise(resolve => { held = resolve; });
  const released = new Promise(resolve => { release = resolve; });
  const finished = new Promise(resolve => { completed = resolve; });
  let delay = true;
  await page.route('**/v1/helpdesk/tickets', async route => {
    if (!delay) return route.continue();
    delay = false;
    const response = await route.fetch();
    held();
    await released;
    try { await route.fulfill({ response }); } catch { /* intentionally aborted */ }
    completed();
  });
  await page.getByRole('button', { name: 'Refresh tickets' }).click();
  await captured;
  await macro(request, ticket, `macro-${ticket}`, 'urgent', `Latest conversation ${ticket}`, 'waiting_on_customer');
  await expect(row).toContainText('urgent');
  await expect(row).toContainText('waiting on customer');
  await expect(page.locator('.messages li').filter({ hasText: `Latest conversation ${ticket}` })).toBeVisible();
  release();
  await finished;
  await expect(row).toContainText('urgent');
  await expect(row).toContainText('waiting on customer');
  await expect(page.locator('.messages li').filter({ hasText: `Latest conversation ${ticket}` })).toBeVisible();
  await page.unroute('**/v1/helpdesk/tickets');
  // A stale error must not clear a newer successful page either.
  let fail;
  let failureCaptured;
  const failing = new Promise(resolve => { fail = resolve; });
  const sawFailure = new Promise(resolve => { failureCaptured = resolve; });
  delay = true;
  await page.route('**/v1/helpdesk/tickets', async route => {
    if (!delay) return route.continue();
    delay = false;
    failureCaptured();
    await failing;
    try { await route.fulfill({ status: 500, body: 'stale failure' }); } catch { /* aborted */ }
  });
  await page.getByRole('button', { name: 'Refresh tickets' }).click();
  await sawFailure;
  const refreshed = page.waitForResponse(r => r.url().endsWith('/v1/helpdesk/tickets') && r.ok());
  await page.getByRole('button', { name: 'Refresh tickets' }).click();
  await refreshed;
  fail();
  await expect(row).toContainText('urgent');
  await expect(row).toContainText('waiting on customer');
  await expect(page.locator('p.error').first()).toBeEmpty();
  expect(errors).toEqual([]);
});

test('refresh removes loaded erased text and never renders ciphertext or alters base64 text', async ({ page, request }) => {
  const id = `erase-${Date.now()}`;
  const requester = `customer-${id}`;
  const title = `Private title ${id}`;
  const message = `Private reply ${id}`;
  await create(request, id, requester, title);
  await create(request, `${id}-other`, `${requester}-other`, 'SGVsbG8=');
  await macro(request, id, `macro-${id}`, 'high', message);
  await trigger(request, seed().acme.assignTicket, { ticket_id: id, staff_id: 'browser-staff' });
  await trigger(request, seed().acme.resolveTicket, { ticket_id: id });
  await trigger(request, seed().acme.rateTicket, { ticket_id: id, requester_id: requester, rating: 5, comment: `Private rating ${id}` });
  await login(page, 'staff');
  const row = page.getByRole('row').filter({ has: page.getByRole('cell', { name: title, exact: true }) });
  await expect(row).toBeVisible();
  // Detail controls live in the sibling row.
  await row.locator('xpath=following-sibling::tr[1]').getByRole('button', { name: 'Show details' }).click();
  await expect(page.locator('.messages li').filter({ hasText: message })).toBeVisible();
  await expect(page.locator('.rating-given').filter({ hasText: `Private rating ${id}` })).toBeVisible();
  const observed = [];
  page.on('response', async response => {
    if (response.url().endsWith('/v1/helpdesk/tickets') && response.ok()) {
      observed.push(await response.json());
    }
  });
  const response = await request.post('http://localhost:8080/graphql', {
    headers: { authorization: `Bearer ${seed().operatorToken}` },
    data: { query: `mutation { forgetSubject(boundedContext: "browser_acme", subjectKey: "customer", subjectValue: "${requester}") { status } }` },
  }).then(r => r.json());
  expect(response.errors).toBeUndefined();
  // Read the retained ciphertext through the raw projection API so the UI
  // assertion rejects the actual encrypted values, as well as prior text.
  const raw = await request.post('http://localhost:8080/graphql', {
    headers: { authorization: `Bearer ${seed().operatorToken}` },
    data: { query: `query { projection(boundedContext: "browser_acme", name: "CustomerTickets", key: "${requester}") { ... on browser_acme_CustomerTickets { tickets } } }` },
  }).then(r => r.json());
  expect(raw.errors).toBeUndefined();
  const encrypted = JSON.parse(raw.data.projection.tickets)[id];
  const ciphertexts = [encrypted.title, encrypted.description, encrypted.rating_comment,
    ...encrypted.messages.map(message => message.text)].filter(Boolean);
  expect(ciphertexts).toHaveLength(4);
  await page.getByRole('button', { name: 'Refresh tickets' }).click();
  await expect(page.getByText(title, { exact: true })).toHaveCount(0);
  await expect(page.getByText(`Description for ${title}`, { exact: true })).toHaveCount(0);
  await expect(page.locator('.messages li').filter({ hasText: message })).toHaveCount(0);
  await expect(page.getByRole('cell', { name: 'Content unavailable', exact: true })).toBeVisible();
  await expect(page.locator('.rating-given').filter({ hasText: `Private rating ${id}` })).toHaveCount(0);
  await expect(page.getByRole('cell', { name: 'SGVsbG8=', exact: true })).toBeVisible();
  for (const ciphertext of ciphertexts) await expect(page.locator('body')).not.toContainText(ciphertext);
  expect(observed.some(p => p.contents[id]?.available === false)).toBe(true);
  expect(observed.every(p => JSON.stringify(p.contents[id] || {}).includes(title) === false)).toBe(true);
  await page.reload();
  await expect(page.getByRole('cell', { name: 'Content unavailable', exact: true })).toBeVisible();
});
