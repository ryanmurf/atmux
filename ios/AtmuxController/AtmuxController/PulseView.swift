import Foundation
import SwiftUI

private struct PulseUsageGroup: Identifiable {
    let profile: String
    let rows: [PulseUsage]
    var id: String { profile }
}

struct PulseView: View {
    @EnvironmentObject private var model: AppModel
    @Binding var showConnections: Bool

    var body: some View {
        NavigationStack {
            Group {
                if model.activeProfile == nil {
                    ContentUnavailableView("No connection", systemImage: "server.rack")
                } else if model.pulseAccounts.isEmpty {
                    ContentUnavailableView(
                        "No Pulse account",
                        systemImage: "gauge.with.dots.needle.67percent",
                        description: Text(model.pulseError ?? "This server did not report a configured Pulse account.")
                    )
                } else {
                    ScrollView {
                        LazyVStack(spacing: 14) {
                            if model.pulseAccounts.count > 1 {
                                Picker("Account", selection: $model.selectedPulseAccountID) {
                                    ForEach(model.pulseAccounts) { account in
                                        Text(account.label).tag(Optional(account.id))
                                    }
                                }
                                .pickerStyle(.menu)
                                .onChange(of: model.selectedPulseAccountID) {
                                    Task { await model.refreshPulseUsage() }
                                }
                            } else if let account = model.pulseAccounts.first {
                                HStack {
                                    Text(account.label).font(.headline)
                                    Spacer()
                                    Text(account.identity).font(.caption).foregroundStyle(.secondary)
                                }
                            }

                            if let error = model.pulseError {
                                Label(error, systemImage: "exclamationmark.triangle")
                                    .font(.caption)
                                    .foregroundStyle(.orange)
                            }

                            if model.pulseUsage.isEmpty {
                                ContentUnavailableView("No usage snapshots", systemImage: "chart.bar")
                            } else {
                                ForEach(groupedUsage) { group in
                                    VStack(alignment: .leading, spacing: 10) {
                                        HStack {
                                            Text(group.profile).font(.headline)
                                            Spacer()
                                            Text(group.rows.first?.vendor.replacingOccurrences(of: "_", with: " ").replacingOccurrences(of: "-", with: " ") ?? "")
                                                .font(.caption)
                                                .foregroundStyle(.secondary)
                                        }
                                        ForEach(group.rows) { usage in PulseUsageCard(usage: usage) }
                                    }
                                }
                            }
                        }
                        .padding()
                    }
                    .refreshable { await model.refreshDashboard() }
                }
            }
            .navigationTitle("Provider usage")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button("Connections", systemImage: "server.rack") { showConnections = true }
                }
            }
        }
    }

    private var groupedUsage: [PulseUsageGroup] {
        Dictionary(grouping: model.pulseUsage, by: \PulseUsage.profile)
            .map { PulseUsageGroup(profile: $0.key, rows: $0.value.sorted { $0.window.kind < $1.window.kind }) }
            .sorted { $0.profile.localizedCaseInsensitiveCompare($1.profile) == .orderedAscending }
    }
}

private struct PulseUsageCard: View {
    let usage: PulseUsage

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text(windowLabel).font(.subheadline.weight(.semibold))
                Spacer()
                Text(usage.window.usedPercent, format: .number.precision(.fractionLength(1)))
                    .font(.title3.monospacedDigit().weight(.semibold))
                Text("%").foregroundStyle(.secondary)
            }
            ProgressView(value: min(max(usage.window.usedPercent, 0), 100), total: 100)
                .tint(usage.window.usedPercent >= 90 ? .red : usage.window.usedPercent >= 70 ? .orange : .accentColor)
            Text("Resets \(formattedReset)")
                .font(.caption)
                .foregroundStyle(.secondary)
            if let selected = usage.contributors.first(where: { $0.chosen }) {
                Label("\(selected.machine) · account value", systemImage: "desktopcomputer")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(14)
        .background(.thinMaterial)
        .clipShape(RoundedRectangle(cornerRadius: 14))
        .accessibilityElement(children: .combine)
    }

    private var windowLabel: String {
        switch usage.window.kind {
        case "five_hour": "5-hour quota"
        case "rolling_seven_day": "Rolling 7-day"
        case "fixed_weekly": "Weekly quota"
        case "monthly_budget": "Monthly budget"
        default: usage.window.kind.replacingOccurrences(of: "_", with: " ").capitalized
        }
    }

    private var formattedReset: String {
        let formatter = ISO8601DateFormatter()
        guard let date = formatter.date(from: usage.window.resetsAt) else { return usage.window.resetsAt }
        return date.formatted(date: .abbreviated, time: .shortened)
    }
}
