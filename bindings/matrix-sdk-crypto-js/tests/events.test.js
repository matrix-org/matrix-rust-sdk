const { HistoryVisibility } = require('../pkg/matrix_sdk_crypto');

describe('HistoryVisibility', () => {
    test('has the correct variant values', () => {
        expect(HistoryVisibility.Invited).toStrictEqual(0);
        expect(HistoryVisibility.Joined).toStrictEqual(1);
        expect(HistoryVisibility.Shared).toStrictEqual(2);
        expect(HistoryVisibility.WorldReadable).toStrictEqual(3);
    });
});
