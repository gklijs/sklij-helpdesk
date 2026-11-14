import { defineConfig } from '@playwright/test';
import { existsSync } from 'node:fs';

const executablePath = process.env.PLAYWRIGHT_USE_BUNDLED_CHROMIUM ? undefined : process.env.CHROMIUM_PATH ||
  ['/usr/bin/chromium', '/usr/bin/chromium-browser', '/snap/bin/chromium']
    .find(existsSync);

export default defineConfig({
  testDir: '.',
  testMatch: '*.spec.mjs',
  workers: 1,
  timeout: 90_000,
  expect: { timeout: 20_000 },
  use: {
    baseURL: 'http://127.0.0.1:8081',
    browserName: 'chromium',
    launchOptions: executablePath ? { executablePath, args: ['--no-sandbox'] } : {},
    trace: 'retain-on-failure',
  },
  webServer: {
    command: 'node stack.mjs',
    url: 'http://127.0.0.1:8081',
    timeout: 180_000,
    reuseExistingServer: false,
    gracefulShutdown: { signal: 'SIGTERM', timeout: 15_000 },
  },
});
