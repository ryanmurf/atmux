import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
#if canImport(Security)
import Security
#endif

enum APIClientError: LocalizedError, Equatable {
    case invalidURL
    case invalidResponse
    case responseTooLarge
    case unauthorized
    case forbidden
    case notFound
    case server(status: Int, message: String)
    case offline
    case timedOut
    case transport
    case decoding
    case invalidCredential

    var errorDescription: String? {
        switch self {
        case .invalidURL: "The server URL is invalid."
        case .invalidResponse: "The server returned an invalid response."
        case .responseTooLarge: "The server response exceeded the 2 MB safety limit."
        case .unauthorized: "The server rejected the saved credentials."
        case .forbidden: "This credential is not allowed to perform that action."
        case .notFound: "The requested atmux resource no longer exists."
        case .server(_, let message): message
        case .offline: "The atmux server is offline or unreachable."
        case .timedOut: "The atmux server did not respond in time."
        case .transport: "The secure connection failed."
        case .decoding: "The server returned an unsupported response."
        case .invalidCredential: "The bearer token must be printable ASCII without whitespace and cannot exceed 8 KB."
        }
    }
}

enum BearerCredential {
    static func normalized(_ value: String?) throws -> String? {
        guard let value else { return nil }
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { return nil }
        guard trimmed.utf8.count <= 8 * 1_024,
              trimmed.unicodeScalars.allSatisfy({ (0x21...0x7e).contains($0.value) }) else {
            throw APIClientError.invalidCredential
        }
        return trimmed
    }
}

final class APIClient {
    static let maximumResponseBytes = 2 * 1_024 * 1_024

    private let baseURL: URL
    private let bearerToken: String?
    private let session: URLSession
    private let retainedDelegate: AnyObject?
    private let decoder = JSONDecoder()
    private let encoder = JSONEncoder()

    init(baseURL: URL, bearerToken: String?, session: URLSession, retainedDelegate: AnyObject? = nil) {
        self.baseURL = baseURL
        self.bearerToken = bearerToken?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
        self.session = session
        self.retainedDelegate = retainedDelegate
    }

    convenience init(profile: ConnectionProfile, secrets: SecretStoring) throws {
        let tokenData = try secrets.data(for: ConnectionSecretKey.bearer(profile.id))
        let token = try BearerCredential.normalized(tokenData.flatMap { String(data: $0, encoding: .utf8) })
        let configuration = URLSessionConfiguration.ephemeral
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.timeoutIntervalForRequest = 20
        configuration.timeoutIntervalForResource = 30
        configuration.urlCache = nil
        configuration.httpCookieStorage = nil

        #if canImport(Security)
        var identity: ClientIdentity?
        if let identityData = try secrets.data(for: ConnectionSecretKey.identity(profile.id)) {
            let passwordData = try secrets.data(for: ConnectionSecretKey.identityPassword(profile.id))
            let password = passwordData.flatMap { String(data: $0, encoding: .utf8) } ?? ""
            identity = try ClientIdentityLoader.load(data: identityData, password: password)
        }
        let delegate = ClientIdentitySessionDelegate(identity: identity)
        let session = URLSession(configuration: configuration, delegate: delegate, delegateQueue: nil)
        self.init(baseURL: profile.baseURL, bearerToken: token, session: session, retainedDelegate: delegate)
        #else
        self.init(baseURL: profile.baseURL, bearerToken: token, session: URLSession(configuration: configuration))
        #endif
    }

    func sessions() async throws -> AtmuxOverview {
        try await get(.sessions, as: AtmuxOverview.self)
    }

    func transcript(sessionID: String) async throws -> Transcript {
        try await get(.transcript(id: sessionID), as: Transcript.self)
    }

    func paneOutput(sessionID: String) async throws -> PaneOutput {
        try await get(.pane(id: sessionID, lines: 2_000), as: PaneOutput.self)
    }

    func paneModels(sessionID: String) async throws -> PaneModels {
        try await get(.paneModels(id: sessionID), as: PaneModels.self)
    }

    func switchModel(_ model: String, for sessionID: String) async throws {
        struct Selection: Encodable { let model: String }
        _ = try await send(
            .switchModel(id: sessionID),
            body: try encoder.encode(Selection(model: model)),
            as: OperationResponse.self
        )
    }

