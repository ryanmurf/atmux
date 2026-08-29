import Foundation

enum HTTPMethod: String, Equatable {
    case get = "GET"
    case post = "POST"
    case delete = "DELETE"
}

enum APIEndpoint: Equatable {
    case sessions
    case pane(id: String, lines: Int)
    case transcript(id: String)
    case paneModels(id: String)
    case switchModel(id: String)
    case sendMessage(id: String)
    case interrupt(id: String)
    case kill(id: String)
    case pulseAccounts
    case pulseUsage(account: Int64, cursor: String?)

    var method: HTTPMethod {
        switch self {
        case .sendMessage, .switchModel, .interrupt: .post
        case .kill: .delete
        default: .get
        }
    }

    func url(relativeTo baseURL: URL) throws -> URL {
        guard var components = URLComponents(url: baseURL, resolvingAgainstBaseURL: false) else {
            throw APIClientError.invalidURL
        }
        let route: [String]
        var queryItems: [URLQueryItem] = []
        switch self {
        case .sessions:
            route = ["api", "v1", "sessions"]
        case .pane(let id, let lines):
            route = ["api", "v1", "panes", id]
            queryItems = [URLQueryItem(name: "lines", value: String(min(max(lines, 1), 2_000)))]
        case .transcript(let id):
            route = ["api", "v1", "panes", id, "transcript"]
        case .paneModels(let id):
            route = ["api", "v1", "panes", id, "models"]
        case .switchModel(let id):
            route = ["api", "v1", "panes", id, "model"]
        case .sendMessage(let id):
            route = ["api", "v1", "panes", id, "messages"]
        case .interrupt(let id):
            route = ["api", "v1", "panes", id, "interrupt"]
        case .kill(let id):
            route = ["api", "v1", "sessions", id]
        case .pulseAccounts:
            route = ["api", "v1", "pulse", "accounts"]
        case .pulseUsage(let account, let cursor):
            route = ["api", "v1", "pulse", "accounts", String(account), "usage"]
            queryItems = [URLQueryItem(name: "limit", value: "100")]
            if let cursor { queryItems.append(URLQueryItem(name: "cursor", value: cursor)) }
        }
        let encoded = route.map(Self.encodePathSegment).joined(separator: "/")
        components.percentEncodedPath = "/\(encoded)"
        components.queryItems = queryItems.isEmpty ? nil : queryItems
        guard let url = components.url else { throw APIClientError.invalidURL }
        return url
    }

    private static func encodePathSegment(_ value: String) -> String {
        var allowed = CharacterSet.alphanumerics
        allowed.insert(charactersIn: "-._~")
        return value.addingPercentEncoding(withAllowedCharacters: allowed) ?? ""
    }
}
