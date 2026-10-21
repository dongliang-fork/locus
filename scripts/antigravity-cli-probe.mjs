// Opt-in P0 probe: real agy, deterministic loopback model, no MCP.
// --host-stdio delegates read to the Rust test's real Locus ToolRegistry.
// --fixture uses a file fixture instead and must never be reported as Locus execution.
import { createServer } from 'node:http';
import { spawn, execFileSync } from 'node:child_process';
import { createInterface } from 'node:readline';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';

const script = fileURLToPath(import.meta.url);
const mode = process.argv[2];
const input = async () => { let data = ''; for await (const chunk of process.stdin) { data += chunk; if (data.length > 65536) throw Error('Input too large'); } return JSON.parse(data); };
async function rpc(path, body, timeout = 15000) {
  const url = process.env.LOCUS_AGY_PROBE_URL, token = process.env.LOCUS_AGY_PROBE_TOKEN;
  if (!/^http:\/\/127\.0\.0\.1:\d+$/.test(url || '') || !token) throw Error('Probe bridge unavailable');
  const response = await fetch(url + path, { method: 'POST', headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' }, body: JSON.stringify(body), signal: AbortSignal.timeout(timeout) });
  if (!response.ok) throw Error(`Bridge rejected request: ${response.status}`);
  return response.json();
}
if (mode === 'hook') {
  try { console.log(JSON.stringify(await rpc('/prepare', await input()))); }
  catch { console.log(JSON.stringify({ decision: 'deny', reason: 'Locus probe bridge unavailable; no tool executed.' })); }
} else if (mode === 'runner' || mode === 'entry') {
  try {
    if (mode === 'entry' && (process.argv.length !== 4 || process.argv[3] !== 'read')) throw Error('Only the fixed probe read is allowed');
    const result = await rpc('/call', { ticket: mode === 'entry' ? process.env.LOCUS_AGY_PROBE_TICKET : process.argv[3] }, 60000);
    console.log(JSON.stringify(result));
    if (result.isError) process.exitCode = 1;
  } catch (error) { console.error(String(error)); process.exitCode = 1; }
} else if (['--host-stdio', '--fixture'].includes(mode)) {
  await main().catch(error => { console.error(String(error)); process.exit(1); });
} else {
  console.error('Usage: bun scripts/antigravity-cli-probe.mjs --host-stdio|--fixture [--binary PATH] [--hook-route]');
  process.exitCode = 2;
}

async function main() {
  const binaryArg = process.argv.indexOf('--binary');
  const binary = resolve(binaryArg >= 0 ? process.argv[binaryArg + 1] : 'ref/antigravity-cli-1.4.2/agy.exe');
  const modelName = 'gemini-3.1-pro-preview';
  const hostTimeout = Number(process.env.LOCUS_AGY_PROBE_HOST_TIMEOUT_MS || 45000);
  if (!Number.isInteger(hostTimeout) || hostTimeout < 100 || hostTimeout > 45000) throw Error('Invalid host timeout');
  if (process.platform !== 'win32') throw Error('P0 managed launcher currently supports Windows only');
  const route = process.argv.includes('--hook-route') ? 'hook' : 'runner';
  const base = process.env.LOCUS_AGY_PROBE_ROOT || (process.platform === 'win32' ? 'E:/LocusTemp' : '/tmp');
  await mkdir(base, { recursive: true });
  const root = await mkdtemp(join(base, 'locus-agy-probe-'));
  const workspace = join(root, 'workspace'), home = join(root, 'home');
  const gemini = join(home, '.gemini'), app = join(gemini, 'antigravity-cli');
  for (const dir of [workspace, join(workspace, '.agents/agents'), app, join(gemini, 'config'), join(home, 'AppData/Roaming'), join(home, 'AppData/Local')]) await mkdir(dir, { recursive: true });
  const secret = `LOCUS_READ_${randomUUID().replaceAll('-', '')}`;
  const fixtureFile = join(workspace, 'proof.txt');
  await writeFile(fixtureFile, secret + '\n');
  const token = randomUUID();
  const entryTicket = randomUUID();
  const tickets = new Map(), scopes = new Map(), pendingHost = new Map();
  const events = [], requests = [], records = [], checks = [], modelTools = new Set();
  let child, executionCount = 0, conversationId, turns = 0, fatal, stage = 0;
  let runDoneResolve;
  const runDone = new Promise(resolve => { runDoneResolve = resolve; });
  const json = (res, status, value) => { res.writeHead(status, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(value)); };
  const emit = value => console.log(JSON.stringify(value));
  const hostLines = mode === '--host-stdio' ? createInterface({ input: process.stdin }) : null;
  hostLines?.on('line', line => {
    try {
      const result = JSON.parse(line);
      if (result.event !== 'tool_result') return;
      const receipt = pendingHost.get(result.id);
      if (!receipt) return;
      pendingHost.delete(result.id);
      clearTimeout(receipt.timer);
      receipt.resolve(result.result);
    } catch (error) { fatal = String(error); runDoneResolve(); }
  });
  async function execute(id) {
    executionCount++;
    if (mode === '--fixture') return { output: await readFile(fixtureFile, 'utf8'), isError: false };
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { pendingHost.delete(id); reject(Error('Locus host timed out; outcome unknown')); }, hostTimeout);
      pendingHost.set(id, { resolve, reject, timer });
      emit({ event: 'tool_request', id, tool: 'read', arguments: { filePath: fixtureFile }, workspace });
    });
  }
  const bridge = createServer(async (req, res) => {
    try {
      if (req.headers.authorization !== `Bearer ${token}`) return json(res, 401, { error: 'Unauthorized' });
      if (req.headers.origin || req.headers.host !== new URL(bridgeUrl).host || req.method !== 'POST') return json(res, 403, { error: 'Invalid request origin' });
      const chunks = []; let size = 0;
      for await (const chunk of req) { size += chunk.length; if (size > 65536) return json(res, 413, { error: 'Too large' }); chunks.push(chunk); }
      const body = JSON.parse(Buffer.concat(chunks).toString('utf8'));
      if (req.url === '/prepare') {
        records.push({ event: 'hook', body });
        if (body.toolCall?.name !== 'run_command' || body.toolCall?.args?.CommandLine !== 'locus-tool read' || !body.conversationId || !Number.isInteger(body.stepIdx)) return json(res, 200, { decision: 'deny', reason: 'Only the Locus probe read operation is allowed.' });
        if (conversationId && body.conversationId !== conversationId) return json(res, 200, { decision: 'deny', reason: 'Conversation mismatch' });
        const scope = `${body.conversationId}:${body.stepIdx}`;
        let ticket = scopes.get(scope);
        if (!ticket) { ticket = randomUUID(); scopes.set(scope, ticket); tickets.set(ticket, { id: scope, outcome: null }); }
        const command = `${process.platform === 'win32' ? '& ' : ''}"${process.execPath}" "${script}" runner ${ticket}`;
        return json(res, 200, { decision: 'auto_approve', overwrite: { CommandLine: command, Cwd: workspace, WaitMsBeforeAsync: 10000, IsDaemon: false } });
      }
      if (req.url === '/call') {
        const receipt = tickets.get(body.ticket);
        if (!receipt) return json(res, 410, { error: 'Unknown ticket' });
        receipt.outcome ??= execute(receipt.id).catch(error => ({ output: String(error), isError: true }));
        const result = await receipt.outcome;
        records.push({ event: 'result', id: receipt.id, result });
        return json(res, 200, result);
      }
      return json(res, 404, { error: 'Unknown operation' });
    } catch (error) { json(res, 400, { error: String(error) }); }
  });
  await new Promise(resolve => bridge.listen(0, '127.0.0.1', resolve));
  const bridgeUrl = `http://127.0.0.1:${bridge.address().port}`;
  const model = createServer(async (req, res) => {
    try {
      const chunks = []; for await (const chunk of req) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString('utf8') || '{}');
      requests.push({ path: req.url, body });
      if (requests.length > 10) throw Error('Model request limit reached; inspect the native tool error');
      if (req.method === 'GET') return json(res, 200, { object: 'list', data: [{ id: 'locus-probe', object: 'model' }] });
      const messages = JSON.stringify(body.messages || body.contents || []);
      const match = messages.match(/LOCUS_READ_[a-f0-9]{32}/);
      const names = (body.tools || []).flatMap(tool => tool.functionDeclarations?.map(item => item.name) || [tool.function?.name || tool.name]).filter(Boolean);
      for (const name of names) modelTools.add(name);
      if (names.some(name => !['run_command', 'manage_task'].includes(name))) throw Error('Unexpected tool exposed to the model');
      let message, finish;
      if (JSON.stringify(body).includes('conversation title generator')) {
        message = { role: 'assistant', content: 'Locus Local Tool Probe' }; finish = 'stop';
      } else if (messages.includes('LOCUS_AGY_PROBE_SECOND')) {
        if (!match) throw Error('Second turn lost the tool result');
        message = { role: 'assistant', content: `SECOND:${match[0]}` }; finish = 'stop'; stage = 3;
      } else if (match) {
        message = { role: 'assistant', content: `FIRST:${match[0]}` }; finish = 'stop'; stage = 2;
      } else if (stage === 1 && (messages.includes('functionResponse') || messages.includes('tool_call_id'))) {
        throw Error('Native tool did not return the proof; inspect bridge.json and requests.json');
      } else if (names.includes('run_command')) {
        message = { role: 'assistant', content: null, tool_calls: [{ id: 'call_locus_probe', type: 'function', function: { name: 'run_command', arguments: JSON.stringify({ CommandLine: 'locus-tool read', Cwd: workspace, WaitMsBeforeAsync: 10000, toolSummary: 'Locus read probe', toolAction: 'Reading probe file' }) } }] }; finish = 'tool_calls'; stage = 1;
      } else {
        throw Error(`Unexpected model request: ${req.url}; tools=${names.join(',')}`);
      }
      const common = { id: `chatcmpl-${randomUUID()}`, model: 'locus-probe', created: Math.floor(Date.now() / 1000) };
      if (req.url.includes('GenerateContent') || req.url.includes('generateContent')) {
        const parts = message.tool_calls ? message.tool_calls.map(call => ({ functionCall: { name: call.function.name, args: JSON.parse(call.function.arguments) } })) : [{ text: message.content }];
        const value = { candidates: [{ content: { role: 'model', parts }, finishReason: 'STOP', index: 0 }], usageMetadata: { promptTokenCount: 100, candidatesTokenCount: 10, totalTokenCount: 110 } };
        if (req.url.includes('streamGenerateContent')) { res.writeHead(200, { 'Content-Type': 'text/event-stream' }); res.end(`data: ${JSON.stringify(value)}\n\n`); }
        else json(res, 200, value);
      } else if (body.stream) {
        res.writeHead(200, { 'Content-Type': 'text/event-stream' });
        const delta = message.tool_calls ? { role: 'assistant', tool_calls: message.tool_calls.map((call, index) => ({ index, ...call })) } : message;
        for (const item of [ { ...common, object: 'chat.completion.chunk', choices: [{ index: 0, delta, finish_reason: null }] }, { ...common, object: 'chat.completion.chunk', choices: [{ index: 0, delta: {}, finish_reason: finish }], usage: { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110 } } ]) res.write(`data: ${JSON.stringify(item)}\n\n`);
        res.end('data: [DONE]\n\n');
      } else json(res, 200, { ...common, object: 'chat.completion', choices: [{ index: 0, message, finish_reason: finish }], usage: { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110 } });
    } catch (error) { fatal = String(error); json(res, 400, { error: { message: String(error), type: 'invalid_request_error' } }); runDoneResolve(); }
  });
  await new Promise(resolve => model.listen(0, '127.0.0.1', resolve));
  const modelUrl = `http://127.0.0.1:${model.address().port}`;
  const agent = `---\nname: locus\ndescription: Locus tool bridge probe\nmainAgent: true\nsubagent: false\ntools:\n  - run_command\n---\n\nUse locus-tool read to get the proof. Do not use any other tools.\n`;
  await writeFile(join(workspace, '.agents/agents/locus.md'), agent);
  const hooks = { 'locus-probe': { PreToolUse: [{ matcher: '*', hooks: [{ type: 'command', command: `bun ${script.replaceAll('\\', '/')} hook`, timeout: 10 }] }] } };
  if (route === 'hook') await writeFile(join(workspace, '.agents/hooks.json'), JSON.stringify(hooks));
  await writeFile(join(app, 'settings.json'), JSON.stringify({ model: modelName, permissions: { allow: route === 'runner' ? ['command(locus-tool read)'] : [] } }));
  const bin = join(root, 'bin'); await mkdir(bin);
  if (route === 'runner') {
    await writeFile(join(bin, 'locus-tool.cmd'), `@echo off\r\n"${process.execPath}" "${script}" entry %*\r\n`);
  }
  const env = { ...process.env, HOME: home, USERPROFILE: home, APPDATA: join(home, 'AppData/Roaming'), LOCALAPPDATA: join(home, 'AppData/Local'), ANTIGRAVITY_GEMINI_DIR: gemini, ANTIGRAVITY_APP_DATA_DIR: app, AGY_CLI_DISABLE_AUTO_UPDATE: '1', AGY_LLM_GATEWAY_URL: modelUrl, AGY_LLM_GATEWAY_API_KEY: 'locus-local-fixture', AGY_LLM_GATEWAY_WIRE_PROTOCOL: process.env.LOCUS_AGY_PROBE_WIRE_PROTOCOL || 'genai', AGY_LLM_GATEWAY_MODELS: 'locus-probe', LOCUS_AGY_PROBE_URL: bridgeUrl, LOCUS_AGY_PROBE_TOKEN: token, NO_PROXY: '127.0.0.1,localhost', no_proxy: '127.0.0.1,localhost' };
  for (const name of ['ANTIGRAVITY_HOME', 'AGY_ADC_AUTH', 'AGY_ACCOUNT', 'AGY_LLM_GATEWAY_HEADERS', 'AGY_LLM_GATEWAY_PROXY_URL', 'GEMINI_API_KEY', 'GOOGLE_API_KEY', 'HTTPS_PROXY', 'HTTP_PROXY', 'ALL_PROXY', 'https_proxy', 'http_proxy', 'all_proxy']) delete env[name];
  env.AGY_LLM_GATEWAY_MODELS = modelName;
  env.PATH = bin + (process.platform === 'win32' ? ';' : ':') + (process.env.PATH || '');
  env.LOCUS_AGY_PROBE_TICKET = entryTicket;
  const argv = ['--input-format', 'stream-json', '--output-format', 'stream-json', '--disable-slash-commands', '--print-timeout', '60s', '--model', 'Gemini 3.1 Pro (Low)', '--agent', 'locus', '--mode', 'accept-edits', '--add-dir', workspace, '--log-file', join(root, 'agy.log')];
  const version = execFileSync(binary, ['--version'], { env, windowsHide: true, timeout: 10000, encoding: 'utf8' }).trim();
  emit({ event: 'started', root, version, model: 'deterministic-loopback', executor: mode === '--host-stdio' ? 'locus-rust-host' : 'file-fixture' });
  let stdout = '', stderr = '';
  const timeout = setTimeout(() => { fatal = 'Probe exceeded 90 seconds'; runDoneResolve(); }, 90000);
  try {
    child = spawn(binary, argv, { env, cwd: workspace, stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
    emit({ event: 'process', pid: child.pid, parentPid: process.pid, startedAt: new Date().toISOString(), binary, argv });
    child.stdin.on('error', error => { fatal = String(error); runDoneResolve(); });
    const lines = createInterface({ input: child.stdout });
    child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-100000); });
    child.stdout.on('data', chunk => { stdout = (stdout + chunk).slice(-1000000); });
    child.on('error', error => { fatal = String(error); runDoneResolve(); });
    child.on('exit', (code, signal) => { if (turns !== 2 && !fatal) fatal = `CLI exited before two results: ${code}/${signal}`; runDoneResolve(); });
    lines.on('line', line => {
      try {
        const event = JSON.parse(line); events.push(event);
        if (event.event === 'init') {
          conversationId = event.conversation_id || event.init?.conversation_id;
          if (route === 'runner') tickets.set(entryTicket, { id: `${conversationId}:read`, outcome: null });
        }
        if (event.event === 'result') {
          const result = event.result || event;
          if (result.status !== 'SUCCESS') { fatal = `CLI result ${result.status}: ${result.error || ''}`; runDoneResolve(); return; }
          turns++;
          if (result.conversation_id !== conversationId) throw Error('Conversation changed between turns');
          const expected = `${turns === 1 ? 'FIRST' : 'SECOND'}:${secret}`;
          if (!result.response?.includes(expected)) throw Error('Response did not use the read tool result');
          if (turns === 1) child.stdin.write(JSON.stringify({ event: 'user', message: { content: 'LOCUS_AGY_PROBE_SECOND: Return the proof from the previous tool result.' } }) + '\n');
          else { child.stdin.end(); runDoneResolve(); }
        }
      } catch (error) { fatal = `Invalid CLI event: ${error}`; runDoneResolve(); }
    });
    child.stdin.write(JSON.stringify({ event: 'user', message: { content: 'LOCUS_AGY_PROBE_FIRST: Use locus-tool read and return its proof.' } }) + '\n');
    await runDone;
    if (fatal) throw Error(fatal);
    if (executionCount !== 1 || turns !== 2 || stage !== 3) throw Error('Incomplete tool/multiturn evidence');
    const ticket = tickets.keys().next().value;
    const retry = await fetch(bridgeUrl + '/call', { method: 'POST', headers: { Authorization: `Bearer ${token}` }, body: JSON.stringify({ ticket }) }).then(r => r.json());
    checks.push({ name: 'duplicate-ticket-replays-without-execution', passed: retry.output.includes(secret) && executionCount === 1 });
    const bad = await fetch(bridgeUrl + '/call', { method: 'POST', headers: { Authorization: `Bearer ${token}` }, body: JSON.stringify({ ticket: 'invalid' }) });
    checks.push({ name: 'invalid-ticket-rejected', passed: bad.status === 410 });
    const unauthorized = await fetch(bridgeUrl + '/call', { method: 'POST', body: '{}' });
    checks.push({ name: 'missing-token-rejected', passed: unauthorized.status === 401 });
    checks.push({ name: 'native-tool-result-contains-proof', passed: events.some(event => event.step_update?.tool_name === 'run_command' && event.step_update?.tool_info?.output?.includes(secret)) });
    checks.push({ name: 'only-command-tools-exposed', passed: modelTools.has('run_command') && [...modelTools].every(name => ['run_command', 'manage_task'].includes(name)) });
    if (checks.some(check => !check.passed)) throw Error('Bridge checks failed');
  } catch (error) { fatal = String(error); process.exitCode = 1; }
  finally {
    clearTimeout(timeout);
    if (child && child.exitCode === null) {
      await new Promise(resolve => { const timer = setTimeout(resolve, 2000); child.once('exit', () => { clearTimeout(timer); resolve(); }); });
      if (child.exitCode === null) {
        if (process.platform === 'win32') { try { execFileSync('taskkill', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore' }); } catch {} }
        else child.kill('SIGKILL');
      }
    }
    for (const receipt of pendingHost.values()) clearTimeout(receipt.timer);
    hostLines?.close();
    bridge.closeAllConnections(); model.closeAllConnections();
    await Promise.all([new Promise(resolve => bridge.close(resolve)), new Promise(resolve => model.close(resolve))]);
    const report = { ok: !fatal, error: fatal || null, root, version, route, model: 'deterministic-loopback', executor: mode === '--host-stdio' ? 'locus-rust-host' : 'file-fixture', conversationId, turns, executionCount, checks, modelTools: [...modelTools], childPid: child?.pid, mcpConfigured: false };
    await Promise.all([writeFile(join(root, 'report.json'), JSON.stringify(report, null, 2)), writeFile(join(root, 'events.json'), JSON.stringify(events, null, 2)), writeFile(join(root, 'requests.json'), JSON.stringify(requests, null, 2)), writeFile(join(root, 'bridge.json'), JSON.stringify(records, null, 2)), writeFile(join(root, 'stdout.txt'), stdout), writeFile(join(root, 'stderr.txt'), stderr)]);
    emit({ event: 'finished', ...report });
  }
}
