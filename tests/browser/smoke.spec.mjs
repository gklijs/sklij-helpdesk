import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';

import { login, trigger } from './helpers.mjs';

test('OIDC, tenant ticket journey, priority-only live update, reconnect, and expiry', async ({ browser, request }) => {
  const customerContext = await browser.newContext();
  const staffContext = await browser.newContext();
  const customer = await customerContext.newPage();
  const staff = await staffContext.newPage();
  const observerContext = await browser.newContext();
  const observer = await observerContext.newPage();
  let prioritySubscribed = false;
  await observer.routeWebSocket('ws://localhost:8080/graphql', socket => {
    const upstream = socket.connectToServer();
    socket.onMessage(raw => {
      const message = JSON.parse(raw);
      if (message.type === 'subscribe') {
        prioritySubscribed = message.payload.variables.eventTypes.includes('TicketPriorityChanged');
        // This staff session receives only priority notifications. A reply
        // event from the same macro cannot conceal a missing priority feed.
        message.payload.variables.eventTypes = prioritySubscribed ? ['TicketPriorityChanged'] : ['TicketCreated'];
      }
      upstream.send(JSON.stringify(message));
    });
    upstream.onMessage(raw => socket.send(raw));
  });
  const errors = [];
  customer.on('pageerror', error => errors.push(error.message));
  staff.on('pageerror', error => errors.push(error.message));
  try {
    await login(customer, 'customer');
    await login(staff, 'staff');
    await expect(staff.getByRole('combobox', { name: 'Company' }).locator('option')).toHaveCount(2);
    const title = `Browser ticket ${Date.now()}`;
    await customer.getByPlaceholder('Title', { exact: true }).fill(title);
    await customer.getByPlaceholder('Description', { exact: true }).fill('A real customer ticket');
    await customer.getByRole('button', { name: 'Create', exact: true }).click();
    const customerRow = customer.getByRole('row').filter({ hasText: title });
    const staffRow = staff.getByRole('row').filter({ hasText: title });
    await expect(customerRow).toContainText('open');
    await expect(staffRow).toContainText('open');
    // Both readable text and tenant routing must work after the 0.3 upgrade.
    const requests = [];
    staff.on('request', r => { if (r.url().endsWith('/v1/helpdesk/tickets')) requests.push(r.postData() || ''); });
    await staffRow.getByRole('button', { name: 'Assign to me', exact: true }).click();
    await expect(customerRow).toContainText('in progress');
    await staff.getByPlaceholder('Ask the customer something...').fill('Which browser?');
    await staff.getByRole('button', { name: 'Request info', exact: true }).click();
    await expect(customerRow).toContainText('waiting on customer');
    await customer.getByPlaceholder('Your reply...').fill('Chromium');
    await customer.getByRole('button', { name: 'Reply', exact: true }).click();
    await expect(staffRow).toContainText('in progress');
    await login(observer, 'staff');
    const seed = JSON.parse(readFileSync(new URL('.stack.json', import.meta.url)));
    // Capture the ticket id from the observable row's backing read response.
    const token = await staff.evaluate(() => localStorage.getItem('skilj_helpdesk_token'));
    const result = await request.post('http://localhost:8080/graphql', {
      headers: { authorization: `Bearer ${token}` },
      data: { query: 'query { projection(boundedContext: "browser_acme", name: "CompanyActiveTickets", key: "acme") { ... on browser_acme_CompanyActiveTickets { tickets } } }' },
    }).then(r => r.json());
    const tickets = JSON.parse(result.data.projection.tickets);
    const ticketId = Object.keys(tickets).at(-1);
    await trigger(request, seed.acme.createMacro, { macro_id: 'priority-only', company_id: 'acme', name: 'Urgent only', staff_id: 'browser-staff', body: 'Your ticket is now urgent', status: null, priority: 'urgent' });
    await trigger(request, seed.acme.applyMacro, { ticket_id: ticketId, macro_id: 'priority-only', staff_id: 'browser-staff', company_id: 'acme' });
    await expect(staffRow).toContainText('urgent');
    await expect(customerRow).toContainText('urgent');
    await expect(observer.getByRole('row').filter({ hasText: title })).toContainText('urgent');
    expect(prioritySubscribed).toBe(true);
    expect(requests.some(body => JSON.parse(body).companyId === 'acme' && JSON.parse(body).waitForSequence !== null)).toBe(true);
    // Drop the network while another session changes the ticket. Resuming
    // must replay the missed event and re-read the projection.
    await customerContext.setOffline(true);
    await staffRow.getByRole('button', { name: 'Resolve', exact: true }).click();
    await expect(staffRow).toContainText('resolved');
    await customerContext.setOffline(false);
    await expect(customerRow).toContainText('resolved');
    await staff.getByRole('combobox', { name: 'Company' }).selectOption('globex');
    await expect(staff.getByRole('row').filter({ hasText: title })).toHaveCount(0);
    await staff.getByRole('combobox', { name: 'Company' }).selectOption('acme');
    await expect(staff.getByRole('row').filter({ hasText: title })).toContainText('resolved');
    // An expired stored session must lead to a usable real login page.
    await customer.evaluate(() => {
      const token = localStorage.getItem('skilj_helpdesk_token').split('.');
      token[1] = btoa(JSON.stringify({ sub: 'expired', exp: 1 })).replaceAll('=', '');
      localStorage.setItem('skilj_helpdesk_token', token.join('.'));
    });
    await customer.reload();
    await expect(customer.getByRole('button', { name: 'Log in', exact: true })).toBeVisible();
    await login(customer, 'customer');
    expect(errors).toEqual([]);
  } finally {
    await customerContext.close();
    await staffContext.close();
    await observerContext.close();
  }
});
