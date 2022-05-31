const { RequestType, KeysUploadRequest, KeysQueryRequest, KeysClaimRequest, ToDeviceRequest, SignatureUploadRequest, RoomMessageRequest, KeysBackupRequest } = require('../../js/pkg/matrix_sdk_crypto');
const test = require('node:test');
const assert = require('node:assert/strict');

test('RequestType', (t) => {
    assert.equal(RequestType.KeysUpload, 0);
    assert.equal(RequestType.KeysQuery, 1);
    assert.equal(RequestType.KeysClaim, 2);
    assert.equal(RequestType.ToDevice, 3);
    assert.equal(RequestType.SignatureUpload, 4);
    assert.equal(RequestType.RoomMessage, 5);
    assert.equal(RequestType.KeysBackup, 6);
});

test('KeysUploadRequest', (t) => {
    assert.ok(new KeysUploadRequest());
});

test('KeysQueryRequest', (t) => {
    assert.ok(new KeysQueryRequest());
});

test('KeysClaimRequest', (t) => {
    assert.ok(new KeysClaimRequest());
});

test('ToDeviceRequest', (t) => {
    assert.ok(new ToDeviceRequest());
});

test('SignatureUploadRequest', (t) => {
    assert.ok(new SignatureUploadRequest());
});

test('RoomMessageRequest', (t) => {
    assert.ok(new RoomMessageRequest());
});

test('KeysBackupRequest', (t) => {
    assert.ok(new KeysBackupRequest());
});
