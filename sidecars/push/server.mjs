import http from 'node:http';
import https from 'node:https';
import http2 from 'node:http2';
import { readFileSync } from 'node:fs';
import { createPrivateKey, sign, timingSafeEqual } from 'node:crypto';
import { pathToFileURL } from 'node:url';

export function validate(body) {
  if (!/^[a-fA-F0-9]{64,200}$/.test(body.apns_token ?? '')) throw new Error('Invalid APNs token');
  if (!['sandbox', 'production'].includes(body.environment)) throw new Error('Invalid APNs environment');
  if (!['completed', 'approval', 'failed'].includes(body.kind)) throw new Error('Invalid event kind');
  if (!/^[a-zA-Z0-9-]{1,128}$/.test(body.event_id ?? '')) throw new Error('Invalid opaque event identifier');
  return body;
}
export function notification(body) {
  validate(body);
  const text = {completed: 'An agent finished a task.', approval: 'An agent needs your attention.', failed: 'An agent reported an error.'}[body.kind];
  return JSON.stringify({aps: {alert: {title: 'Control Plane', body: text}, sound: 'default'}, event_id: body.event_id});
}
export function authorized(actual, expected) {
  const a = Buffer.from(actual ?? ''), b = Buffer.from(expected ?? '');
  return b.length >= 32 && a.length === b.length && timingSafeEqual(a, b);
}

function providerToken(config) {
  const header = Buffer.from(JSON.stringify({alg: 'ES256', kid: config.keyId})).toString('base64url');
  const claims = Buffer.from(JSON.stringify({iss: config.teamId, iat: Math.floor(Date.now() / 1000)})).toString('base64url');
  const input = `${header}.${claims}`;
  return `${input}.${sign('sha256', Buffer.from(input), {key: config.key, dsaEncoding: 'ieee-p1363'}).toString('base64url')}`;
}
async function sendAPNs(body, config) {
  const authority = body.environment === 'sandbox' ? 'https://api.sandbox.push.apple.com' : 'https://api.push.apple.com';
  const session = http2.connect(authority);
  try {
    return await new Promise((resolve, reject) => {
      session.on('error', reject);
      const request = session.request({':method': 'POST', ':path': `/3/device/${body.apns_token}`,
        authorization: `bearer ${providerToken(config)}`, 'apns-topic': config.topic,
        'apns-push-type': 'alert', 'apns-priority': '10', 'apns-expiration': String(Math.floor(Date.now() / 1000) + 3600),
        'content-type': 'application/json'});
      let status = 0;
      request.on('response', headers => {status = Number(headers[':status']);});
      request.on('error', reject);
      request.on('data', () => {});
      request.on('end', () => status === 200 ? resolve() : reject(new Error(`APNs status ${status}`)));
      request.setTimeout(10000, () => request.destroy(new Error('APNs timeout')));
      request.end(notification(body));
    });
  } finally {session.close();}
}

export function handler(config, transport = sendAPNs) {
  let active = 0;
  return async (request, response) => {
    const reply = (status, body) => {response.writeHead(status, {'content-type': 'application/json'}); response.end(JSON.stringify(body));};
    if (request.url === '/healthz' && request.method === 'GET') return reply(200, {status: 'ok'});
    if (request.url !== '/v1/notify' || request.method !== 'POST') return reply(404, {error: 'Not found'});
    if (!authorized(request.headers.authorization, `Bearer ${config.secret}`)) return reply(401, {error: 'Unauthorized'});
    if (active >= 16) return reply(429, {error: 'Gateway busy'});
    active++;
    try {
      const chunks = []; let size = 0;
      for await (const chunk of request) {size += chunk.length; if (size > 4096) return reply(413, {error: 'Body too large'}); chunks.push(chunk);}
      let body;
      try {body = validate(JSON.parse(Buffer.concat(chunks).toString('utf8')));} catch {return reply(400, {error: 'Invalid notification'});}
      await transport(body, config);
      reply(202, {accepted: true});
    } catch {reply(502, {error: 'Notification delivery failed'});}
    finally {active--;}
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const secret = process.env.CONTROL_PLANE_PUSH_TOKEN;
  if (!secret || secret.length < 32) throw new Error('CONTROL_PLANE_PUSH_TOKEN must contain at least 32 characters');
  const config = {secret, keyId: process.env.APNS_KEY_ID, teamId: process.env.APNS_TEAM_ID,
    topic: process.env.APNS_TOPIC, key: createPrivateKey(readFileSync(process.env.APNS_KEY_FILE))};
  if (!config.keyId || !config.teamId || !config.topic) throw new Error('APNS_KEY_ID, APNS_TEAM_ID and APNS_TOPIC required');
  const host = process.env.LISTEN_HOST ?? '127.0.0.1';
  const tls = process.env.TLS_CERT_FILE && process.env.TLS_KEY_FILE;
  if (!tls && !['127.0.0.1', '::1', 'localhost'].includes(host)) throw new Error('TLS is required for remote listeners');
  const server = tls ? https.createServer({cert: readFileSync(process.env.TLS_CERT_FILE), key: readFileSync(process.env.TLS_KEY_FILE)}, handler(config)) : http.createServer(handler(config));
  server.requestTimeout = 15000; server.headersTimeout = 10000;
  server.listen(Number(process.env.PORT ?? 9849), host, () => console.log('Control Plane push gateway listening'));
}
