import Foundation

struct ConnectionProfile: Codable, Identifiable, Equatable, Hashable {
    let id: UUID
    var name: String
    var baseURL: URL

    init(id: UUID = UUID(), name: String, baseURL: URL) {
        self.id = id
        self.name = name
        self.baseURL = baseURL
    }
}

enum ConnectionValidationError: LocalizedError, Equatable {
    case missingName
    case invalidURL
    case insecureURL
    case unsupportedBasePath
    case nameTooLong
    case embeddedCredentials

    var errorDescription: String? {
        switch self {
        case .missingName: "Enter a connection name."
        case .invalidURL: "Enter a complete server URL."
        case .insecureURL: "The server URL must use HTTPS."
        case .unsupportedBasePath: "The server URL cannot contain a query, fragment, or path."
        case .nameTooLong: "The connection name cannot exceed 80 UTF-8 bytes."
        case .embeddedCredentials: "Credentials cannot be embedded in the server URL."
        }
    }
}

enum ConnectionValidator {
    static func profile(id: UUID, name: String, urlText: String) throws -> ConnectionProfile {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty else { throw ConnectionValidationError.missingName }
        guard trimmedName.utf8.count <= 80 else { throw ConnectionValidationError.nameTooLong }
        let trimmedURL = urlText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmedURL.utf8.count <= 2_048,
              let url = URL(string: trimmedURL),
              url.host != nil else {
            throw ConnectionValidationError.invalidURL
        }
        guard url.scheme?.lowercased() == "https" else {
            throw ConnectionValidationError.insecureURL
        }
        guard url.user == nil, url.password == nil else {
            throw ConnectionValidationError.embeddedCredentials
        }
        guard url.query == nil, url.fragment == nil, url.path.isEmpty || url.path == "/" else {
            throw ConnectionValidationError.unsupportedBasePath
        }
        return ConnectionProfile(id: id, name: trimmedName, baseURL: url)
    }
}

final class ConnectionProfileStore {
    private let defaults: UserDefaults
    private let profilesKey = "connectionProfiles"
    private let activeProfileKey = "activeConnectionProfile"
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    func loadProfiles() -> [ConnectionProfile] {
        guard let data = defaults.data(forKey: profilesKey) else { return [] }
        return (try? decoder.decode([ConnectionProfile].self, from: data)) ?? []
    }

    func saveProfiles(_ profiles: [ConnectionProfile]) throws {
        defaults.set(try encoder.encode(profiles), forKey: profilesKey)
    }

    var activeProfileID: UUID? {
        get {
            defaults.string(forKey: activeProfileKey).flatMap(UUID.init(uuidString:))
        }
        set {
            if let newValue {
                defaults.set(newValue.uuidString, forKey: activeProfileKey)
            } else {
                defaults.removeObject(forKey: activeProfileKey)
            }
        }
    }
}
