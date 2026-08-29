import Foundation
import SwiftUI
import UniformTypeIdentifiers

struct ConnectionView: View {
    @EnvironmentObject private var model: AppModel
    @Binding var isPresented: Bool

    @State private var profileID = UUID()
    @State private var name = ""
    @State private var urlText = "https://"
    @State private var bearerToken = ""
    @State private var clearBearer = false
    @State private var importedIdentity: Data?
    @State private var identityFileName: String?
    @State private var identityPassword = ""
    @State private var removeIdentity = false
    @State private var showImporter = false
    @State private var isSaving = false
    @State private var validationMessage: String?

    var body: some View {
        NavigationStack {
            Form {
                if !model.profiles.isEmpty {
                    Section("Saved connections") {
                        ForEach(model.profiles) { profile in
                            Button {
                                load(profile)
                            } label: {
                                HStack {
                                    VStack(alignment: .leading) {
                                        Text(profile.name)
                                        Text(profile.baseURL.absoluteString)
                                            .font(.caption)
                                            .foregroundStyle(.secondary)
                                    }
                                    Spacer()
                                    if model.activeProfile?.id == profile.id {
                                        Image(systemName: "checkmark.circle.fill")
                                            .foregroundStyle(.green)
                                    }
                                }
                            }
                            .buttonStyle(.plain)
                        }
                        .onDelete { offsets in
                            let removed = offsets.compactMap { index in
                                model.profiles.indices.contains(index) ? model.profiles[index] : nil
                            }
                            for profile in removed { model.deleteConnection(profile) }
                            resetEditor()
                        }
                        Button("New connection", systemImage: "plus") { resetEditor() }
                    }
                }

                Section("Server") {
                    TextField("Name", text: $name)
                        .textContentType(.name)
                    TextField("https://atmux.example.com", text: $urlText)
                        .textContentType(.URL)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    Text("Only trusted HTTPS endpoints are accepted. Server certificate validation is never bypassed.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                Section("Bearer token") {
                    SecureField(model.hasBearer(for: profileID) ? "Leave blank to keep saved token" : "Token", text: $bearerToken)
                        .textContentType(.password)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    if model.hasBearer(for: profileID) {
                        Toggle("Remove saved token", isOn: $clearBearer)
                    }
                    Text("Tokens are stored only in the iOS Keychain and are never written to app preferences or logs.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                Section("Client certificate (optional)") {
                    Button("Import PKCS#12 identity", systemImage: "person.badge.key") {
                        showImporter = true
                    }
                    if let identityFileName {
                        Label(identityFileName, systemImage: "checkmark.shield")
                            .foregroundStyle(.green)
                    } else if model.hasIdentity(for: profileID) {
                        Label("Client identity saved in Keychain", systemImage: "checkmark.shield")
                            .foregroundStyle(.green)
                        Toggle("Remove saved identity", isOn: $removeIdentity)
                    }
                    if importedIdentity != nil {
                        SecureField("PKCS#12 password", text: $identityPassword)
                            .textContentType(.password)
                    }
                    Text("The identity and its password remain in the Keychain. The app presents it only when the server requests a TLS client certificate.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                if let validationMessage {
                    Section { Text(validationMessage).foregroundStyle(.red) }
                }
            }
            .navigationTitle("Connections")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { isPresented = false }
                        .disabled(model.activeProfile == nil)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") { Task { await save() } }
                        .disabled(isSaving)
                }
            }
            .fileImporter(
                isPresented: $showImporter,
                allowedContentTypes: [UTType(filenameExtension: "p12") ?? .data, UTType(filenameExtension: "pfx") ?? .data]
            ) { result in
                importIdentity(result)
            }
            .onAppear {
                if let active = model.activeProfile { load(active) }
            }
        }
        .interactiveDismissDisabled(model.activeProfile == nil)
    }

    private func load(_ profile: ConnectionProfile) {
        profileID = profile.id
        name = profile.name
        urlText = profile.baseURL.absoluteString
        bearerToken = ""
        clearBearer = false
        importedIdentity = nil
        identityFileName = nil
        identityPassword = ""
        removeIdentity = false
        validationMessage = nil
    }

    private func resetEditor() {
        profileID = UUID()
        name = ""
        urlText = "https://"
        bearerToken = ""
        clearBearer = false
        importedIdentity = nil
        identityFileName = nil
        identityPassword = ""
        removeIdentity = false
        validationMessage = nil
    }

    private func save() async {
        isSaving = true
        defer { isSaving = false }
        do {
            let profile = try ConnectionValidator.profile(id: profileID, name: name, urlText: urlText)
            let saved = await model.saveConnection(
                profile,
                bearerToken: bearerToken,
                clearBearer: clearBearer,
                importedIdentity: importedIdentity,
                identityPassword: identityPassword,
                removeIdentity: removeIdentity
            )
            if saved {
                isPresented = false
            } else {
                validationMessage = model.errorMessage ?? "The connection could not be saved."
                model.errorMessage = nil
            }
        } catch {
            validationMessage = (error as? LocalizedError)?.errorDescription ?? "Invalid connection."
        }
    }

    private func importIdentity(_ result: Result<URL, Error>) {
        do {
            let url = try result.get()
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            let values = try url.resourceValues(forKeys: [.fileSizeKey])
            if let size = values.fileSize, size > ClientIdentityLoader.maximumBytes {
                throw ClientIdentityError.fileTooLarge
            }
            let data = try Data(contentsOf: url, options: .mappedIfSafe)
            guard data.count <= ClientIdentityLoader.maximumBytes else {
                throw ClientIdentityError.fileTooLarge
            }
            importedIdentity = data
            identityFileName = url.lastPathComponent
            identityPassword = ""
            removeIdentity = false
            validationMessage = nil
        } catch {
            validationMessage = (error as? LocalizedError)?.errorDescription ?? "The identity could not be imported."
        }
    }
}
