import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import XCTest
#if SWIFT_PACKAGE
@testable import AtmuxCore
#else
@testable import AtmuxController
#endif

final class APIClientTests: XCTestCase {
    override func tearDown() {
        StubURLProtocol.handler = nil
        super.tearDown()
    }

    func testTypedSessionsRequestCarriesBearerWithoutPersistingCookies() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubURLProtocol.self]
        let session = URLSession(configuration: configuration)
        StubURLProtocol.handler = { request in
            XCTAssertEqual(request.url?.path, "/api/v1/sessions")
            XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer token-canary")
            let data = #"{"revision":1,"sessions":[],"health":null,"machines":[]}"#.data(using: .utf8)!
            return (HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: nil, headerFields: ["Content-Type": "application/json"])!, data)
        }
        let client = APIClient(
            baseURL: URL(string: "https://atmux.example.com")!,
            bearerToken: "token-canary",
            session: session
        )
        let overview = try await client.sessions()
        XCTAssertEqual(overview.revision, 1)
    }

    func testServerErrorIsBoundedAndDoesNotIncludeAuthorization() async {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubURLProtocol.self]
        let session = URLSession(configuration: configuration)
        StubURLProtocol.handler = { request in
            let message = String(repeating: "x", count: 700)
            let data = try JSONSerialization.data(withJSONObject: ["error": message])
            return (HTTPURLResponse(url: request.url!, statusCode: 500, httpVersion: nil, headerFields: nil)!, data)
        }
        let client = APIClient(
            baseURL: URL(string: "https://atmux.example.com")!,
            bearerToken: "SECRET_TOKEN_CANARY",
            session: session
        )
        do {
            _ = try await client.sessions()
            XCTFail("Expected an error")
        } catch {
            let message = (error as? LocalizedError)?.errorDescription ?? ""
            XCTAssertLessThanOrEqual(message.count, 512)
            XCTAssertFalse(message.contains("SECRET_TOKEN_CANARY"))
        }
    }

    func testTypedModelSwitchPostsOnlyTheSelectedOwnerReportedID() async throws {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubURLProtocol.self]
        let session = URLSession(configuration: configuration)
        StubURLProtocol.handler = { request in
            XCTAssertEqual(request.url?.path, "/api/v1/panes/max~%2/model")
            XCTAssertEqual(request.httpMethod, "POST")
            let body = try XCTUnwrap(request.bodyData())
            let decoded = try XCTUnwrap(JSONSerialization.jsonObject(with: body) as? [String: String])
            XCTAssertEqual(decoded, ["model": "gpt-5.6-sol"])
            let data = #"{"ok":true}"#.data(using: .utf8)!
            return (HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: nil, headerFields: nil)!, data)
        }
        let client = APIClient(
            baseURL: URL(string: "https://atmux.example.com")!,
            bearerToken: "token-canary",
            session: session
        )
        try await client.switchModel("gpt-5.6-sol", for: "max~%2")
    }
}

private extension URLRequest {
    func bodyData() -> Data? {
        if let httpBody { return httpBody }
        guard let httpBodyStream else { return nil }
        httpBodyStream.open()
        defer { httpBodyStream.close() }
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: 4_096)
        while httpBodyStream.hasBytesAvailable {
            let count = httpBodyStream.read(&buffer, maxLength: buffer.count)
            guard count >= 0 else { return nil }
            if count == 0 { break }
            data.append(contentsOf: buffer.prefix(count))
        }
        return data
    }
}

private final class StubURLProtocol: URLProtocol {
    static var handler: ((URLRequest) throws -> (HTTPURLResponse, Data))?

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        do {
            guard let handler = Self.handler else { throw URLError(.badServerResponse) }
            let (response, data) = try handler(request)
            client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: data)
            client?.urlProtocolDidFinishLoading(self)
        } catch {
            client?.urlProtocol(self, didFailWithError: error)
        }
    }

    override func stopLoading() {}
}
