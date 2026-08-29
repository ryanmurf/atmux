import XCTest
#if SWIFT_PACKAGE
@testable import AtmuxCore
#else
@testable import AtmuxController
#endif

final class APIEndpointTests: XCTestCase {
    private let baseURL = URL(string: "https://atmux.example.com")!

    func testCompositePaneIDIsEncodedAsOnePathSegment() throws {
        let url = try APIEndpoint.transcript(id: "tron~%100").url(relativeTo: baseURL)
        XCTAssertEqual(url.absoluteString, "https://atmux.example.com/api/v1/panes/tron~%25100/transcript")
    }

    func testPaneLineLimitIsBounded() throws {
        let url = try APIEndpoint.pane(id: "max~%2", lines: 99_999).url(relativeTo: baseURL)
        let lines = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first {
            $0.name == "lines"
        }
        XCTAssertEqual(lines?.value, "2000")
    }

    func testMutationMethodsMatchRustRoutes() {
        XCTAssertEqual(APIEndpoint.sendMessage(id: "x").method, .post)
        XCTAssertEqual(APIEndpoint.switchModel(id: "x").method, .post)
        XCTAssertEqual(APIEndpoint.interrupt(id: "x").method, .post)
        XCTAssertEqual(APIEndpoint.kill(id: "x").method, .delete)
        XCTAssertEqual(APIEndpoint.pulseAccounts.method, .get)
    }

    func testPaneModelRoutesUseTheCompositePaneIdentifier() throws {
        XCTAssertEqual(
            try APIEndpoint.paneModels(id: "max~%2").url(relativeTo: baseURL).absoluteString,
            "https://atmux.example.com/api/v1/panes/max~%252/models"
        )
        XCTAssertEqual(
            try APIEndpoint.switchModel(id: "max~%2").url(relativeTo: baseURL).absoluteString,
            "https://atmux.example.com/api/v1/panes/max~%252/model"
        )
    }

    func testConnectionRequiresRootHTTPSURL() throws {
        XCTAssertThrowsError(try ConnectionValidator.profile(id: UUID(), name: "Max", urlText: "http://max.local")) {
            XCTAssertEqual($0 as? ConnectionValidationError, .insecureURL)
        }
        XCTAssertThrowsError(try ConnectionValidator.profile(id: UUID(), name: "Max", urlText: "https://max.local/admin")) {
            XCTAssertEqual($0 as? ConnectionValidationError, .unsupportedBasePath)
        }
        XCTAssertThrowsError(try ConnectionValidator.profile(id: UUID(), name: "Max", urlText: "https://user:secret@max.local")) {
            XCTAssertEqual($0 as? ConnectionValidationError, .embeddedCredentials)
        }
        XCTAssertEqual(
            try ConnectionValidator.profile(id: UUID(), name: " Max ", urlText: "https://max.local").name,
            "Max"
        )
    }

    func testBearerCredentialRejectsHeaderInjectionAndUnboundedInput() throws {
        XCTAssertThrowsError(try BearerCredential.normalized("valid\r\nInjected: value"))
        XCTAssertThrowsError(try BearerCredential.normalized(String(repeating: "x", count: 8_193)))
        XCTAssertEqual(try BearerCredential.normalized("  safe-token  "), "safe-token")
    }
}
