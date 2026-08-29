import Foundation
#if canImport(Security)
import Security
#endif

protocol SecretStoring: AnyObject {
    func data(for account: String) throws -> Data?
    func set(_ data: Data, for account: String) throws
    func remove(_ account: String) throws
}

enum SecretStoreError: LocalizedError, Equatable {
    case unavailable(Int32)

    var errorDescription: String? {
        "The secure credential store is unavailable (\(codeDescription))."
    }

    private var codeDescription: String {
        switch self {
        case .unavailable(let code): String(code)
        }
    }
}

#if canImport(Security)
final class KeychainStore: SecretStoring {
    private let service: String

    init(service: String = "com.murphytek.AtmuxController") {
        self.service = service
    }

    func data(for account: String) throws -> Data? {
        var query = baseQuery(account: account)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess else { throw SecretStoreError.unavailable(status) }
        return result as? Data
    }

    func set(_ data: Data, for account: String) throws {
        let query = baseQuery(account: account)
        let attributes: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        ]
        let update = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
        if update == errSecItemNotFound {
            var insertion = query
            attributes.forEach { insertion[$0.key] = $0.value }
            let status = SecItemAdd(insertion as CFDictionary, nil)
            guard status == errSecSuccess else { throw SecretStoreError.unavailable(status) }
        } else if update != errSecSuccess {
            throw SecretStoreError.unavailable(update)
        }
    }

    func remove(_ account: String) throws {
        let status = SecItemDelete(baseQuery(account: account) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw SecretStoreError.unavailable(status)
        }
    }

    private func baseQuery(account: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
    }
}
#else
final class KeychainStore: SecretStoring {
    func data(for account: String) throws -> Data? { throw SecretStoreError.unavailable(-1) }
    func set(_ data: Data, for account: String) throws { throw SecretStoreError.unavailable(-1) }
    func remove(_ account: String) throws { throw SecretStoreError.unavailable(-1) }
}
#endif

enum ConnectionSecretKey {
    static func bearer(_ profileID: UUID) -> String { "\(profileID.uuidString).bearer" }
    static func identity(_ profileID: UUID) -> String { "\(profileID.uuidString).pkcs12" }
    static func identityPassword(_ profileID: UUID) -> String { "\(profileID.uuidString).pkcs12-password" }
}
