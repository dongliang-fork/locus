// Runs inside Claude Code >= 2.1.299. All filesystem/process work stays in Locus.
// Keep this module self-contained: Claude statically checks every mods API call.
async function callBridge($, path, body) {
  const url = await $.env.get('LOCUS_CLAUDE_BRIDGE_URL');
  const token = await $.env.get('LOCUS_CLAUDE_BRIDGE_TOKEN');
  if (!url || !/^http:\/\/127\.0\.0\.1:[0-9]+$/.test(url) || !token) {
    throw new Error('Locus tool bridge is unavailable');
  }
  // Claude caps one HTTP operation at 30 seconds. Only submission may execute
  // a tool; subsequent requests poll its receipt and never submit it again.
  for (;;) {
  const response = await $.http.fetch(url + path, {
    method: 'POST',
    headers: { Authorization: 'Bearer ' + token, 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new Error('Locus tool bridge returned HTTP ' + response.status);
  const answer = JSON.parse(response.text);
  if (!answer.pending) return answer;
  path = '/poll'; body = { protocol: 1, ticket: answer.pending };
  }
}

async function reportInterruption($, turnId, runId, text, thinking) {
  await callBridge($, '/event', { protocol: 1, type: 'turn_interrupted', runId, id: turnId });
  const content = [];
  if (text) content.push({ type: 'text', text });
  if (thinking) content.push({ type: 'thinking', thinking });
  if (content.length) await callBridge($, '/event', { protocol: 1, type: 'interrupted_assistant', runId, message: { content } });
}

// Mods only allows passing $ to file-level helpers; keep session state explicit.
async function reportPhase($, phase, activity) {
  if (!activity || activity.phase === phase) return;
  activity.phase = phase;
  const event = { protocol: 1, type: 'phase', runId: activity.runId,
    stepId: activity.stepId, sequence: ++activity.sequence, phase };
  // The host rejects out-of-order sequences. Never await another hook's
  // Promise: that wait spends this hook's 10 s budget, unlike its own $ calls.
  try { await callBridge($, '/event', event); } catch { /* display only */ }
}

async function registerTools($, tools, registeredTools, toolPlacements) {
  // Validate first: replacing even one eager/previously-referenced schema
  // changes the cache prefix. Registration is append-only for this session.
  for (const tool of tools || []) {
    if (tool.name === 'read' || tool.name === 'web_fetch') continue;
    const prior = registeredTools.get(tool.name);
    if (prior && prior !== JSON.stringify(tool)) throw new Error('Claude tool definition changed: ' + tool.name + '. Start a new conversation.');
  }
  for (const tool of tools || []) {
    if (tool.name === 'read' || tool.name === 'web_fetch' || registeredTools.has(tool.name)) continue;
    toolPlacements.set('mcp__locus__' + tool.name, tool.defer_loading === true);
    // Reserve synchronously: concurrent tool receipts may contain the same
    // append-only delta. The owning hook still awaits the actual registration.
    registeredTools.set(tool.name, JSON.stringify(tool));
    try { await $.tool.register({ name: tool.name, description: tool.description, inputSchema: tool.input_schema }); }
    catch (error) {
      registeredTools.delete(tool.name);
      toolPlacements.delete('mcp__locus__' + tool.name);
      throw error;
    }
  }
}

// Correct only NEW attachment rows (see session.append below). Re-rendering
// old prompt.attachment rows on resume would change an already-cached prefix.
function correctDeferredToolNotice(text, toolPlacements) {
  const lines = text.split('\n');
  const output = [];
  for (let index = 0; index < lines.length; index++) {
    const header = lines[index];
    if (!header.startsWith('The following deferred tools are now available via ToolSearch.')) {
      output.push(header);
      continue;
    }
    const entries = [];
    let end = index + 1;
    while (end < lines.length && /^[A-Za-z_][A-Za-z0-9_.-]*$/.test(lines[end])) entries.push(lines[end++]);
    // Unknown future layouts pass through verbatim rather than losing their
    // header or accidentally changing removal/server-status sections.
    if (end < lines.length && lines[end] !== '' && lines[end] !== '</system-reminder>') {
      output.push(header);
      continue;
    }
    const eager = entries.filter(name => toolPlacements.get(name) === false);
    if (!eager.length) { output.push(header); continue; }
    const deferred = entries.filter(name => toolPlacements.get(name) !== false);
    if (deferred.length) output.push(header, ...deferred);
    output.push('Already available directly (no ToolSearch needed): ' + eager.join(', ') + '. Prefer an equivalent native CLI tool when available.');
    index = end - 1;
  }
  return output.join('\n');
}

// session.append exposes synthetic API errors and engine system rows separately
// from model output. Its system projection retains name/content, not level or
// retry counters. Never infer diagnostics by scanning ordinary assistant text.
function nativeDiagnostic(event, content) {
  const text = content.filter(block => block.type === 'text').map(block => block.text).join('\n');
  if (event.origin?.kind === 'model' && event.origin.model === '<synthetic>') {
    return { kind: 'api_error', text };
  }
  if (event.origin?.kind !== 'engine' || event.message.type !== 'system') return null;
  const kind = event.message.name;
  if (kind === 'api_error' || kind === 'api_retry') {
    return { kind: 'api_retry', text: text || 'Claude Code is retrying an API request.' };
  }
  const notices = {
    model_refusal_no_fallback: 'Claude Code refused the request; no fallback was used.',
    model_refusal_fallback: 'Claude Code is using a configured fallback after a model refusal.',
    model_fallback: 'Claude Code switched to a configured fallback model.',
    model_consent_fallback: 'Claude Code reported a model consent/fallback change.',
    instruction_size_warning: 'Claude Code reports that the instruction files exceed its recommended size.',
    permission_denied: 'Claude Code denied permission for an operation.',
    file_attachments_missing: 'Claude Code could not load one or more file attachments.',
    error: 'Claude Code reported an error.',
  };
  if (Object.hasOwn(notices, kind)) return { kind, text: text.trim() ? text : notices[kind] };
  return null;
}

export function register(on) {
  let tui = false;
  let runId = null;
  let settings = {};
  let pendingText = '';
  let pendingThinking = '';
  let pendingTurnId = null;
  let bridgeFailed = false;
  let bridgeState = 'idle';
  let reportingHealth = false;
  const announcedTools = new Set();
  const toolScopes = new Map();
  let stagedRunId = null;
  const stagedInputs = new Set();
  let imageStaging = null;
  let pollingInput = false;
  let currentTurnId = null;
  let executionSequence = 0;
  let executionId = null;
  let insertion = null;
  let insertionExecutionId = null;
  let insertionAppended = false;
  let interruptionReported = false;
  let lastCheckpoint = null;
  let imageCheck = null;
  let appendingPending = null;
  let activity = null;
  let activityStep = 0;
  on('ui.render', { component: 'Spinner' }, async ($, e, next) => {
    if (tui && e.surface === 'terminal' && e.requestId === 'main'
        && ['requesting', 'thinking', 'tool-input', 'tool-use', 'responding'].includes(e.props?.mode)) {
      await reportPhase($, e.props.mode, activity);
    }
    return next(e);
  });
  const registeredTools = new Map();
  const toolPlacements = new Map();
  on('session.start', async ($, e, next) => {
    tui = await $.env.get('LOCUS_CLAUDE_TUI') === '1';
    if (tui) {
      // Restore the pinned catalog before the engine resumes native history,
      // not only when the next user prompt is submitted.
      const options = await callBridge($, '/options', { protocol: 1 });
      await registerTools($, options.tools, registeredTools, toolPlacements);
    }
    // A worker replacement must be authorized before it can refill/submit the
    // current input. The host rejects reloads during an active turn.
    const ready = await callBridge($, '/ready', { protocol: 1, ...(tui ? { sessionId: await $.session.id(), model: await $.session.model() } : {}) });
    if (ready.deny) {
      bridgeFailed = true;
      throw new Error(ready.deny);
    }
    if (tui) $.clock.every(150, async () => {
      if (pollingInput || bridgeFailed) return;
      pollingInput = true;
      let options;
      let input;
      try {
      options = await callBridge($, '/options', { protocol: 1, view: 'input' });
      input = options.insertion || options;
      if (!options.active || !input.inputId) return;
      const key = input.inputId;
      if (stagedRunId !== options.inputId) {
        stagedRunId = options.inputId;
        stagedInputs.clear();
        imageStaging = null;
      }
      // Clearing an insertion must not refill the already submitted main input.
      if (stagedInputs.has(key)) return;
      const imageCount = input.imageCount || 0;
      if (imageCount) {
        if (imageStaging?.key !== key) {
          const cleared = await $.prompt.fill({ text: '', mode: 'replace' });
          if (!cleared.isFilled) return;
          imageStaging = { key, count: -1 };
          return;
        }
        const draft = await $.prompt.read();
        const markers = draft.text.match(/\[Image #\d+\]/g) || [];
        if (markers.length > imageCount || markers.length < imageStaging.count) {
          throw new Error('Claude image attachments changed while preparing the input.');
        }
        // A path still visible as text is not a loaded image. The host waits
        // for a receipt and times out without submitting it as plain text.
        if (draft.text.replace(/\[Image #\d+\]/g, '').trim()) return;
        if (markers.length !== imageStaging.count) {
          imageStaging.count = markers.length;
          await callBridge($, '/event', { protocol: 1, type: 'input_images', runId: options.inputId, id: key, count: markers.length });
        }
        if (markers.length !== imageCount) return;
      }
      // A trailing space closes native @file completion, so Enter submits
      // instead of selecting the highlighted path on Linux terminals.
      // Append preserves the native media markers and their attachment IDs.
      const filled = await $.prompt.fill({ text: (imageCount ? ' ' : '') + (input.terminalText || input.inputId) + ' ', mode: imageCount ? 'append' : 'replace' });
      if (!filled.isFilled) return;
      // Enter can arrive before the SSH receipt returns. Do not refill or
      // announce the same input again while that acknowledgement is in flight.
      stagedInputs.add(key);
      await callBridge($, '/event', { protocol: 1, type: 'input_ready', runId: options.inputId, id: input.inputId });
      } catch (error) {
        if (options?.active && input?.inputId) {
          stagedInputs.add(input.inputId);
          await callBridge($, '/event', { protocol: 1, type: 'input_staging_error', runId: options.inputId, id: input.inputId, error: String(error) });
        }
      } finally { pollingInput = false; }
    });
    if (tui) $.clock.every(2000, async () => {
      if (reportingHealth || !runId || bridgeFailed) return;
      reportingHealth = true;
      try {
        await callBridge($, '/event', { protocol: 1, type: 'bridge_health', runId, state: bridgeState });
      } catch { /* The host's independent lease detects a missing heartbeat. */ }
      finally { reportingHealth = false; }
    });
    return next(e);
  });
  on('prompt.compose', async ($, e, next) => {
    if (!tui) return next(e);
    const options = await callBridge($, '/options', { protocol: 1, view: 'prompt' });
    return { sections: [{ id: 'locus:system', text: options.systemPrompt || 'You are Locus.', scope: 'session' }] };
  });
  // CLI environment is a separate attachment, not part of prompt.compose.
  // Locus already supplies the tool host's OS/shell/workspace in its prompt.
  // Advertising the PTY host here gives the model two conflicting machines.
  on('prompt.attachment', { type: 'environment' }, async ($, e, next) => {
    if (await $.env.get('LOCUS_CLAUDE_TUI') === '1' && e.type === 'environment') {
      return { text: null };
    }
    return next(e);
  }).catch(async ($, e, next) => {
    if (e.type === 'environment') return { text: null };
    if (next.called) return next(e);
    throw new Error('Claude prompt attachment could not be rendered');
  });
  on('prompt.submit', async ($, e, next) => {
    if (!tui) return next(e);
    if (bridgeFailed) return { drop: 'Locus bridge recovery is required.' };
    settings = await callBridge($, '/options', { protocol: 1 });
    if (!settings.active) return { drop: 'The Locus turn is no longer active.' };
    const inputToken = e.text.replace(/\[Image #\d+\]/g, '').trim();
    const input = (settings.insertion?.terminalText || settings.insertion?.inputId) === inputToken ? settings.insertion : settings;
    const inputExecutionId = executionId;
    if (!e.command && inputToken !== (input.terminalText || input.inputId)) return { drop: 'Input must come from the active Locus conversation.' };
    if (!e.command && (e.attachments || []).filter(item => item.type === 'image').length !== (input.imageCount || 0)) {
      await callBridge($, '/event', { protocol: 1, type: 'input_error', runId: settings.inputId, error: 'Claude did not accept every image attachment.' });
      return { drop: 'Claude did not accept every image attachment.' };
    }
    runId = settings.inputId;
    bridgeState = 'running';
    if (input === settings) { announcedTools.clear(); toolScopes.clear(); }
    if (input.expectedImageCount) {
      const messages = await $.session.messages({ as: 'api' });
      imageCheck = { before: messages.reduce((count, message) => count + message.content.filter(block => block.type === 'image').length, 0), expected: input.expectedImageCount };
    }
    if (input !== settings) { insertion = input; insertionExecutionId = inputExecutionId; insertionAppended = false; interruptionReported = false; }
    await registerTools($, settings.tools, registeredTools, toolPlacements);
    if (!e.command && input === settings) {
      for (const prior of settings.pendingUsers || []) {
        // Recovery may find the native append after its receipt was lost.
        // The stable token makes retry idempotent; it is never text matching.
        const marker = `<locus-pending-user id="${prior.token}">\n`;
        if (typeof prior.id !== 'string' || typeof prior.text !== 'string' || !prior.text.startsWith(marker)
            || !/^[0-9a-f-]{36}$/i.test(prior.token)) throw new Error('Invalid pending user input');
        const existing = await $.session.messages({ as: 'api' });
        if (existing.some(message => message.role === 'user' && message.content.some(block => block.type === 'text' && block.text.startsWith(marker)))) continue;
        appendingPending = prior;
        try {
          const appended = await $.session.append({ message: { type: 'user', content: [{ type: 'text', text: prior.text }] } });
          if (appended.deny) throw new Error(appended.deny);
        } finally { appendingPending = null; }
      }
    }
    const result = await next(e.command ? e : { ...e, text: input.prompt });
    if (result.drop) {
      await callBridge($, '/event', { protocol: 1, type: 'input_error', runId, error: result.drop });
      return result;
    }
    if (input !== settings) {
      // Unblock a local tool even when the native turn waits for its result
      // before raising turn.complete. The GUI is sending this queue now.
      await callBridge($, '/event', { protocol: 1, type: 'turn_interrupted', runId, id: inputExecutionId });
      await callBridge($, '/event', { protocol: 1, type: 'input_queued', runId, id: input.submissionId });
    }
    return result;
  }).catch(async ($, e, next) => {
    const error = 'Locus input bridge failed.' + (next.error?.message ? ' ' + next.error.message : '');
    if (tui && runId) {
      try { await callBridge($, '/event', { protocol: 1, type: 'input_error', runId, error }); }
      catch { /* The host's input timeout still fails closed if the bridge is unavailable. */ }
    }
    return { drop: error };
  });
  // Keep a real, low-frequency builtin deferred from the FIRST request, even
  // with zero Skill tools. Otherwise the first Skill adds eager ToolSearch and
  // invalidates the entire prefix. Never toggle placement after discovery.
  on('tool.describe', { tool: 'TaskList' }, async ($, e, next) => {
    const result = await next(e);
    return await $.env.get('LOCUS_CLAUDE_NATIVE_TOOL_SEARCH') === '1'
      ? { ...result, isDeferred: true } : result;
  });
  on('tool.describe', { tool: /^mcp__locus__/ }, async ($, e, next) => ({
    ...(await next(e)), isDeferred: toolPlacements.get(e.tool) === true,
  }));
  on('session.append', async ($, e, next) => {
    if (!tui) return next(e);
    // Since 2.1.299, queue records also pass through this hook. They are
    // transcript previews, not consumed input or native conversation checkpoints.
    if (e.door === 'queue') return next(e);
    const options = await callBridge($, '/options', { protocol: 1, view: 'session' });
    const pendingUser = appendingPending && e.door === 'note' && e.message.role === 'user'
      && e.message.content.some(block => block.type === 'text' && block.text === appendingPending.text) ? appendingPending : null;
    const inserted = insertion && !insertionAppended && e.message.role === 'user' && e.message.content.some(block => block.type === 'text' && block.text.includes(insertion.prompt)) ? insertion : null;
    // Native attachments retain their media blocks. Only their runtime cache
    // paths are replaced with client labels in model-visible text.
    const content = e.message.content.map(block => {
      if (block.type !== 'text') return block;
      let text = block.text;
      if (e.door === 'attachment' && e.message.type === 'attachment' && e.message.name === 'deferred_tools_delta') {
        text = correctDeferredToolNotice(text, toolPlacements);
      }
      for (const file of options.attachmentMappings || []) {
        text = text.split(file.path).join(file.label);
        const windowsPath = file.path.replaceAll('/', '\\');
        text = text.split(JSON.stringify(windowsPath).slice(1, -1)).join(JSON.stringify(file.label).slice(1, -1));
        text = text.split(windowsPath).join(file.label);
      }
      return { ...block, text };
    });
    const result = await next({ ...e, message: { ...e.message, content } });
    if (result.deny) return result;
    const diagnostic = nativeDiagnostic(e, content);
    if (diagnostic) {
      if (runId && options.active && !e.agentId) {
        await callBridge($, '/event', { protocol: 1, type: 'diagnostic', runId, ...diagnostic });
      }
      return result;
    }
    // Native history is authoritative. This second observation closes the
    // display even if tool.call's post-processing/catch lost its receipt. The
    // host deduplicates by tool_use_id and never re-executes an operation here.
    if (runId && options.active && !e.agentId && !bridgeFailed) {
      for (const block of content) {
        if (block.type !== 'tool_result' || !toolScopes.has(block.tool_use_id)) continue;
        const scope = toolScopes.get(block.tool_use_id);
        await callBridge($, '/event', { protocol: 1, type: 'tool_result', runId: scope.runId,
          id: block.tool_use_id, output: typeof block.content === 'string' ? block.content : JSON.stringify(block.content),
          isError: block.is_error === true });
        toolScopes.delete(block.tool_use_id);
      }
    }
    if (!e.agentId) lastCheckpoint = { version: 1, uuid: e.uuid, sessionId: await $.session.id() };
    if (inserted) {
      insertionAppended = true;
      await callBridge($, '/event', { protocol: 1, type: 'input_consumed', runId, id: inserted.submissionId,
        checkpoint: { version: 1, uuid: e.uuid, sessionId: await $.session.id() } });
    }
    const checkpointUser = !!pendingUser || !!inserted || (e.door === 'prompt' && e.message.type === 'user' && !e.message.isMeta);
    const results = content.filter(block => block.type === 'tool_result').map(block => block.tool_use_id);
    // Auxiliary native user rows have no Locus message to bind. Keep them in
    // native storage and lastCheckpoint, without an unused SSH round trip.
    // Consumed mid-turn inputs can be native attachments/meta rows, not a new
    // user turn. Their actual append UUID is still the delivery checkpoint.
    if (runId && options.active && !e.agentId && (inserted || pendingUser || (!e.message.isMeta && ['user', 'assistant'].includes(e.message.type))) && (checkpointUser || e.message.role === 'assistant' || results.length)) {
      await callBridge($, '/event', { protocol: 1, type: 'checkpoint', runId,
        version: 1, uuid: e.uuid, sessionId: await $.session.id(), role: e.message.role,
        user: checkpointUser, submissionId: inserted ? inserted.submissionId : null,
        messageId: pendingUser ? pendingUser.id : (checkpointUser && !inserted ? settings.messageId : null),
        text: e.message.role === 'assistant' ? content.filter(block => block.type === 'text').map(block => block.text).join('') : '',
        tools: content.filter(block => block.type === 'tool_use').map(block => block.id),
        results });
    }
    return result;
  });
  on('turn.start', async ($, e, next) => {
    if (!e.agentId) {
      currentTurnId = e.turnId;
      executionId = `${runId}:${++executionSequence}`;
    }
    if (tui && !e.agentId) await callBridge($, '/event', { protocol: 1, type: 'turn_start', runId, turnId: e.turnId });
    return next(e);
  });
  on('turn.step', async function* ($, e, next) {
    if (!tui) return yield* next(e);
    if (bridgeFailed) throw new Error('Locus bridge recovery is required.');
    const inputState = await callBridge($, '/options', { protocol: 1, view: 'input' });
    if (!inputState.active) throw new Error('Claude is idle; model requests require an active Locus input.');
    if (e.agentId) return yield* next(e);
    // Claude may absorb input without starting another native turn. Tools
    // produced after that input need a fresh cancellation scope nonetheless.
    if (insertion && insertionAppended) executionId = `${runId}:${++executionSequence}`;
    const stepTurnId = executionId;
    try {
    if (insertion && insertionAppended) {
      if (pendingText || pendingThinking) {
        await reportInterruption($, pendingTurnId, runId, pendingText, pendingThinking);
        pendingText = ''; pendingThinking = '';
      }
      insertion = null; insertionAppended = false;
    }
    pendingTurnId = stepTurnId;
    if (imageCheck) {
      const messages = await $.session.messages({ as: 'api' });
      const images = messages.reduce((count, message) => count + message.content.filter(block => block.type === 'image').length, 0);
      if (images < imageCheck.before + imageCheck.expected) throw new Error('Claude did not load every image attachment; no model request was sent.');
      imageCheck = null;
    }
    // --model is owned by the GUI selection; a changed selection restarts the
    // TUI with --resume. The engine has already resolved aliases here. Writing
    // settings.model back would send e.g. "opus" directly to the provider.
    const request = { ...e, ...(settings.effort ? { effort: settings.effort } : {}) };
    const stepId = ++activityStep;
    const stepRunId = runId;
    activity = null;
    await callBridge($, '/event', { protocol: 1, type: 'step_start', runId, stepId, model: request.model });
    activity = { runId, stepId, phase: 'requesting', sequence: 0 };
    const timing = { version: 1, source: 'claude-turn-step', requestStartedAtMs: await $.clock.now(), firstOutputAtMs: null, responseStoppedAtMs: null, resultReadyAtMs: null, bridgeDurationMs: 0, consumerPauseMs: 0, outputChunks: 0 };
    const stream = next(request);
    const tools = [];
    let thinking = '';
    for await (const chunk of stream) {
      if (timing.firstOutputAtMs === null && ['text', 'thinking', 'tool'].includes(chunk.kind)) timing.firstOutputAtMs = await $.clock.now();
      // Stop belongs to this model response. stream.result / turn.complete can
      // arrive after a tool has finished, so neither is the output clock's end.
      if (chunk.kind === 'stop') timing.responseStoppedAtMs = await $.clock.now();
      if (chunk.kind === 'text' || chunk.kind === 'thinking') {
        const beforeBridge = await $.clock.now();
        await reportPhase($, chunk.kind === 'text' ? 'responding' : 'thinking', activity);
        // Hidden thinking may report progress without a text payload.
        if (typeof chunk.text === 'string') {
          timing.outputChunks++;
          if (chunk.kind === 'text') pendingText += chunk.text;
          else pendingThinking += chunk.text;
          if (chunk.kind === 'thinking') thinking += chunk.text;
          await callBridge($, '/event', { protocol: 1, type: chunk.kind, runId, text: chunk.text });
        }
        timing.bridgeDurationMs += Math.max(0, (await $.clock.now()) - beforeBridge);
      } else if (chunk.kind === 'tool') {
        const beforeBridge = await $.clock.now();
        await reportPhase($, 'tool-input', activity);
        tools.push({ id: chunk.id, name: chunk.name });
        await callBridge($, '/event', { protocol: 1, type: 'tool_start', runId, id: chunk.id, name: chunk.name });
        timing.bridgeDurationMs += Math.max(0, (await $.clock.now()) - beforeBridge);
      }
      const beforeYield = timing.responseStoppedAtMs === null ? await $.clock.now() : null;
      yield chunk;
      if (beforeYield !== null) timing.consumerPauseMs += Math.max(0, (await $.clock.now()) - beforeYield);
    }
    const result = await stream.result;
    timing.resultReadyAtMs = await $.clock.now();
    const context = (await $.session.usage()).context;
    await callBridge($, '/event', { protocol: 1, type: 'usage', runId: stepRunId, stepId, usage: result.usage, context, timing });
    const content = [];
    if (result.answer) content.push({ type: 'text', text: result.answer });
    if (thinking) content.push({ type: 'thinking', thinking });
    for (let i = 0; i < result.toolUses.length; i++) {
      const tool = result.toolUses[i];
      if (!tools[i]) throw new Error('Missing native tool ID');
      content.push({ type: 'tool_use', id: tools[i].id, name: tool.name, input: tool.input });
    }
    // Native TUI starts tools while the model stream is still open. Their
    // authoritative envelopes were persisted by tool.call before execution.
    if (!result.toolUses.length) {
      await callBridge($, '/event', { protocol: 1, type: insertion ? 'interrupted_assistant' : 'assistant', runId, message: { content } });
      pendingText = ''; pendingThinking = '';
    }
    return result;
    } catch (error) {
      activity = null;
      if (insertion) {
        if (!interruptionReported) {
          interruptionReported = true;
          await reportInterruption($, stepTurnId, runId, pendingText, pendingThinking);
          pendingText = ''; pendingThinking = '';
        }
        throw error;
      }
      bridgeState = 'complete';
      await callBridge($, '/event', { protocol: 1, type: 'complete', runId, reason: 'error', answer: String(error) });
      throw error;
    } finally {
      if (insertion) activity = null;
      // Native queue interruption can close a generator with return(), which
      // bypasses catch and has no stream.result. Keep already-visible output.
      if (insertion && (pendingText || pendingThinking)) {
        await reportInterruption($, stepTurnId, runId, pendingText, pendingThinking);
        pendingText = ''; pendingThinking = ''; interruptionReported = true;
      }
    }
  });
  on('turn.complete', async ($, e, next) => {
    if (tui && !e.agentId && insertion) {
      if (e.isAborted && !interruptionReported) {
        interruptionReported = true;
        await reportInterruption($, insertionExecutionId, runId, pendingText, pendingThinking);
        pendingText = ''; pendingThinking = '';
      }
      return next(e);
    }
    if (tui && !e.agentId) {
      activity = null;
      bridgeState = 'complete';
      await callBridge($, '/event', { protocol: 1, type: 'complete', runId, ...e });
    }
    return next(e);
  }).catch(async ($, e, next) => {
    if (tui && !e.agentId && !insertion && runId) {
      bridgeState = 'complete';
      try {
        await callBridge($, '/event', { protocol: 1, type: 'bridge_fault', runId,
          error: 'Claude completion could not be delivered. Resume from native history.' });
      } catch { /* Terminal heartbeat bounds the wait even if this receipt fails. */ }
    }
    return next(e);
  });
  on('session.compact', async ($, e, next) => {
    if (!tui || e.agentId || e.trigger === 'precompute') return next(e);
    await callBridge($, '/event', { protocol: 1, type: 'compact_start', runId, trigger: e.trigger });
    const result = await next(e);
    if (result.skip) {
      await callBridge($, '/event', { protocol: 1, type: 'input_error', runId, error: result.skip });
      return result;
    }
    const context = (await $.session.usage()).context;
    const summary = result.messages.map(message => message.text).filter(Boolean).join('\n');
    await callBridge($, '/event', { protocol: 1, type: 'compact_done', runId,
      before: e.messages.length, after: result.messages.length, tokens: result.tokensAfter, context, summary, checkpoint: lastCheckpoint });
    return result;
  }).catch(async ($, e, next) => {
    const detail = next.error?.message;
    await callBridge($, '/event', { protocol: 1, type: 'input_error', runId, error: 'Locus could not record Claude compaction.' + (detail ? ' ' + detail : '') });
    return next.called ? next(e) : { skip: 'Locus could not record Claude compaction.' };
  });
  on('command.run', async ($, e, next) => {
    if (!tui) return next(e);
    settings = await callBridge($, '/options', { protocol: 1 });
    const commandRunId = settings.inputId;
    runId = commandRunId;
    bridgeState = 'running';
    await callBridge($, '/event', { protocol: 1, type: 'command_start', runId: commandRunId });
    // Only the GUI's explicit compact action may run a native command. Model
    // selection and diagnostics never become chat input or assistant output.
    if (!settings.active || settings.control !== 'compact' || e.command !== 'compact') {
      await callBridge($, '/event', { protocol: 1, type: 'input_error', runId: commandRunId, error: 'Use the corresponding Locus control for Claude commands.' });
      return { text: '' };
    }
    const result = await next(e);
    bridgeState = 'complete';
    await callBridge($, '/event', { protocol: 1, type: 'command', runId: commandRunId });
    return result;
  }).catch(async ($, e, next) => {
    const detail = next.error?.message;
    await callBridge($, '/event', { protocol: 1, type: 'input_error', runId,
      error: 'Claude command failed: /' + e.command + (detail ? '. ' + detail : '') });
    return { text: 'Locus 命令桥接失败，未回退执行命令。' };
  });
  on('tool.call', async ($, e, next) => {
    if (tui && settings.control === 'compact') {
      return { deny: 'Tool execution is disabled during Locus compaction.' };
    }
    const callTurnId = currentTurnId;
    const callExecutionId = executionId;
    const callRunId = runId;
    toolScopes.set(e.tool_use_id, { runId: callRunId, executionId: callExecutionId, dispatched: false });
    if (bridgeFailed) return { deny: 'Locus bridge recovery is required.' };
    if (next.signal?.aborted) throw new Error('Claude tool dispatch was interrupted before execution');
    // Native tools retain Claude's concurrency. Bridged tools are serialized by
    // the host, where waiting for execution cannot exhaust a Mod hook's budget.
    if (tui) {
      if (!e.agentId) await reportPhase($, 'tool-use', activity);
      const { tool, tool_use_id, agentId, ...input } = e;
      const content = [];
      if (pendingText) content.push({ type: 'text', text: pendingText });
      if (pendingThinking) content.push({ type: 'thinking', thinking: pendingThinking });
      pendingText = ''; pendingThinking = '';
      content.push({ type: 'tool_use', id: tool_use_id, name: tool, input });
      await callBridge($, '/event', { protocol: 1, type: 'assistant', runId: callRunId, message: { content } });
      announcedTools.add(tool_use_id);
    }
    if (next.signal?.aborted) throw new Error('Claude tool dispatch was interrupted before execution');
    const locusTool = e.tool.startsWith('mcp__locus__');
    // Session bookkeeping and web tools retain Claude's implementation.
    // Search/fetch requests belong to the CLI, never to Locus.
    if (['TodoWrite',
      'TaskCreate', 'TaskGet', 'TaskList', 'TaskUpdate', 'ToolSearch', 'WebSearch', 'WebFetch'].includes(e.tool)) {
      let result;
      try {
        result = await next(e);
      } catch (error) {
        if (tui) await callBridge($, '/event', { protocol: 1, type: 'tool_result', runId: callRunId, id: e.tool_use_id, output: String(error), isError: true });
        throw error;
      }
      let observed = result;
      if (tui && !result.deny && !result.isError) {
        try {
        let snapshot;
        if (e.tool === 'TodoWrite') {
          snapshot = { version: 1, kind: 'todos', items: e.todos.map(todo => ({ content: todo.content, status: todo.status, priority: todo.priority || 'medium' })) };
        } else if (['TaskCreate', 'TaskUpdate', 'TaskList'].includes(e.tool)) {
          const listed = e.tool === 'TaskList' ? result : await $.tool.call({ tool: 'TaskList' });
          const listedTasks = listed.result?.tasks || listed.result;
          if (listed.deny || listed.isError || !Array.isArray(listedTasks)) {
            await callBridge($, '/event', { protocol: 1, type: 'input_error', runId, error: 'Claude updated its tasks but could not synchronize the task list. Resume to inspect the saved tasks; do not repeat the mutation.' });
            return result;
          }
          const tasks = [...listedTasks];
          for (let index = 0; index < tasks.length; index++) {
            const detail = await $.tool.call({ tool: 'TaskGet', taskId: tasks[index].id });
            if (detail.deny || detail.isError || !(detail.result?.task || detail.result)?.id) throw new Error('Claude task detail could not be synchronized');
            tasks[index] = detail.result.task || detail.result;
          }
          snapshot = { version: 1, kind: 'tasks', tasks, items: tasks.filter(task => task.status !== 'deleted').map(task => ({ content: task.subject, status: task.status, priority: 'medium' })) };
        }
        if (snapshot) observed = { ...result, locusTodoSnapshot: snapshot };
        } catch (error) {
          await callBridge($, '/event', { protocol: 1, type: 'input_error', runId, error: 'Claude task synchronization failed after native execution: ' + String(error) + '. Inspect the tasks before retrying.' });
          return result;
        }
      }
      if (tui) await callBridge($, '/event', { protocol: 1, type: 'tool_result', runId: callRunId, id: e.tool_use_id, output: JSON.stringify(observed), isError: !!(result.deny || result.isError) });
      return result;
    }
    if (e.tool === 'mcp__locus__web_fetch') {
      const deny = 'Use Claude Code WebFetch. Locus web_fetch is unavailable in Claude Code sessions.';
      if (tui) await callBridge($, '/event', { protocol: 1, type: 'tool_result', runId: callRunId, id: e.tool_use_id, output: deny, isError: true });
      return { deny };
    }
    if (!locusTool && !['Read', 'Write', 'Edit', 'Bash', 'Glob', 'Grep', 'TaskOutput', 'TaskStop', 'AskUserQuestion'].includes(e.tool)) {
      const deny = 'Locus has not adapted ' + e.tool + ' for local execution yet.';
      if (tui) await callBridge($, '/event', { protocol: 1, type: 'tool_result', runId: callRunId, id: e.tool_use_id, output: deny, isError: true });
      return { deny };
    }
    // Do not call next for a bridged tool, including on failure. Retrying a
    // transport failure could run the same write twice, so never retry here.
    const { tool, tool_use_id, agentId, ...input } = e;
    toolScopes.get(tool_use_id).dispatched = true;
    const answer = await callBridge($, '/call', {
      protocol: 1, tool, callId: tool_use_id, agentId: agentId ?? null, input, ...(tui ? { runId: callRunId, turnId: callTurnId, executionId: callExecutionId } : {}),
    });
    if (answer.callId !== tool_use_id) throw new Error('Locus tool call ID mismatch');
    if (answer.deny) return { deny: answer.deny };
    if (!Object.hasOwn(answer, 'result')) throw new Error('Locus tool result is missing');
    // Registration must settle before returning the result: the engine then
    // appends its deferred-tools delta and handles ToolSearch/tool_reference.
    // Schemas stay in bridge metadata, never in result text or system prompts.
    await registerTools($, answer.toolCatalogDelta, registeredTools, toolPlacements);
    return { result: answer.result, ...(answer.context ? { context: answer.context } : {}) };
  }).catch(async ($, e, next) => {
    if (next.error?.kind === 're-entry' && ['TaskList','TaskGet'].includes(e.tool)) return next(e);
    // Replay a completed native result, never execute a bridged write again.
    const scope = toolScopes.get(e.tool_use_id) || { runId, executionId };
    let result;
    if (next.called) {
      try { result = await next(e); }
      catch (error) { result = { deny: String(error) }; }
    } else {
      result = { deny: next.signal?.aborted
        ? (scope.dispatched
          ? 'Locus tool dispatch was interrupted; execution outcome may be unknown. Verify local state before retrying.'
          : 'Claude tool dispatch was interrupted before execution.')
        : 'Locus tool bridge failed; no fallback tool was run. Verify local state before retrying a write.' };
    }
    if (tui && !bridgeFailed && scope?.runId) {
      try {
        // These receipts bypass the ordered tool lane. Send them FIRST: a
        // tool_result behind a pending /call cannot unblock that same call.
        if (!next.called && next.signal?.aborted && scope.executionId) {
          await callBridge($, '/event', { protocol: 1, type: 'turn_interrupted', runId: scope.runId, id: scope.executionId });
        }
        if (!next.called && (!next.signal?.aborted || next.error?.kind === 'timeout')) {
          await callBridge($, '/event', { protocol: 1, type: 'bridge_fault', runId: scope.runId,
            error: 'Claude tool hook failed. Resume from native history; no tool will be automatically repeated.' });
          bridgeFailed = true;
        }
        if (!announcedTools.has(e.tool_use_id)) {
          const { tool, tool_use_id, agentId, ...input } = e;
          await callBridge($, '/event', { protocol: 1, type: 'assistant', runId: scope.runId,
            message: { content: [{ type: 'tool_use', id: tool_use_id, name: tool, input }] } });
          announcedTools.add(tool_use_id);
        }
        await callBridge($, '/event', { protocol: 1, type: 'tool_result', runId: scope.runId,
          id: e.tool_use_id, output: JSON.stringify(result), isError: !!(result.deny || result.isError) });
      } catch { bridgeFailed = true; /* Host heartbeat lease will fail closed. */ }
    }
    return result;
  });

  // Independent final guard: if the forwarding hook is skipped, native
  // filesystem/process tools must not execute on the Claude host.
  on('tool.check', async ($, e, next) => {
    if (bridgeFailed) return { decision: 'deny', reason: 'Locus bridge recovery is required.' };
    if (tui && settings.control === 'compact') {
      return { decision: 'deny', reason: 'Tool execution is disabled during Locus compaction.' };
    }
    if (['TodoWrite',
        'TaskCreate', 'TaskGet', 'TaskList', 'TaskUpdate', 'ToolSearch', 'WebSearch', 'WebFetch'].includes(e.tool)) {
      const result = await next(e);
      if (tui && settings.active && runId && !e.agentId && result.decision === 'deny') {
        await callBridge($, '/event', { protocol: 1, type: 'diagnostic', runId,
          kind: 'tool_denied', text: e.tool + ': ' + (result.reason || 'Claude Code denied this tool operation.') });
      }
      return result;
    }
    return { decision: 'deny', reason: 'This tool must execute through the Locus bridge.' };
  }).catch(async () => ({ decision: 'deny', reason: 'Locus tool guard failed.' }));
}
