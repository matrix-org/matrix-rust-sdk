const { DeviceLists, UserId } = require('../../pkg/matrix_sdk_crypto');
const test = require('node:test');
const assert = require('node:assert/strict');

test('DeviceLists', (t) => {
    const empty = new DeviceLists([], []);
    
    assert.equal(empty.isEmpty(), true, 'List is empty');
    assert.equal(empty.changed().length, 0, 'No user ID changed');
    assert.equal(empty.left().length, 0, 'No user ID left');

    const list = new DeviceLists([new UserId('@foo:bar.org')], [new UserId('@baz:qux.org')]);

    assert.equal(list.isEmpty(), false, 'List is not empty');

    const changed = list.changed();
    assert.equal(changed.length, 1, 'There is one user ID changed');
    assert.equal(changed[0].toString(), '@foo:bar.org', 'The user ID changed is correct');

    const left = list.left();
    assert.equal(left.length, 1, 'There is one user ID left');
    assert.equal(left[0].toString(), '@baz:qux.org', 'The user ID left is correct');
});
