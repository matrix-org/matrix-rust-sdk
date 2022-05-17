const { HistoryVisibility } = require('../../pkg/matrix_sdk_crypto');
const test = require('node:test');
const assert = require('node:assert/strict');

test('HistoryVisibility', (t) => {
    assert.equal(HistoryVisibility.Invited, 0);
    assert.equal(HistoryVisibility.Joined, 1);
    assert.equal(HistoryVisibility.Shared, 2);
    assert.equal(HistoryVisibility.WorldReadable, 3);
});
