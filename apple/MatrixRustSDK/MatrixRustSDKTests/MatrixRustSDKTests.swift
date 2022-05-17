//
//  MatrixRustSDKTests.swift
//  MatrixRustSDKTests
//
//  Created by Stefan Ceriu on 08.02.2022.
//

import XCTest
@testable import MatrixRustSDK

class MatrixRustSDKTests: XCTestCase {
    
    static var client: Client!
    
    override class func setUp() {
        client = try! guestClient(basePath: basePath, homeserver: "https://matrix.org")
    }
    
    func testClientProperties() {
        XCTAssertTrue(Self.client.isGuest())
        
        XCTAssertNotNil(try? Self.client.restoreToken())
        XCTAssertNotNil(try? Self.client.deviceId())
        XCTAssertNotNil(try? Self.client.displayName())
    }
    
    func testReadOnlyFileSystemError() {
        do {
            let _ = try loginNewClient(basePath: "", username: "test", password: "test")
        } catch ClientError.Generic(let message) {
            XCTAssertNotNil(message.range(of: "Read-only file system"))
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
