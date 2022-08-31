import XCTest
@testable import MatrixRustSDK

final class ClientTests: XCTestCase {
    func testBuildingWithHomeserverURL() {
        do {
            _ = try ClientBuilder()
                .homeserverUrl(url: "https://localhost:8008")
                .build()
        } catch {
            XCTFail("The client should build successfully when given a homeserver.")
        }
    }
    
    func testBuildingWithUsername() {
        do {
            _ = try ClientBuilder()
                .username(username: "@test:matrix.org")
                .build()
        } catch {
            XCTFail("The client should build successfully when given a username.")
        }
    }
    
    func testBuildingWithInvalidUsername() {
        do {
            _ = try ClientBuilder()
                .username(username: "@test:invalid")
                .build()
            
            XCTFail("The client should not build when given an invalid username.")
        } catch ClientError.Generic(let message) {
            XCTAssertTrue(message.contains(".well-known"), "The client should fail to do the well-known lookup.")
        } catch {
            XCTFail("Not expecting any other kind of exception")
        }
    }
    
    // MARK: - Private
    
    static private var basePath: String {
        guard let url = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first else {
            fatalError("Should always be able to retrieve the caches directory")
        }
        
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: false, attributes: nil)
        
        return url.path
    }
}
