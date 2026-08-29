import Foundation

struct AtmuxOverview: Decodable, Equatable {
    let revision: UInt64
    let sessions: [AtmuxSession]
    let health: String?
    let machines: [AtmuxMachine]

    private enum CodingKeys: String, CodingKey {
        case revision, sessions, health, machines
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        revision = try values.decode(UInt64.self, forKey: .revision)
        sessions = try values.decode([AtmuxSession].self, forKey: .sessions)
        health = try values.decodeIfPresent(String.self, forKey: .health)
        machines = try values.decodeIfPresent([AtmuxMachine].self, forKey: .machines) ?? []
    }
}

struct AtmuxSession: Decodable, Identifiable, Hashable {
    let id: String
    let machine: String
    let name: String
    let paneID: String
    let status: String
    let agent: String
    let profile: String
    let attached: Bool?
    let activity: UInt64?
    let path: String?
    let title: String?
    let command: String?
    let launchCommand: String?

    private enum CodingKeys: String, CodingKey {
        case id, machine, name, status, agent, profile, attached, activity, path, title, command
        case paneID = "pane_id"
        case launchCommand = "launch_command"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        id = try values.decode(String.self, forKey: .id)
        machine = try values.decodeIfPresent(String.self, forKey: .machine) ?? "local"
        name = try values.decode(String.self, forKey: .name)
        paneID = try values.decode(String.self, forKey: .paneID)
        status = try values.decode(String.self, forKey: .status)
        agent = try values.decode(String.self, forKey: .agent)
        profile = try values.decodeIfPresent(String.self, forKey: .profile) ?? "default"
        attached = try values.decodeIfPresent(Bool.self, forKey: .attached)
        activity = try values.decodeIfPresent(UInt64.self, forKey: .activity)
        path = try values.decodeIfPresent(String.self, forKey: .path)
        title = try values.decodeIfPresent(String.self, forKey: .title)
        command = try values.decodeIfPresent(String.self, forKey: .command)
        launchCommand = try values.decodeIfPresent(String.self, forKey: .launchCommand)
    }
}

struct AtmuxMachine: Decodable, Identifiable, Equatable {
    let id: String
    let label: String
    let kind: String
    let online: Bool
    let sessions: Int
    let health: String?
    let metrics: MachineMetrics?
}

struct MachineMetrics: Decodable, Equatable {
    let cpuPercent: Int?
    let memoryUsedBytes: UInt64
    let memoryTotalBytes: UInt64
    let gpus: [GPUMetrics]
    let temperatures: [TemperatureReading]

    private enum CodingKeys: String, CodingKey {
        case cpuPercent = "cpu_percent"
        case memoryUsedBytes = "memory_used_bytes"
        case memoryTotalBytes = "memory_total_bytes"
        case gpus, temperatures
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        cpuPercent = try values.decodeIfPresent(Int.self, forKey: .cpuPercent)
        memoryUsedBytes = try values.decodeIfPresent(UInt64.self, forKey: .memoryUsedBytes) ?? 0
        memoryTotalBytes = try values.decodeIfPresent(UInt64.self, forKey: .memoryTotalBytes) ?? 0
        gpus = try values.decodeIfPresent([GPUMetrics].self, forKey: .gpus) ?? []
        temperatures = try values.decodeIfPresent([TemperatureReading].self, forKey: .temperatures) ?? []
    }
}

struct GPUMetrics: Decodable, Equatable, Identifiable {
    let id: String
    let name: String
    let vendor: String?
    let utilizationPercent: Int?
    let memoryUsedBytes: UInt64?
    let memoryTotalBytes: UInt64?
    let temperatureCelsius: Double?

    private enum CodingKeys: String, CodingKey {
        case id, name, vendor
        case utilizationPercent = "utilization_percent"
        case memoryUsedBytes = "memory_used_bytes"
        case memoryTotalBytes = "memory_total_bytes"
        case temperatureCelsius = "temperature_celsius"
    }
}

struct TemperatureReading: Decodable, Equatable {
    let label: String
    let celsius: Double
}

struct PaneOutput: Decodable, Equatable {
    let revision: UInt64
    let paneID: String
    let session: String
    let contentHash: String
    let content: String?
    let changed: Bool

