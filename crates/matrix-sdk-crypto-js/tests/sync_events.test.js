const { DeviceLists, UserId } = require('../pkg/matrix_sdk_crypto');

describe(DeviceLists.name, () => {
    test('can be empty', () => {
        const empty = new DeviceLists();

        expect(empty.isEmpty()).toStrictEqual(true);
        expect(empty.changed).toHaveLength(0);
        expect(empty.left).toHaveLength(0);
    });

    test('can be coerced empty', () => {
        const empty = new DeviceLists([], []);

        expect(empty.isEmpty()).toStrictEqual(true);
        expect(empty.changed).toHaveLength(0);
        expect(empty.left).toHaveLength(0);
    });

    test('returns the correct `changed` and `left`', () => {
        const list = new DeviceLists([new UserId('@foo:bar.org')], [new UserId('@baz:qux.org')]);

        expect(list.isEmpty()).toStrictEqual(false);

        expect(list.changed).toHaveLength(1);
        expect(list.changed[0].toString()).toStrictEqual('@foo:bar.org');

        expect(list.left).toHaveLength(1);
        expect(list.left[0].toString()).toStrictEqual('@baz:qux.org');
    });
});
