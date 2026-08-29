import Foundation
import SwiftUI

struct SessionsView: View {
    @EnvironmentObject private var model: AppModel
    @Binding var showConnections: Bool

    var body: some View {
        NavigationStack {
            Group {
                if model.isLoading && model.overview == nil {
                    ProgressView("Loading agents…")
                } else if model.machineGroups.isEmpty {
                    ContentUnavailableView(
                        "No agents",
                        systemImage: "terminal",
                        description: Text(model.activeProfile == nil ? "Add an atmux connection." : "No sessions were reported by this server.")
                    )
                } else {
                    List {
                        ForEach(model.machineGroups) { group in
                            Section {
                                if let machine = group.machine {
                                    NavigationLink {
                                        MachineDetailView(machine: machine)
                                    } label: {
                                        Label("Machine telemetry", systemImage: "gauge.with.dots.needle.50percent")
                                            .font(.subheadline)
                                    }
                                }
                                ForEach(group.sessions) { session in
                                    NavigationLink(value: session) { SessionRow(session: session) }
                                }
                            } header: {
                                MachineHeader(machine: group.machine, fallbackID: group.id)
                            }
                        }
                    }
                    .refreshable { await model.refreshDashboard() }
                }
            }
            .navigationTitle(model.activeProfile?.name ?? "atmux")
            .navigationDestination(for: AtmuxSession.self) { session in
                SessionDetailView(session: session)
            }
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    if model.isLoading { ProgressView().controlSize(.small) }
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button("Connections", systemImage: "server.rack") { showConnections = true }
                }
            }
        }
    }
}

private struct MachineDetailView: View {
    let machine: AtmuxMachine

    var body: some View {
        List {
            Section("System") {
                LabeledContent("Status", value: machine.online ? "Online" : "Offline")
                if let health = machine.health { LabeledContent("Health", value: health) }
                if let metrics = machine.metrics {
                    if let cpu = metrics.cpuPercent {
                        metricGauge("CPU", value: Double(cpu), detail: "\(cpu)%")
                    }
                    if metrics.memoryTotalBytes > 0 {
                        let percent = Double(metrics.memoryUsedBytes) / Double(metrics.memoryTotalBytes) * 100
                        metricGauge(
                            "Memory",
                            value: percent,
                            detail: "\(bytes(metrics.memoryUsedBytes)) / \(bytes(metrics.memoryTotalBytes))"
                        )
                    }
                }
            }

            if let metrics = machine.metrics, !metrics.gpus.isEmpty {
                Section("GPU") {
                    ForEach(metrics.gpus) { gpu in
                        GPUCard(gpu: gpu)
                    }
                }
            }

            if let temperatures = machine.metrics?.temperatures, !temperatures.isEmpty {
                Section("Temperatures") {
                    ForEach(temperatures.indices, id: \.self) { index in
                        let reading = temperatures[index]
                        LabeledContent(reading.label, value: "\(reading.celsius.formatted(.number.precision(.fractionLength(1))))°C")
                    }
                }
            }
        }
        .navigationTitle(machine.label)
        .navigationBarTitleDisplayMode(.inline)
    }

    private func metricGauge(_ title: String, value: Double, detail: String) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack { Text(title); Spacer(); Text(detail).foregroundStyle(.secondary) }
            ProgressView(value: min(max(value, 0), 100), total: 100)
        }
    }

    private func bytes(_ value: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: value), countStyle: .memory)
    }
}

private struct GPUCard: View {
    let gpu: GPUMetrics

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack {
                VStack(alignment: .leading) {
                    Text(gpu.name).font(.subheadline.weight(.semibold))
                    if let vendor = gpu.vendor { Text(vendor).font(.caption).foregroundStyle(.secondary) }
                }
                Spacer()
                if let temperature = gpu.temperatureCelsius {
                    Text("\(temperature.formatted(.number.precision(.fractionLength(1))))°C")
                        .font(.subheadline.monospacedDigit())
                }
            }
            if let utilization = gpu.utilizationPercent {
                HStack { Text("Utilization").font(.caption); Spacer(); Text("\(utilization)%").font(.caption.monospacedDigit()) }
                ProgressView(value: Double(utilization), total: 100)
            }
            if let used = gpu.memoryUsedBytes, let total = gpu.memoryTotalBytes, total > 0 {
                HStack {
                    Text("VRAM").font(.caption)
                    Spacer()
                    Text("\(bytes(used)) / \(bytes(total))").font(.caption.monospacedDigit())
                }
                ProgressView(value: Double(used), total: Double(total))
            }
        }
        .padding(.vertical, 5)
    }

    private func bytes(_ value: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: value), countStyle: .memory)
    }
}

private struct SessionRow: View {
    let session: AtmuxSession

    var body: some View {
        HStack(spacing: 12) {
            Circle()
                .fill(statusColor)
                .frame(width: 9, height: 9)
            VStack(alignment: .leading, spacing: 3) {
                Text(session.name).font(.body.weight(.semibold))
                Text([session.agent, session.profile, session.status].filter { !$0.isEmpty }.joined(separator: " · "))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .padding(.vertical, 2)
    }

    private var statusColor: Color {
        switch session.status.lowercased() {
        case "working": .green
        case "waiting": .orange
        default: .secondary
        }
    }
}

private struct MachineHeader: View {
    let machine: AtmuxMachine?
    let fallbackID: String

    var body: some View {
        HStack {
            Text(machine?.label ?? fallbackID)
            Spacer()
            if let metrics = machine?.metrics {
                Text(metricText(metrics))
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            if machine?.online == false { Image(systemName: "wifi.slash").foregroundStyle(.orange) }
        }
    }

    private func metricText(_ metrics: MachineMetrics) -> String {
        var values: [String] = []
        if let cpu = metrics.cpuPercent { values.append("CPU \(cpu)%") }
        if metrics.memoryTotalBytes > 0 {
            let fraction = Double(metrics.memoryUsedBytes) / Double(metrics.memoryTotalBytes)
            values.append("RAM \(Int((fraction * 100).rounded()))%")
        }
        return values.joined(separator: " · ")
    }
}
