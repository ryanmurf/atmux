import Foundation
#if canImport(Security)
import Security

struct ClientIdentity {
    let identity: SecIdentity
    let certificates: [Any]
}

enum ClientIdentityError: LocalizedError {
    case fileTooLarge
    case passwordTooLong
    case invalidIdentity
    case importFailed(Int32)

    var errorDescription: String? {
        switch self {
        case .fileTooLarge: "The PKCS#12 identity exceeds the 1 MB limit."
        case .passwordTooLong: "The PKCS#12 password exceeds the 1 KB limit."
        case .invalidIdentity: "The PKCS#12 file does not contain a client identity."
        case .importFailed: "The PKCS#12 identity or password is invalid."
        }
    }
}

enum ClientIdentityLoader {
    static let maximumBytes = 1_048_576

    static func load(data: Data, password: String) throws -> ClientIdentity {
        guard data.count <= maximumBytes else { throw ClientIdentityError.fileTooLarge }
        guard password.utf8.count <= 1_024 else { throw ClientIdentityError.passwordTooLong }
        var imported: CFArray?
        let options = [kSecImportExportPassphrase as String: password] as CFDictionary
        let status = SecPKCS12Import(data as CFData, options, &imported)
        guard status == errSecSuccess else { throw ClientIdentityError.importFailed(status) }
        guard let item = (imported as? [[String: Any]])?.first,
              let identityValue = item[kSecImportItemIdentity as String],
              CFGetTypeID(identityValue as CFTypeRef) == SecIdentityGetTypeID() else {
            throw ClientIdentityError.invalidIdentity
        }
        let identity = identityValue as! SecIdentity
        let chain = item[kSecImportItemCertChain as String] as? [Any] ?? []
        return ClientIdentity(identity: identity, certificates: chain)
    }
}
#else
enum ClientIdentityError: LocalizedError {
    case unavailable
    var errorDescription: String? { "Client identities require an Apple platform." }
}
#endif
