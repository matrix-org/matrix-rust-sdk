@testable import MatrixRustSDK
import XCTest

class AuthenticationServiceTests: XCTestCase {
    var service: AuthenticationService!
    
    override func setUp() {
        service = AuthenticationService(basePath: FileManager.default.temporaryDirectory.path,
                                        passphrase: nil,
                                        customSlidingSyncProxy: nil)
    }
    
    func testValidServers() {
        XCTAssertNoThrow(try service.configureHomeserver(serverNameOrHomeserverUrl: "matrix.org"))
        XCTAssertNoThrow(try service.configureHomeserver(serverNameOrHomeserverUrl: "https://matrix.org"))
        XCTAssertNoThrow(try service.configureHomeserver(serverNameOrHomeserverUrl: "https://matrix.org/"))
    }
    
    func testInvalidCharacters() {
        XCTAssertThrowsError(try service.configureHomeserver(serverNameOrHomeserverUrl: "hello!@$£%^world"),
                             "A server name with invalid characters should not succeed to build.") { error in
            guard case AuthenticationError.InvalidServerName = error else { XCTFail("Expected invalid name error."); return }
        }
    }
    
    func textNonExistentDomain() {
        XCTAssertThrowsError(try service.configureHomeserver(serverNameOrHomeserverUrl: "somesillylinkthatdoesntexist.com"),
                             "A server name that doesn't exist should not succeed.") { error in
            guard case AuthenticationError.Generic = error else { XCTFail("Expected generic error."); return }
        }
        XCTAssertThrowsError(try service.configureHomeserver(serverNameOrHomeserverUrl: "https://somesillylinkthatdoesntexist.com"),
                             "A server URL that doesn't exist should not succeed.") { error in
            guard case AuthenticationError.Generic = error else { XCTFail("Expected generic error."); return }
        }
    }
    
    func testValidDomainWithoutServer() {
        XCTAssertThrowsError(try service.configureHomeserver(serverNameOrHomeserverUrl: "https://google.com"),
                             "Google should not succeed as it doesn't host a homeserver.") { error in
            guard case AuthenticationError.Generic = error else { XCTFail("Expected generic error."); return }
        }
    }
    
    func testServerWithoutSlidingSync() {
        XCTAssertThrowsError(try service.configureHomeserver(serverNameOrHomeserverUrl: "envs.net"),
                             "Envs should not succeed as it doesn't advertise a sliding sync proxy.") { error in
            guard case AuthenticationError.SlidingSyncNotAvailable = error else { XCTFail("Expected sliding sync error."); return }
        }
    }
    
    func testHomeserverURL() {
        XCTAssertThrowsError(try service.configureHomeserver(serverNameOrHomeserverUrl: "https://matrix-client.matrix.org"),
                             "Directly using a homeserver should not succeed as a sliding sync proxy won't be found.") { error in
            guard case AuthenticationError.SlidingSyncNotAvailable = error else { XCTFail("Expected sliding sync error."); return }
        }
    }
    
    func testHomeserverURLWithProxyOverride() {
        service = AuthenticationService(basePath: FileManager.default.temporaryDirectory.path,
                                        passphrase: nil, customSlidingSyncProxy: "https://slidingsync.proxy")
        XCTAssertNoThrow(try service.configureHomeserver(serverNameOrHomeserverUrl: "https://matrix-client.matrix.org"),
                         "Directly using a homeserver should succeed what a custom sliding sync proxy has been set.")
    }
}
