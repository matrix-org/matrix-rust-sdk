//
//  MatrixRustSDKTests.swift
//  MatrixRustSDKTests
//
//  Created by Stefan Ceriu on 08.02.2022.
//

import XCTest
@testable import MatrixRustSDK

class MatrixRustSDKTests: XCTestCase {
    
    func testReadOnlyFileSystemError() {
        do {
            let client = try ClientBuilder()
                .basePath(path: "")
                .username(username: "@test:domain")
                .build()
            
            try client.login(username: "@test:domain", password: "test")
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
