const { Tracing, LoggerLevel, OlmMachine, UserId, DeviceId } = require('../pkg/matrix_sdk_crypto_js');

describe('LoggerLevel', () => {
    test('has the correct variant values', () => {
        expect(LoggerLevel.Trace).toStrictEqual(0);
        expect(LoggerLevel.Debug).toStrictEqual(1);
        expect(LoggerLevel.Info).toStrictEqual(2);
        expect(LoggerLevel.Warn).toStrictEqual(3);
        expect(LoggerLevel.Error).toStrictEqual(4);
    });
});

describe(Tracing.name, () => {
    if (Tracing.isAvailable()) {
        let tracing = new Tracing(LoggerLevel.Debug);

        test('can installed several times', () => {
            new Tracing(LoggerLevel.Debug);
            new Tracing(LoggerLevel.Warn);
            new Tracing(LoggerLevel.Debug);
        });

        const originalConsoleDebug = console.debug;

        for (const [testName, testPreState, testPostState, expectedGotcha] of [
            [
                'can log something',
                () => {},
                () => {},
                true,
            ],
            [
                'can change the logger level',
                () => { tracing.minLevel = LoggerLevel.Warn },
                () => { tracing.minLevel = LoggerLevel.Debug },
                false,
            ],
            [
                'can be turned off',
                () => { tracing.turnOff() },
                () => {},
                false,
            ],
            [
                'can be turned on',
                () => { tracing.turnOn() },
                () => {},
                true,
            ],

            // This one *must* be the last. We are turning tracing off
            // again for the other tests.
            [
                'can be turned off',
                () => { tracing.turnOff() },
                () => {},
                false,
            ],
        ]) {
            test(testName, async () => {
                testPreState();

                let gotcha = false;

                console.debug = (msg) => {
                    gotcha = true;
                    expect(msg).not.toHaveLength(0);
                };

                // Do something that emits a `DEBUG` log.
                await OlmMachine.initialize(new UserId('@alice:example.org'), new DeviceId('foo'));

                console.debug = originalConsoleDebug;
                testPostState();

                expect(gotcha).toStrictEqual(expectedGotcha);
            });
        }
    } else {
        test('cannot be constructed', () => {
            expect(() => { new Tracing(LoggerLevel.Error) }).toThrow();
        });
    }
});
