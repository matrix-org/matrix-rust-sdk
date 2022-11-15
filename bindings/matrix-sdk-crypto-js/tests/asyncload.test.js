const { UserId, initAsync } = require("../pkg/matrix_sdk_crypto_js");

test('can instantiate rust objects with async initialiser', async () => {
    initUserId = () => new UserId('@foo:bar.org');

    // stub out the synchronous WebAssembly loader with one that raises an error
    jest.spyOn(WebAssembly, 'Module').mockImplementation(() => { throw new Error('synchronous WebAssembly.Module() not allowed')});

    // this should fail
    expect(initUserId).toThrow(/synchronous/);

    // but once we init with async, it should work
    await initAsync();
    initUserId();
});
