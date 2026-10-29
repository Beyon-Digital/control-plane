import { test } from 'node:test';
import assert from 'node:assert/strict';
import { notification, authorized, validate } from './server.mjs';
test('push includes only generic text and opaque event ID', () => {
  const payload = JSON.parse(notification({apns_token: 'a'.repeat(64), environment: 'sandbox', kind: 'approval', event_id: 'event-1', prompt: 'secret coding data'}));
  assert.equal(payload.event_id, 'event-1');
  assert.ok(!JSON.stringify(payload).includes('secret coding data'));
  assert.deepEqual(Object.keys(payload).sort(), ['aps', 'event_id']);
});
test('rejects malformed delivery parameters', () => {
  assert.throws(() => validate({apns_token: 'invalid'}));
  assert.throws(() => validate({apns_token: 'a'.repeat(64), environment: 'sandbox', kind: 'completed', event_id: '../../x'}));
});
test('auth requires full secret', () => {
  const expected = 'Bearer ' + 'x'.repeat(64);
  assert.ok(authorized(expected, expected));
  assert.ok(!authorized('Bearer x', expected));
  assert.ok(!authorized('', ''));
});
