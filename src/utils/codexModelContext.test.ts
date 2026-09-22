import assert from 'node:assert/strict';
import test from 'node:test';
import { validateModelContext } from './codexModelContext';

test('model context follows metadata unless both overrides are explicitly configured', () => {
  assert.equal(validateModelContext({}), null);
  assert.equal(validateModelContext({ context_window: 516000, auto_compact_token_limit: 460000 }), null);
  assert.equal(validateModelContext({ context_window: 1000000, auto_compact_token_limit: 900000 }), null);
  assert.equal(validateModelContext({ context_window: 32768, auto_compact_token_limit: 30000 }), null);
});

test('model context rejects missing, non-integer, unsafe and out-of-range values', () => {
  for (const model of [
    { context_window: 100 }, { auto_compact_token_limit: 50 },
    { context_window: 0, auto_compact_token_limit: 1 },
    { context_window: 1.5, auto_compact_token_limit: 1 },
    { context_window: Infinity, auto_compact_token_limit: 1 },
    { context_window: Number.MAX_SAFE_INTEGER + 1, auto_compact_token_limit: 1 },
    { context_window: 100, auto_compact_token_limit: NaN },
    { context_window: 100, auto_compact_token_limit: -1 },
    { context_window: 100, auto_compact_token_limit: 100 },
    { context_window: 100, auto_compact_token_limit: 101 },
  ]) assert.ok(validateModelContext(model), JSON.stringify(model));
});
