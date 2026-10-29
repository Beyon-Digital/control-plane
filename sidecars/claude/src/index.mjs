// JSONL protocol only on stdout. Credentials remain in the execution host.
import { createInterface } from 'node:readline';
import { randomUUID } from 'node:crypto';

const output = (kind, payload) => process.stdout.write(JSON.stringify({ kind, payload }) + '\n');
const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
const pending = new Map();
let started = false;
input.on('line', async line => {
  try {
    const value = JSON.parse(line);
    if (!started) {
      started = true;
      await run(value);
      input.close();
    } else {
      const resolve = pending.get(String(value.request_id));
      if (resolve) { pending.delete(String(value.request_id)); resolve(value); }
    }
  } catch (error) {
    output('error', { message: error.message });
    process.exitCode = 1;
    input.close();
  }
});

async function run(config) {
  if (!process.env.ANTHROPIC_API_KEY) throw new Error('ANTHROPIC_API_KEY must be set on the execution host');
  if (typeof config.prompt !== 'string' || typeof config.cwd !== 'string') throw new Error('Invalid sidecar configuration');
  const { query } = await import('@anthropic-ai/claude-agent-sdk');
  const options = {
    cwd: config.cwd,
    includePartialMessages: true,
    permissionMode: config.trusted === true ? 'bypassPermissions' : 'default',
    allowDangerouslySkipPermissions: config.trusted === true,
    mcpServers: config.mcpServers || {},
    ...(config.resume ? { resume: config.resume } : {}),
    canUseTool: async (toolName, toolInput, context) => {
      if (config.trusted === true) return { behavior: 'allow', updatedInput: toolInput };
      const request_id = randomUUID();
      output('approval_requested', { request_id, method: toolName, params: toolInput });
      const answer = await new Promise(resolve => {
        pending.set(request_id, resolve);
        context.signal?.addEventListener('abort', () => {
          pending.delete(request_id);
          resolve({ decision: 'decline' });
        }, { once: true });
      });
      output('approval_resolved', { request_id });
      return ['accept', 'allow'].includes(answer.decision)
        ? { behavior: 'allow', updatedInput: toolInput }
        : { behavior: 'deny', message: 'Denied by Control Plane operator' };
    },
  };
  let completed = false;
  for await (const message of query({ prompt: config.prompt, options })) {
    if (message.type === 'system' && message.subtype === 'init') {
      output('provider_session', { provider_session_id: message.session_id });
    }
    if (message.type === 'stream_event' && message.event?.type === 'content_block_delta' && message.event.delta?.type === 'text_delta') {
      output('message_delta', { text: message.event.delta.text });
    } else {
      output('provider_event', message);
    }
    if (message.type === 'result') {
      if (message.is_error || message.subtype !== 'success') throw new Error(message.errors?.join('; ') || `Claude turn ended with ${message.subtype}`);
      completed = true;
    }
  }
  if (!completed) throw new Error('Claude SDK ended without a successful result');
  output('completed', {});
}