    private enum CodingKeys: String, CodingKey {
        case revision, session, content, changed
        case paneID = "pane_id"
        case contentHash = "content_hash"
    }
}

struct PaneModels: Decodable, Equatable {
    let paneID: String
    let harness: String
    let current: String?
    let version: String?
    let models: [PaneModelOption]
    let note: String?

    private enum CodingKeys: String, CodingKey {
        case harness, current, version, models, note
        case paneID = "pane_id"
    }
}

struct PaneModelOption: Decodable, Equatable, Identifiable {
    let id: String
    let label: String
    let switchable: Bool
}

struct Transcript: Decodable, Equatable {
    let available: Bool
    let source: String
    let contentHash: String
    let changed: Bool
    let truncated: Bool
    let messages: [TranscriptMessage]

    private enum CodingKeys: String, CodingKey {
        case available, source, changed, truncated, messages
        case contentHash = "content_hash"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        available = try values.decode(Bool.self, forKey: .available)
        source = try values.decode(String.self, forKey: .source)
        contentHash = try values.decodeIfPresent(String.self, forKey: .contentHash) ?? ""
        changed = try values.decodeIfPresent(Bool.self, forKey: .changed) ?? false
        truncated = try values.decodeIfPresent(Bool.self, forKey: .truncated) ?? false
        messages = try values.decodeIfPresent([TranscriptMessage].self, forKey: .messages) ?? []
    }
}

struct TranscriptMessage: Decodable, Equatable, Identifiable {
    let id: String
    let role: String
    let kind: String
    let markdown: String
    let toolName: String?
    let toolInput: String?
    let toolOutput: String?
    let timestamp: String?

    private enum CodingKeys: String, CodingKey {
        case id, role, kind, markdown, timestamp
        case toolName = "tool_name"
        case toolInput = "tool_input"
        case toolOutput = "tool_output"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        id = try values.decode(String.self, forKey: .id)
        role = try values.decode(String.self, forKey: .role)
        kind = try values.decodeIfPresent(String.self, forKey: .kind) ?? "message"
        markdown = try values.decodeIfPresent(String.self, forKey: .markdown) ?? ""
        toolName = try values.decodeIfPresent(String.self, forKey: .toolName)
        toolInput = try values.decodeIfPresent(String.self, forKey: .toolInput)
        toolOutput = try values.decodeIfPresent(String.self, forKey: .toolOutput)
        timestamp = try values.decodeIfPresent(String.self, forKey: .timestamp)
    }
}

struct PulseAccount: Decodable, Identifiable, Equatable {
    let id: Int64
    let identity: String
    let displayName: String?

    private enum CodingKeys: String, CodingKey {
        case id, identity
        case displayName = "display_name"
    }

    var label: String { displayName ?? identity }
}

struct PulsePage<Element: Decodable & Equatable>: Decodable, Equatable {
    let items: [Element]
    let nextCursor: String?

    private enum CodingKeys: String, CodingKey {
        case items
        case nextCursor = "next_cursor"
    }
}

struct PulseUsage: Decodable, Equatable, Identifiable {
    let profile: String
    let vendor: String
    let window: PulseQuotaWindow
    let polledAt: String
    let contributors: [PulseContributor]

    private enum CodingKeys: String, CodingKey {
        case profile, vendor, window, contributors
        case polledAt = "polled_at"
    }

    var id: String { "\(profile):\(window.kind)" }
}

struct PulseQuotaWindow: Decodable, Equatable {
    let kind: String
    let usedPercent: Double
    let resetsAt: String

    private enum CodingKeys: String, CodingKey {
        case kind
        case usedPercent = "used_percent"
        case resetsAt = "resets_at"
    }
}

struct PulseContributor: Decodable, Equatable, Identifiable {
    let machine: String
    let reporterVersion: String?
    let polledAt: String
    let chosen: Bool

    private enum CodingKeys: String, CodingKey {
        case machine, chosen
        case reporterVersion = "reporter_version"
        case polledAt = "polled_at"
    }

    var id: String { machine }
}

struct OperationResponse: Decodable, Equatable {
    let ok: Bool
}

struct APIErrorResponse: Decodable {
    let error: String?
    let kind: String?
}
