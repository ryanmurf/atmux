import Foundation
import SwiftUI

struct MachineSessionGroup: Identifiable {
    let machine: AtmuxMachine?
    let id: String
    let sessions: [AtmuxSession]
}

@MainActor
final class AppModel: ObservableObject {
    @Published private(set) var profiles: [ConnectionProfile]
    @Published private(set) var activeProfile: ConnectionProfile?
    @Published private(set) var overview: AtmuxOverview?
    @Published private(set) var pulseAccounts: [PulseAccount] = []
    @Published var selectedPulseAccountID: Int64?
    @Published private(set) var pulseUsage: [PulseUsage] = []
    @Published private(set) var transcripts: [String: Transcript] = [:]
    @Published private(set) var rawOutputs: [String: PaneOutput] = [:]
    @Published private(set) var paneModels: [String: PaneModels] = [:]
    @Published private(set) var modelErrors: [String: String] = [:]
    @Published private(set) var detailErrors: [String: String] = [:]
    @Published private(set) var isLoading = false
    @Published private(set) var isOffline = false
    @Published var errorMessage: String?
    @Published var pulseError: String?

    private let profileStore: ConnectionProfileStore
    private let secrets: SecretStoring
    private var client: APIClient?

    init(
        profileStore: ConnectionProfileStore = ConnectionProfileStore(),
        secrets: SecretStoring = KeychainStore()
    ) {
        self.profileStore = profileStore
        self.secrets = secrets
        profiles = profileStore.loadProfiles()
        let selected = profileStore.activeProfileID.flatMap { id in profiles.first { $0.id == id } }
        activeProfile = selected ?? profiles.first
        if let activeProfile {
            profileStore.activeProfileID = activeProfile.id
            do {
                client = try APIClient(profile: activeProfile, secrets: secrets)
            } catch {
                errorMessage = Self.boundedMessage(error)
            }
        }
    }

    var machineGroups: [MachineSessionGroup] {
        let sessions = overview?.sessions ?? []
        let machinesByID = Dictionary(uniqueKeysWithValues: (overview?.machines ?? []).map { ($0.id, $0) })
        return Dictionary(grouping: sessions, by: \AtmuxSession.machine)
            .map { key, value in
                MachineSessionGroup(
                    machine: machinesByID[key],
                    id: key,
                    sessions: value.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
                )
            }
            .sorted { left, right in
                (left.machine?.label ?? left.id).localizedCaseInsensitiveCompare(right.machine?.label ?? right.id) == .orderedAscending
            }
    }

    func hasBearer(for profileID: UUID) -> Bool {
        do { return try secrets.data(for: ConnectionSecretKey.bearer(profileID))?.isEmpty == false }
        catch { return false }
    }

    func hasIdentity(for profileID: UUID) -> Bool {
        do { return try secrets.data(for: ConnectionSecretKey.identity(profileID))?.isEmpty == false }
        catch { return false }
    }

