const mod = require('./index');

const machine = new mod.SledBackedOlmMachine("@travis:localhost", "TESTDEVICE", "./sled/travis-localhost");
console.log(machine.userId);
console.log(machine.deviceId);
console.log(machine.deviceDisplayName);
console.log(machine.identityKeys);
console.log(machine.getDevice("@travis:localhost", "TESTDEVICE"));
console.log(machine.getUserDevices("@travis:localhost"));
console.log(machine.outgoingRequests);
machine.updateTrackedUsers(["@travis:localhost"]);
console.log(machine.isUserTracked("@travis:localhost"));
console.log(machine.outgoingRequests);
console.log(machine.encrypt("!room:example.org", "org.matrix.example", JSON.stringify({hello: "world"})));
console.log(machine.outgoingRequests);
