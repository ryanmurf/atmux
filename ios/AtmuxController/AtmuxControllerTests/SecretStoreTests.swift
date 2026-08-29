import XCTest
#if SWIFT_PACKAGE
@testable import AtmuxCore
#else
@testable import AtmuxController
#endif

final class SecretStoreTests: XCTestCase {
    func testSecretStoreAbstractionReplacesAndDeletesValues() throws {
        let store = MemorySecretStore()
        try store.set(Data("first".utf8), for: "profile.bearer")
        XCTAssertEqual(try store.data(for: "profile.bearer"), Data("first".utf8))
        try store.set(Data("second".utf8), for: "profile.bearer")
        XCTAssertEqual(try store.data(for: "profile.bearer"), Data("second".utf8))
        try store.remove("profile.bearer")
        XCTAssertNil(try store.data(for: "profile.bearer"))
    }

    func testProfilePreferencesNeverContainCredentialCanary() throws {
        let suite = "AtmuxControllerTests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let profiles = ConnectionProfileStore(defaults: defaults)
        let secrets = MemorySecretStore()
        try secrets.set(Data("SECRET_TOKEN_CANARY".utf8), for: "profile.bearer")
        try profiles.saveProfiles([
            ConnectionProfile(name: "Home", baseURL: URL(string: "https://atmux.example.com")!),
        ])
        let stored = defaults.dictionaryRepresentation().description
        XCTAssertFalse(stored.contains("SECRET_TOKEN_CANARY"))
        XCTAssertFalse(stored.lowercased().contains("bearer"))
        XCTAssertEqual(try secrets.data(for: "profile.bearer"), Data("SECRET_TOKEN_CANARY".utf8))
    }
}

private final class MemorySecretStore: SecretStoring {
    private var values: [String: Data] = [:]
    func data(for account: String) throws -> Data? { values[account] }
    func set(_ data: Data, for account: String) throws { values[account] = data }
    func remove(_ account: String) throws { values.removeValue(forKey: account) }
}
