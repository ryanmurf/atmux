import XCTest
#if SWIFT_PACKAGE
@testable import AtmuxCore
#else
@testable import AtmuxController
#endif

final class ModelDecodingTests: XCTestCase {
    func testOverviewDecodesFederatedSessionAndDefaultsOptionalFields() throws {
        let json = #"{"revision":42,"health":null,"sessions":[{"id":"max~%1","name":"ibm","pane_id":"%1","status":"waiting","agent":"claude","profile":"claude-max"}],"machines":[{"id":"max","label":"Max","kind":"remote","online":true,"sessions":1,"health":null,"metrics":{"cpu_percent":25,"memory_used_bytes":8,"memory_total_bytes":16,"gpus":[],"temperatures":[]}}]}"#.data(using: .utf8)!
        let overview = try JSONDecoder().decode(AtmuxOverview.self, from: json)
        XCTAssertEqual(overview.revision, 42)
        XCTAssertEqual(overview.sessions.first?.machine, "local")
        XCTAssertEqual(overview.sessions.first?.profile, "claude-max")
        XCTAssertEqual(overview.machines.first?.metrics?.cpuPercent, 25)
    }

    func testTranscriptDecodesMessageAndToolDetails() throws {
        let json = #"{"available":true,"source":"claude","content_hash":"abc","changed":true,"truncated":false,"messages":[{"id":"m1","role":"assistant","kind":"tool_call","markdown":"Checking","tool_name":"Read","tool_input":"file.swift","timestamp":"2026-08-09T12:00:00Z"}]}"#.data(using: .utf8)!
        let transcript = try JSONDecoder().decode(Transcript.self, from: json)
        XCTAssertEqual(transcript.messages.first?.toolName, "Read")
        XCTAssertEqual(transcript.messages.first?.kind, "tool_call")
    }

    func testPulseUsageDecodesProviderQuotaAndProvenance() throws {
        let json = #"{"items":[{"profile":"claude-max","vendor":"anthropic-oauth","window":{"kind":"five_hour","used_percent":62.5,"resets_at":"2026-08-10T01:00:00Z"},"polled_at":"2026-08-09T20:00:00Z","contributors":[{"machine":"max","reporter_version":"atmux-test","polled_at":"2026-08-09T20:00:00Z","chosen":true}]}],"next_cursor":null}"#.data(using: .utf8)!
        let page = try JSONDecoder().decode(PulsePage<PulseUsage>.self, from: json)
        XCTAssertEqual(page.items.first?.window.usedPercent, 62.5)
        XCTAssertEqual(page.items.first?.contributors.first?.machine, "max")
        XCTAssertEqual(page.items.first?.contributors.first?.chosen, true)
    }

    func testPaneModelsDecodeCurrentAndSwitchableOwnerOptions() throws {
        let json = #"{"pane_id":"%1","harness":"codex","current":"gpt-5.6-sol","version":"0.147.0","models":[{"id":"gpt-5.6-sol","label":"GPT-5.6 Sol","switchable":true},{"id":"configured-only","label":"Configured only","switchable":false}],"note":null}"#.data(using: .utf8)!
        let models = try JSONDecoder().decode(PaneModels.self, from: json)
        XCTAssertEqual(models.current, "gpt-5.6-sol")
        XCTAssertEqual(models.models.first?.switchable, true)
        XCTAssertEqual(models.models.last?.switchable, false)
    }

    func testMachineGPUShapeMatchesBoundedRustTelemetry() throws {
        let json = #"{"id":"max","label":"Max","kind":"remote","online":true,"sessions":1,"health":null,"last_seen_ms":42,"address":"max.local:7345","metrics":{"cpu_percent":25,"memory_used_bytes":8589934592,"memory_total_bytes":17179869184,"gpus":[{"id":"0000:03:00.0","name":"Radeon RX 7900 XTX","vendor":"AMD","utilization_percent":42,"memory_used_bytes":8589934592,"memory_total_bytes":25769803776,"temperature_celsius":61.5,"unavailable":[]}],"temperatures":[{"label":"CPU","celsius":55.5}],"gpu_diagnostics":[]}}"#.data(using: .utf8)!
        let machine = try JSONDecoder().decode(AtmuxMachine.self, from: json)
        let gpu = try XCTUnwrap(machine.metrics?.gpus.first)
        XCTAssertEqual(gpu.name, "Radeon RX 7900 XTX")
        XCTAssertEqual(gpu.vendor, "AMD")
        XCTAssertEqual(gpu.utilizationPercent, 42)
        XCTAssertEqual(gpu.memoryTotalBytes, 25_769_803_776)
        XCTAssertEqual(gpu.temperatureCelsius, 61.5)
    }
}
