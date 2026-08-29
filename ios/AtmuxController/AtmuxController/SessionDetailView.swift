import Foundation
import SwiftUI

struct SessionDetailView: View {
    enum DetailTab: String, CaseIterable, Identifiable {
        case transcript = "Conversation"
        case raw = "Raw pane"
        var id: Self { self }
    }

    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    let session: AtmuxSession

    @State private var selectedTab: DetailTab = .transcript
    @State private var message = ""
    @State private var isSending = false
    @State private var isSwitchingModel = false
    @State private var showKillConfirmation = false

    var body: some View {
        VStack(spacing: 0) {
            Picker("View", selection: $selectedTab) {
                ForEach(DetailTab.allCases) { tab in Text(tab.rawValue).tag(tab) }
            }
            .pickerStyle(.segmented)
            .padding()

            modelControl

            if let error = model.detailErrors[session.id] {
                Label(error, systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .padding(.horizontal)
            }

            Group {
                switch selectedTab {
                case .transcript:
                    TranscriptView(transcript: model.transcripts[session.id])
                case .raw:
                    RawPaneView(output: model.rawOutputs[session.id])
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .navigationTitle(session.name)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItemGroup(placement: .topBarTrailing) {
                Button("Interrupt", systemImage: "stop.circle") {
                    Task { await model.interrupt(sessionID: session.id) }
                }
                Button(role: .destructive) {
                    showKillConfirmation = true
                } label: {
                    Label("Kill", systemImage: "trash")
                }
            }
        }
        .safeAreaInset(edge: .bottom) { composer }
        .confirmationDialog(
            "Kill \(session.name)?",
            isPresented: $showKillConfirmation,
            titleVisibility: .visible
        ) {
            Button("Kill session", role: .destructive) {
                Task {
                    if await model.kill(sessionID: session.id) { dismiss() }
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This terminates the tmux session on \(session.machine).")
        }
        .task(id: session.id) {
            while !Task.isCancelled {
                await model.refreshDetail(sessionID: session.id)
                try? await Task.sleep(for: .seconds(3))
            }
        }
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 10) {
            TextField("Message agent", text: $message, axis: .vertical)
                .lineLimit(1...6)
                .textFieldStyle(.roundedBorder)
                .submitLabel(.send)
                .onSubmit { send() }
            Button("Send", systemImage: "arrow.up.circle.fill") { send() }
                .labelStyle(.iconOnly)
                .font(.title2)
                .disabled(isSending || message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                .accessibilityLabel("Send message")
        }
        .padding()
        .background(.bar)
    }

    @ViewBuilder
    private var modelControl: some View {
        if let capabilities = model.paneModels[session.id] {
            HStack(spacing: 10) {
                Image(systemName: "cpu")
                VStack(alignment: .leading, spacing: 2) {
                    Text(capabilities.current ?? "Model not reported")
                        .font(.subheadline.weight(.semibold))
                    Text([capabilities.harness, capabilities.version].compactMap { $0 }.joined(separator: " · "))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if isSwitchingModel {
                    ProgressView().controlSize(.small)
                } else if capabilities.models.contains(where: { $0.switchable }) {
                    Menu("Switch") {
                        ForEach(capabilities.models.filter { $0.switchable }) { option in
                            Button(option.label) { switchModel(option) }
                                .disabled(option.id == capabilities.current)
                        }
                    }
                    .buttonStyle(.bordered)
                }
            }
            .padding(.horizontal)
            if let note = capabilities.note, !note.isEmpty {
                Text(note).font(.caption).foregroundStyle(.secondary).padding(.horizontal)
            }
        } else if let error = model.modelErrors[session.id] {
            Label(error, systemImage: "cpu")
                .font(.caption)
                .foregroundStyle(.secondary)
                .padding(.horizontal)
        }
    }

    private func send() {
        guard !isSending else { return }
        let pending = message
        isSending = true
        Task {
            if await model.send(pending, to: session.id) { message = "" }
            isSending = false
        }
    }

    private func switchModel(_ option: PaneModelOption) {
        guard !isSwitchingModel else { return }
        isSwitchingModel = true
        Task {
            _ = await model.switchModel(option, sessionID: session.id)
            isSwitchingModel = false
        }
    }
}

private struct TranscriptView: View {
    let transcript: Transcript?

    var body: some View {
        if let transcript {
            if !transcript.available {
                ContentUnavailableView("Transcript unavailable", systemImage: "text.bubble")
            } else if transcript.messages.isEmpty {
                ContentUnavailableView("No messages yet", systemImage: "text.bubble")
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 12) {
                        if transcript.truncated {
                            Label("Older transcript content was truncated by the server.", systemImage: "ellipsis.circle")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        ForEach(transcript.messages) { item in
                            TranscriptMessageView(message: item)
                        }
                    }
                    .padding()
                }
                .defaultScrollAnchor(.bottom)
            }
        } else {
            ProgressView("Loading conversation…")
        }
    }
}

private struct TranscriptMessageView: View {
    let message: TranscriptMessage

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack {
                Text(message.role.capitalized).font(.caption.weight(.bold))
                if message.kind != "message" { Text(message.kind.replacingOccurrences(of: "_", with: " ")).font(.caption) }
                Spacer()
                if let timestamp = message.timestamp { Text(timestamp).font(.caption2).foregroundStyle(.secondary) }
            }
            if !message.markdown.isEmpty {
                Text(markdown)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if message.toolName != nil || message.toolInput != nil || message.toolOutput != nil {
                DisclosureGroup(message.toolName ?? "Tool details") {
                    if let input = message.toolInput, !input.isEmpty {
                        LabeledContent("Input") { Text(input).font(.caption.monospaced()).textSelection(.enabled) }
                    }
                    if let output = message.toolOutput, !output.isEmpty {
                        LabeledContent("Output") { Text(output).font(.caption.monospaced()).textSelection(.enabled) }
                    }
                }
                .font(.caption)
            }
        }
        .padding(12)
        .background(message.role == "user" ? Color.accentColor.opacity(0.14) : Color.secondary.opacity(0.1))
        .clipShape(RoundedRectangle(cornerRadius: 12))
    }

    private var markdown: AttributedString {
        (try? AttributedString(markdown: message.markdown)) ?? AttributedString(message.markdown)
    }
}

private struct RawPaneView: View {
    let output: PaneOutput?

    var body: some View {
        if let output {
            ScrollView([.horizontal, .vertical]) {
                Text(output.content ?? "No pane output")
                    .font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .topLeading)
                    .padding()
            }
        } else {
            ProgressView("Loading pane…")
        }
    }
}
