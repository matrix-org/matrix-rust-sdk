const { DeviceLists, RequestType, KeysUploadRequest, KeysQueryRequest } = require('../pkg/matrix_sdk_crypto_js');

// Add a machine to another machine, i.e. be sure a machine knows
// another exists.
async function addMachineToMachine(machineToAdd, machine) {
    const toDeviceEvents = JSON.stringify({});
    const changedDevices = new DeviceLists();
    const oneTimeKeyCounts = new Map();
    const unusedFallbackKeys = new Set();

    const receiveSyncChanges = JSON.parse(await machineToAdd.receiveSyncChanges(toDeviceEvents, changedDevices, oneTimeKeyCounts, unusedFallbackKeys));

    expect(receiveSyncChanges).toEqual({});

    const outgoingRequests = await machineToAdd.outgoingRequests();

    expect(outgoingRequests).toHaveLength(2);

    let keysUploadRequest;
    // Read the `KeysUploadRequest`.
    {
        expect(outgoingRequests[0]).toBeInstanceOf(KeysUploadRequest);
        expect(outgoingRequests[0].id).toBeDefined();
        expect(outgoingRequests[0].type).toStrictEqual(RequestType.KeysUpload);

        const body = JSON.parse(outgoingRequests[0].body);
        expect(body.device_keys).toBeDefined();
        expect(body.one_time_keys).toBeDefined();

        keysUploadRequest = body;
    }

    {
        // Just to be sure… not important.
        expect(outgoingRequests[1]).toBeInstanceOf(KeysQueryRequest);
    }

    // Let's forge a `KeysQuery`'s response.
    let keyQueryResponse = {'device_keys': {}};
    const userId = machineToAdd.userId.toString();
    const deviceId = machineToAdd.deviceId.toString();
    keyQueryResponse['device_keys'][userId] = {};
    keyQueryResponse['device_keys'][userId][deviceId] = keysUploadRequest.device_keys;

    await machine.markRequestAsSent('anID', RequestType.KeysQuery, JSON.stringify(keyQueryResponse));
}

module.exports = { addMachineToMachine };
