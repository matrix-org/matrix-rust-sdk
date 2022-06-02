const { RequestType, KeysUploadRequest, KeysQueryRequest, KeysClaimRequest, ToDeviceRequest, SignatureUploadRequest, RoomMessageRequest, KeysBackupRequest } = require('../');

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

for (const request of [
    KeysUploadRequest,
    KeysQueryRequest,
    KeysClaimRequest,
    ToDeviceRequest,
    SignatureUploadRequest,
    RoomMessageRequest,
    KeysBackupRequest,
]) {
    describe(request.name, () => {
        test('cannot be instantiated', () => {
            expect(() => { new (request)() }).toThrow();
        });
    })

}
