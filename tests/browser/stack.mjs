// Isolated, disposable PostgreSQL + real Dex + compiled application. No
// production database is used. The operator is promoted only in this fixture
// to provision tenants through the same GraphQL API as the provisioner.
import { spawn, execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync, mkdtempSync, rmSync, existsSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { resolve, extname } from 'node:path';

const root = resolve(import.meta.dirname, '../..');
const temp = mkdtempSync(`${tmpdir()}/helpdesk-browser-`);
const prefix = `helpdesk-browser-${process.pid}`;
const containers = [];
let backend;
let frontend;
let log = '';
let stopped = false;
function docker(...args) { return execFileSync('docker', args, { encoding: 'utf8' }).trim(); }
async function cleanup() {
  if (stopped) return;
  stopped = true;
  frontend?.close();
  backend?.kill('SIGTERM');
  if (backend && backend.exitCode === null) {
    await Promise.race([
      new Promise(r => backend.once('exit', r)),
      new Promise(r => setTimeout(r, 10_000)),
    ]);
    if (backend.exitCode === null) backend.kill('SIGKILL');
  }
  for (const name of containers.reverse()) {
    try { docker('rm', '-f', name); } catch { /* already removed */ }
  }
  rmSync(resolve(import.meta.dirname, '.stack.json'), { force: true });
  rmSync(temp, { recursive: true, force: true });
}
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, async () => { await cleanup(); process.exit(0); });
}
async function waitFor(check, description) {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    try { const result = await check(); if (result) return result; } catch { /* starting */ }
    if (backend && backend.exitCode !== null) throw new Error(`backend exited: ${log}`);
    await new Promise(r => setTimeout(r, 250));
  }
  throw new Error(`timed out waiting for ${description}\n${log}`);
}
try {
  for (const path of ['target/debug/server', 'frontend/dist/index.html']) {
    if (!existsSync(resolve(root, path))) throw new Error(`Build ${path} first; see README.md`);
  }
  const pg = `${prefix}-pg`;
  containers.push(pg);
  docker('run', '-d', '--name', pg, '-e', 'POSTGRES_PASSWORD=postgres',
    '-p', '127.0.0.1::5432', 'postgres:16-alpine');
  const port = docker('port', pg, '5432/tcp').split(':').at(-1);
  await waitFor(() => {
    try { return docker('exec', pg, 'pg_isready', '-U', 'postgres').includes('accepting'); } catch { return false; }
  }, 'PostgreSQL');
  const dexConfig = readFileSync(resolve(root, 'dex/config.yaml'), 'utf8')
    .replace('http: 127.0.0.1:5556', 'http: 0.0.0.0:5556');
  writeFileSync(`${temp}/dex.yaml`, dexConfig);
  const dex = `${prefix}-dex`;
  containers.push(dex);
  docker('run', '-d', '--name', dex, '-p', '127.0.0.1:5556:5556',
    '-v', `${temp}/dex.yaml:/etc/dex/config.yaml:ro`,
    'ghcr.io/dexidp/dex:v2.45.1', 'dex', 'serve', '/etc/dex/config.yaml');
  await waitFor(async () => (await fetch('http://127.0.0.1:5556/dex/.well-known/openid-configuration')).ok, 'Dex');
  backend = spawn(resolve(root, 'target/debug/server'), [], {
    cwd: root,
    env: { ...process.env,
      DATABASE_URL: `postgres://postgres:postgres@127.0.0.1:${port}/postgres`,
      ENCRYPTION_KEY_FILE: `${temp}/keys.json`, OIDC_ISSUER_URL: 'http://127.0.0.1:5556/dex',
      TICKET_ROUTING: 'tenant', PORT: '8080', RUST_LOG: 'warn', SEED_DEMO_TRAFFIC: '0',
    }, stdio: ['ignore', 'pipe', 'pipe'],
  });
  backend.stdout.on('data', chunk => { log += chunk; writeFileSync(`${temp}/server.log`, log); });
  backend.stderr.on('data', chunk => { log += chunk; });
  await waitFor(async () => log.includes('listening on') && (await fetch('http://127.0.0.1:8080/')).status, 'application');
  const tokens = Object.fromEntries([...log.matchAll(/^  (\w+): ([a-f0-9]{32}\.[a-f0-9]{64})$/gm)].map(m => [m[1], m[2]]));
  async function trigger(name, payload, credential = tokens[name]) {
    const response = await fetch('http://127.0.0.1:8080/v1/commands/trigger', {
      method: 'POST', headers: { authorization: `Bearer ${credential}`, 'content-type': 'application/json' },
      body: JSON.stringify({ payload }),
    });
    const body = await response.json();
    if (!body.accepted) throw new Error(`${name}: ${JSON.stringify(body)}`);
    return body;
  }
  docker('exec', pg, 'psql', '-U', 'postgres', '-v', 'ON_ERROR_STOP=1', '-c',
    "UPDATE roles SET superadmin = true WHERE name = 'operator'");
  const login = await fetch('http://127.0.0.1:5556/dex/token', {
    method: 'POST', body: new URLSearchParams({ grant_type: 'password',
      client_id: 'skilj-helpdesk-frontend', username: 'operator@acme.example',
      password: 'operator-demo-pw', scope: 'openid email profile' }),
  }).then(r => r.json());
  if (!login.id_token) throw new Error(`operator login: ${JSON.stringify(login)}`);
  async function graphql(query, variables = {}) {
    const result = await fetch('http://localhost:8080/graphql', {
      method: 'POST', headers: { authorization: `Bearer ${login.id_token}`, 'content-type': 'application/json' },
      body: JSON.stringify({ query, variables }),
    }).then(r => r.json());
    if (result.errors) throw new Error(JSON.stringify(result.errors));
    return result.data;
  }
  const contexts = await graphql('query { boundedContexts { name accessMappings { role { id name } } } }');
  const operator = contexts.boundedContexts.find(c => c.name === 'helpdesk').accessMappings.find(m => m.role.name === 'operator').role.id;
  const seed = {};
  for (const company of ['acme', 'globex']) {
    await trigger('SignUpCompany', { company_id: company, name: company, contact_email: `ops@${company}.example` });
    const tenant = `browser_${company}`;
    await graphql('mutation($name: String!, $role: ID!) { createBoundedContextFromTemplate(template: "helpdesk", name: $name, roleId: $role, level: ADMIN, canReadSensitive: false) { name } }', { name: tenant, role: operator });
    await trigger('RecordCompanyTenant', { company_id: company, tenant_name: tenant });
    async function mint(command) {
      const { createCommandToken: token } = await graphql('mutation($bc: String!, $command: String!) { createCommandToken(boundedContext: $bc, commandTypeName: $command) { id secret } }', { bc: tenant, command });
      return `${token.id}.${token.secret}`;
    }
    await trigger('RecordTenantLifecycle', { company_id: company, status: 'trialing', source_event_type: 'CompanySignedUp' }, await mint('RecordTenantLifecycle'));
    seed[company] = { context: tenant, applyMacro: await mint('ApplyMacro'), createMacro: await mint('CreateMacro') };
  }
  // Only fixture command credentials are written; removed with this process.
  writeFileSync(resolve(import.meta.dirname, '.stack.json'), JSON.stringify(seed), { mode: 0o600 });
  frontend = createServer((req, res) => {
    const path = new URL(req.url, 'http://localhost').pathname;
    const asset = resolve(root, 'frontend/dist', `.${path}`);
    const file = asset.startsWith(resolve(root, 'frontend/dist') + '/') && existsSync(asset) && extname(asset) ? asset : resolve(root, 'frontend/dist/index.html');
    const type = { '.wasm': 'application/wasm', '.js': 'application/javascript', '.css': 'text/css', '.html': 'text/html' }[extname(file)] || 'application/octet-stream';
    res.writeHead(200, { 'content-type': type });
    res.end(readFileSync(file));
  }).listen(8081, '127.0.0.1');
} catch (error) {
  console.error(error);
  await cleanup();
  process.exit(1);
}