    func sendMessage(_ text: String, to sessionID: String) async throws {
        struct Message: Encodable { let text: String; let submit = true }
        _ = try await send(
            .sendMessage(id: sessionID),
            body: try encoder.encode(Message(text: text)),
            as: OperationResponse.self
        )
    }

    func interrupt(sessionID: String) async throws {
        _ = try await send(.interrupt(id: sessionID), body: nil, as: OperationResponse.self)
    }

    func kill(sessionID: String) async throws {
        _ = try await send(.kill(id: sessionID), body: nil, as: OperationResponse.self)
    }

    func pulseAccounts() async throws -> [PulseAccount] {
        try await get(.pulseAccounts, as: [PulseAccount].self)
    }

    func pulseUsage(accountID: Int64) async throws -> [PulseUsage] {
        var items: [PulseUsage] = []
        var cursor: String?
        for _ in 0..<8 {
            let page = try await get(
                .pulseUsage(account: accountID, cursor: cursor),
                as: PulsePage<PulseUsage>.self
            )
            items.append(contentsOf: page.items)
            cursor = page.nextCursor
            if cursor == nil { return items }
        }
        throw APIClientError.responseTooLarge
    }

    private func get<Response: Decodable>(_ endpoint: APIEndpoint, as type: Response.Type) async throws -> Response {
        try await send(endpoint, body: nil, as: type)
    }

    private func send<Response: Decodable>(
        _ endpoint: APIEndpoint,
        body: Data?,
        as type: Response.Type
    ) async throws -> Response {
        var request = URLRequest(url: try endpoint.url(relativeTo: baseURL))
        request.httpMethod = endpoint.method.rawValue
        request.timeoutInterval = 20
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let bearerToken {
            request.setValue("Bearer \(bearerToken)", forHTTPHeaderField: "Authorization")
        }
        if let body {
            request.httpBody = body
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }

        do {
            let (data, response) = try await session.data(for: request)
            guard let http = response as? HTTPURLResponse else { throw APIClientError.invalidResponse }
            if let length = http.value(forHTTPHeaderField: "Content-Length"),
               let bytes = Int(length), bytes > Self.maximumResponseBytes {
                throw APIClientError.responseTooLarge
            }
            guard data.count <= Self.maximumResponseBytes else { throw APIClientError.responseTooLarge }
            guard (200..<300).contains(http.statusCode) else {
                throw Self.httpError(status: http.statusCode, data: data)
            }
            do {
                return try decoder.decode(type, from: data)
            } catch {
                throw APIClientError.decoding
            }
        } catch let error as APIClientError {
            throw error
        } catch let error as URLError {
            switch error.code {
            case .notConnectedToInternet, .cannotConnectToHost, .cannotFindHost, .networkConnectionLost:
                throw APIClientError.offline
            case .timedOut:
                throw APIClientError.timedOut
            case .cancelled:
                throw CancellationError()
            default:
                throw APIClientError.transport
            }
        } catch {
            throw APIClientError.transport
        }
    }

    private static func httpError(status: Int, data: Data) -> APIClientError {
        switch status {
        case 401: return .unauthorized
        case 403: return .forbidden
        case 404: return .notFound
        default:
            let decoded = try? JSONDecoder().decode(APIErrorResponse.self, from: data)
            let bounded = decoded?.error.map { String($0.prefix(512)).trimmingCharacters(in: .whitespacesAndNewlines) }
            return .server(status: status, message: bounded?.nilIfEmpty ?? "The server returned HTTP \(status).")
        }
    }
}

#if canImport(Security)
private final class ClientIdentitySessionDelegate: NSObject, URLSessionDelegate {
    private let identity: ClientIdentity?

    init(identity: ClientIdentity?) {
        self.identity = identity
    }

    func urlSession(
        _ session: URLSession,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodClientCertificate,
              let identity else {
            completionHandler(.performDefaultHandling, nil)
            return
        }
        completionHandler(
            .useCredential,
            URLCredential(identity: identity.identity, certificates: identity.certificates, persistence: .forSession)
        )
    }
}
#endif

private extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}
