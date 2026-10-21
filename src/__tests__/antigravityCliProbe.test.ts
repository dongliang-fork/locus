// @vitest-environment node
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { existsSync, readFileSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { describe, expect, it } from 'vitest';

const script = resolve('scripts/antigravity-cli-probe.mjs');
function run(args: string[], input = '', env: Record<string, string> = {}) {
  return new Promise<{ code: number | null; stdout: string; stderr: string }>((done, reject) => {
    const child = spawn('bun', [script, ...args], {
      env: { ...process.env, LOCUS_AGY_PROBE_URL: '', LOCUS_AGY_PROBE_TOKEN: '', ...env },
      windowsHide: true,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    let stdout = '', stderr = '';
    child.stdout.on('data', chunk => { stdout += chunk; });
    child.stderr.on('data', chunk => { stderr += chunk; });
    child.on('error', reject);
    child.on('close', code => done({ code, stdout, stderr }));
    child.stdin.on('error', () => {});
    child.stdin.end(input);
  });
}

describe('Antigravity P0 command boundary', () => {
  it('rejects unsupported operations before contacting the host', async () => {
    const server = createServer((_req, res) => { calls++; res.end('{}'); });
    let calls = 0;
    await new Promise<void>(done => server.listen(0, '127.0.0.1', done));
    try {
      const address = server.address() as { port: number };
      const result = await run(['entry', 'write'], '', {
        LOCUS_AGY_PROBE_URL: `http://127.0.0.1:${address.port}`,
        LOCUS_AGY_PROBE_TOKEN: 'test', LOCUS_AGY_PROBE_TICKET: 'test',
      });
      expect(result.code).toBe(1);
      expect(result.stderr).toContain('Only the fixed probe read is allowed');
      expect(calls).toBe(0);
    } finally { await new Promise<void>(done => server.close(() => done())); }
  });

  it('returns deny when the hook bridge is unavailable', async () => {
    const result = await run(['hook'], JSON.stringify({ toolCall: { name: 'run_command' } }));
    expect(result.code).toBe(0);
    expect(JSON.parse(result.stdout).decision).toBe('deny');
  });

  it('does not convert malformed hook input into approval', async () => {
    const result = await run(['hook'], 'not-json');
    expect(JSON.parse(result.stdout).decision).toBe('deny');
  });

  const live = process.env.LOCUS_TEST_AGY === '1' && process.platform === 'win32'
    && existsSync(resolve('ref/antigravity-cli-1.4.2/agy.exe'));
  it.skipIf(!live)('records unknown outcome once when the Locus host times out', async () => {
    const result = await run(['--host-stdio'], '', { LOCUS_AGY_PROBE_HOST_TIMEOUT_MS: '300' });
    expect(result.code).toBe(1);
    const events = result.stdout.trim().split('\n').map(line => JSON.parse(line));
    const finished = events.find(event => event.event === 'finished');
    expect(finished.ok).toBe(false);
    expect(finished.executionCount).toBe(1);
    expect(events.filter(event => event.event === 'tool_request')).toHaveLength(1);
    const records = JSON.parse(readFileSync(join(finished.root, 'bridge.json'), 'utf8'));
    expect(records.some((record: { result?: { output: string; isError: boolean } }) =>
      record.result?.isError && record.result.output.includes('outcome unknown'))).toBe(true);
  }, 120_000);
});
