const { DeviceLists, RequestType, KeysUploadRequest, KeysQueryRequest } = require("../pkg/matrix_sdk_crypto_js");

function* zip(...arrays) {
    const len = Math.min(...arrays.map((array) => array.length));

    for (let nth = 0; nth < len; ++nth) {
        yield [...arrays.map((array) => array.at(nth))];
    }
}

// Add a machine to another machine, i.e. be sure a machine knows
// another exists.
async function addMachineToMachine(machineToAdd, machine) {
    const toDeviceEvents = JSON.stringify([]);
    const changedDevices = new DeviceLists();
    const oneTimeKeyCounts = new Map();
    const unusedFallbackKeys = new Set();

    const receiveSyncChanges = JSON.parse(
        await machineToAdd.receiveSyncChanges(toDeviceEvents, changedDevices, oneTimeKeyCounts, unusedFallbackKeys),
    );

    expect(receiveSyncChanges).toEqual([[], []]);

    const outgoingRequests = await machineToAdd.outgoingRequests();

    expect(outgoingRequests).toHaveLength(2);

    let keysUploadRequest;
    // Read the `KeysUploadRequest`.
    {
        expect(outgoingRequests[0]).toBeInstanceOf(KeysUploadRequest);
        expect(outgoingRequests[0].id).toBeDefined();
        expect(outgoingRequests[0].type).toStrictEqual(RequestType.KeysUpload);
        expect(outgoingRequests[0].body).toBeDefined();

        const body = JSON.parse(outgoingRequests[0].body);
        expect(body.device_keys).toBeDefined();
        expect(body.one_time_keys).toBeDefined();

        // https://spec.matrix.org/v1.2/client-server-api/#post_matrixclientv3keysupload
        const hypothetical_response = JSON.stringify({
            one_time_key_counts: {
                curve25519: 10,
                signed_curve25519: 20,
            },
        });
        const marked = await machineToAdd.markRequestAsSent(
            outgoingRequests[0].id,
            outgoingRequests[0].type,
            hypothetical_response,
        );
        expect(marked).toStrictEqual(true);

        keysUploadRequest = outgoingRequests[0];
    }

    {
        expect(outgoingRequests[1]).toBeInstanceOf(KeysQueryRequest);

        let [signingKeysUploadRequest, _] = await machineToAdd.bootstrapCrossSigning(true);

        // Let's forge a `KeysQuery`'s response.
        let keyQueryResponse = {
            device_keys: {},
            master_keys: {},
            self_signing_keys: {},
            user_signing_keys: {},
        };
        const userId = machineToAdd.userId.toString();
        const deviceId = machineToAdd.deviceId.toString();
        keyQueryResponse.device_keys[userId] = {};
        keyQueryResponse.device_keys[userId][deviceId] = JSON.parse(keysUploadRequest.body).device_keys;

        const keys = JSON.parse(signingKeysUploadRequest.body);
        keyQueryResponse.master_keys[userId] = keys.master_key;
        keyQueryResponse.self_signing_keys[userId] = keys.self_signing_key;
        keyQueryResponse.user_signing_keys[userId] = keys.user_signing_key;

        const marked = await machine.markRequestAsSent(
            outgoingRequests[1].id,
            outgoingRequests[1].type,
            JSON.stringify(keyQueryResponse),
        );
        expect(marked).toStrictEqual(true);
    }
}

module.exports = {
    zip,
    addMachineToMachine,
};
