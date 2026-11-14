import { expect } from '@playwright/test';
export async function login(page, kind) {
  await page.goto('/login');
  await page.getByRole('button', { name: 'Log in', exact: true }).click();
  await page.getByPlaceholder('Email address').fill(kind === 'customer' ? 'customer@acme.example' : 'lead@acme.example');
  await page.getByPlaceholder('Password').fill(kind === 'customer' ? 'customer-demo-pw' : 'staff-demo-pw');
  await page.getByRole('button', { name: 'Login', exact: true }).click();
  const grant = page.getByRole('button', { name: 'Grant Access' });
  await Promise.race([grant.waitFor().then(() => grant.click()), page.waitForURL('http://127.0.0.1:8081/')]);
  await expect(page.getByText(kind === 'customer' ? /Customer view/ : /Staff view/)).toBeVisible();
  await expect(page.getByText('● Live', { exact: true })).toBeVisible();
}
export async function trigger(request, credential, payload) {
  const response = await request.post('http://localhost:8080/v1/commands/trigger', {
    headers: { authorization: `Bearer ${credential}` }, data: { payload },
  });
  const body = await response.json();
  expect(body.accepted, JSON.stringify(body)).toBe(true);
  return body;
}
