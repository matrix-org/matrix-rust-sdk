//
//  MatrixRustSDKTests.swift
//  MatrixRustSDKTests
//
//  Created by Stefan Ceriu on 08.02.2022.
//

import XCTest
@testable import MatrixRustSDK

class MatrixRustSDKTests: XCTestCase {
    func testExample() throws {
        do {
            let _ = try loginNewClient(basePath: "", username: "test", password: "test")
        } catch ClientError.generic(let message) {
            XCTAssertNotNil(message.range(of: "Read-only file system"))
        } catch {
            XCTFail("Not expecting any other kind of exception")
        }
    }
}
