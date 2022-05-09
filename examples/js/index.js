const { UserId, DeviceId, OlmMachine } = require('../../crates/matrix-sdk-crypto/pkg');

async function run_example() {
    const user_id = new UserId('@alice:example.org');
    console.log(user_id);

    const device_id = new DeviceId("DEVICE_ID");
    console.log(device_id);

    const olm_machine = await new OlmMachine(user_id, device_id);
    console.log(olm_machine);
}

run_example();
