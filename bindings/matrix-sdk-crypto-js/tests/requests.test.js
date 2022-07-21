const { RequestType, KeysUploadRequest, KeysQueryRequest, KeysClaimRequest, ToDeviceRequest, SignatureUploadRequest, RoomMessageRequest, KeysBackupRequest } = require('../pkg/matrix_sdk_crypto');

describe('RequestType', () => {
    test('has the correct variant values', () => {
        expect(RequestType.KeysUpload).toStrictEqual(0);
        expect(RequestType.KeysQuery).toStrictEqual(1);
        expect(RequestType.KeysClaim).toStrictEqual(2);
        expect(RequestType.ToDevice).toStrictEqual(3);
        expect(RequestType.SignatureUpload).toStrictEqual(4);
        expect(RequestType.RoomMessage).toStrictEqual(5);
        expect(RequestType.KeysBackup).toStrictEqual(6);
    });
});

for (const [request, requestType] of [
    [KeysUploadRequest, RequestType.KeysUpload],
    [KeysQueryRequest, RequestType.KeysQuery],
    [KeysClaimRequest, RequestType.KeysClaim],
    [ToDeviceRequest, RequestType.ToDevice],
    [SignatureUploadRequest, RequestType.SignatureUpload],
    [RoomMessageRequest, RequestType.RoomMessage],
    [KeysBackupRequest, RequestType.KeysBackup],
]) {
    describe(request.name, () => {
        test('can be instantiated', () => {
            const r = new (request)('foo', '{"bar": "baz"}');

            expect(r).toBeInstanceOf(request);
            expect(r.id).toStrictEqual('foo');
            expect(r.body).toStrictEqual('{"bar": "baz"}');
            expect(r.type).toStrictEqual(requestType);
        });
    })

}