    func saveConnection(
        _ profile: ConnectionProfile,
        bearerToken: String,
        clearBearer: Bool,
        importedIdentity: Data?,
        identityPassword: String,
        removeIdentity: Bool
    ) async -> Bool {
        do {
            let normalizedBearer = try BearerCredential.normalized(bearerToken)
            if let importedIdentity {
                _ = try ClientIdentityLoader.load(data: importedIdentity, password: identityPassword)
            }
            var updated = profiles.filter { $0.id != profile.id }
            updated.append(profile)
            updated.sort { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
            try profileStore.saveProfiles(updated)
            if clearBearer {
                try secrets.remove(ConnectionSecretKey.bearer(profile.id))
            } else if let normalizedBearer {
                try secrets.set(Data(normalizedBearer.utf8), for: ConnectionSecretKey.bearer(profile.id))
            }
            if removeIdentity {
                try secrets.remove(ConnectionSecretKey.identity(profile.id))
                try secrets.remove(ConnectionSecretKey.identityPassword(profile.id))
            } else if let importedIdentity {
                try secrets.set(importedIdentity, for: ConnectionSecretKey.identity(profile.id))
                try secrets.set(Data(identityPassword.utf8), for: ConnectionSecretKey.identityPassword(profile.id))
            }
            profiles = updated
            try activate(profile)
            await refreshDashboard()
            return true
        } catch {
            errorMessage = Self.boundedMessage(error)
            return false
        }
    }

    func activate(_ profile: ConnectionProfile) throws {
        client = try APIClient(profile: profile, secrets: secrets)
        activeProfile = profile
        profileStore.activeProfileID = profile.id
        overview = nil
        pulseAccounts = []
        pulseUsage = []
        transcripts = [:]
        rawOutputs = [:]
        paneModels = [:]
        modelErrors = [:]
        errorMessage = nil
        pulseError = nil
    }

    func deleteConnection(_ profile: ConnectionProfile) {
        do {
            try secrets.remove(ConnectionSecretKey.bearer(profile.id))
            try secrets.remove(ConnectionSecretKey.identity(profile.id))
            try secrets.remove(ConnectionSecretKey.identityPassword(profile.id))
            profiles.removeAll { $0.id == profile.id }
            try profileStore.saveProfiles(profiles)
            if activeProfile?.id == profile.id {
                activeProfile = profiles.first
                profileStore.activeProfileID = activeProfile?.id
                if let activeProfile {
                    client = try APIClient(profile: activeProfile, secrets: secrets)
                } else {
                    client = nil
                }
            }
        } catch {
            errorMessage = Self.boundedMessage(error)
        }
    }

    func refreshDashboard() async {
        guard let client else { return }
        isLoading = true
        errorMessage = nil
        defer { isLoading = false }
        do {
            overview = try await client.sessions()
            isOffline = false
        } catch {
            record(error)
            return
        }

        do {
            let accounts = try await client.pulseAccounts()
            pulseAccounts = accounts
            if !accounts.contains(where: { $0.id == selectedPulseAccountID }) {
                selectedPulseAccountID = accounts.first?.id
            }
            await refreshPulseUsage()
            pulseError = nil
        } catch {
            pulseAccounts = []
            pulseUsage = []
            pulseError = Self.boundedMessage(error)
        }
    }

    func refreshPulseUsage() async {
        guard let client, let selectedPulseAccountID else {
            pulseUsage = []
            return
        }
        do {
            pulseUsage = try await client.pulseUsage(accountID: selectedPulseAccountID)
            pulseError = nil
        } catch {
            pulseUsage = []
            pulseError = Self.boundedMessage(error)
        }
    }

    func refreshDetail(sessionID: String) async {
        guard let client else { return }
        do {
            async let transcript = client.transcript(sessionID: sessionID)
            async let output = client.paneOutput(sessionID: sessionID)
            let values = try await (transcript, output)
            transcripts[sessionID] = values.0
            rawOutputs[sessionID] = values.1
            detailErrors.removeValue(forKey: sessionID)
            isOffline = false
        } catch is CancellationError {
            return
        } catch {
            detailErrors[sessionID] = Self.boundedMessage(error)
            if error as? APIClientError == .offline { isOffline = true }
        }
        do {
            paneModels[sessionID] = try await client.paneModels(sessionID: sessionID)
            modelErrors.removeValue(forKey: sessionID)
        } catch is CancellationError {
            return
        } catch {
            paneModels.removeValue(forKey: sessionID)
            modelErrors[sessionID] = Self.boundedMessage(error)
        }
    }

    func switchModel(_ selection: PaneModelOption, sessionID: String) async -> Bool {
        guard selection.switchable, let client else { return false }
        do {
            try await client.switchModel(selection.id, for: sessionID)
            paneModels[sessionID] = try await client.paneModels(sessionID: sessionID)
            modelErrors.removeValue(forKey: sessionID)
            return true
        } catch {
            modelErrors[sessionID] = Self.boundedMessage(error)
            return false
        }
    }

    func send(_ text: String, to sessionID: String) async -> Bool {
        let message = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !message.isEmpty, message.utf8.count <= 48 * 1_024, let client else { return false }
        do {
            try await client.sendMessage(message, to: sessionID)
            await refreshDetail(sessionID: sessionID)
            return true
        } catch {
            detailErrors[sessionID] = Self.boundedMessage(error)
            return false
        }
    }

    func interrupt(sessionID: String) async {
        guard let client else { return }
        do {
            try await client.interrupt(sessionID: sessionID)
        } catch {
            detailErrors[sessionID] = Self.boundedMessage(error)
        }
    }

    func kill(sessionID: String) async -> Bool {
        guard let client else { return false }
        do {
            try await client.kill(sessionID: sessionID)
            await refreshDashboard()
            return true
        } catch {
            detailErrors[sessionID] = Self.boundedMessage(error)
            return false
        }
    }

    private func record(_ error: Error) {
        errorMessage = Self.boundedMessage(error)
        isOffline = error as? APIClientError == .offline
    }

    private static func boundedMessage(_ error: Error) -> String {
        let message = (error as? LocalizedError)?.errorDescription ?? "The operation failed."
        return String(message.prefix(512))
    }
}
