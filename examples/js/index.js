const { UserId, DeviceId, OlmMachine, ToDevice, DeviceLists } = require('../../crates/matrix-sdk-crypto/pkg');

async function run_example() {
    const user_id = new UserId('@alice:example.org');
    const device_id = new DeviceId('DEVICE_ID');

    const olm_machine = await new OlmMachine(user_id, device_id);
    console.log(olm_machine);
    console.log('olm_machine.user_id().localpart() =', olm_machine.user_id().localpart());
    console.log('olm_machine.device_id =', olm_machine.device_id());
    console.log('olm_machine.display_name =', await olm_machine.display_name());
    console.log('olm_machine.identity_keys =', olm_machine.identity_keys());

    const to_device_events =  '{}';
    const changed_devices = new DeviceLists(
        ['@foo:matrix.org', '@bar:matrix.org'],
        ['@baz:matrix.org', '@qux:matrix.org'],
    );
    console.log(changed_devices);
    const one_time_key_counts = new Map();
    one_time_key_counts.set('foo', 42);
    one_time_key_counts.set('bar', 153);
    const unused_fallback_keys = new Set();
    unused_fallback_keys.add('baz');
    unused_fallback_keys.add('qux');

    const decrypted_to_device = await olm_machine.receive_sync_changes(
        to_device_events,
        changed_devices,
        one_time_key_counts,
        unused_fallback_keys,
    );
    console.log(JSON.parse(decrypted_to_device));

    const outgoing_requests = await olm_machine.outgoing_requests();
    console.log(outgoing_requests);
    //console.log(JSON.parse(outgoing_requests[0].body));
}

run_example();
