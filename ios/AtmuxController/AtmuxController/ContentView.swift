import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var model: AppModel
    @State private var showConnections = false

    var body: some View {
        TabView {
            SessionsView(showConnections: $showConnections)
                .tabItem { Label("Agents", systemImage: "terminal") }
            PulseView(showConnections: $showConnections)
                .tabItem { Label("Usage", systemImage: "gauge.with.dots.needle.67percent") }
        }
        .safeAreaInset(edge: .top) {
            if model.isOffline {
                Label("Server offline — showing the last loaded view", systemImage: "wifi.slash")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.white)
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 6)
                    .background(.orange)
            }
        }
        .sheet(isPresented: $showConnections) {
            ConnectionView(isPresented: $showConnections)
                .environmentObject(model)
        }
        .alert(
            "atmux",
            isPresented: Binding(
                get: { model.errorMessage != nil },
                set: { if !$0 { model.errorMessage = nil } }
            ),
            actions: { Button("OK") { model.errorMessage = nil } },
            message: { Text(model.errorMessage ?? "") }
        )
        .task {
            if model.activeProfile == nil {
                showConnections = true
            } else {
                await model.refreshDashboard()
            }
        }
    }
}
