import Foundation
import Observation

@Observable
@MainActor
final class AppModel {
    var paused: Bool = false
    var threadFilter: String = "all threads"
    var searchQuery: String = ""
    var searchRegex: Bool = true

    enum CPUMode: String, CaseIterable, Identifiable {
        case onCPU = "on-cpu"
        case offCPU = "off-cpu"
        case wall = "wall"
        var id: String { rawValue }
    }
    var cpuMode: CPUMode = .onCPU

    enum EventMode: String, CaseIterable, Identifiable {
        case ipc = "ipc"
        case l1d = "l1d"
        case brMiss = "br-miss"
        var id: String { rawValue }
    }
    var eventMode: EventMode? = .ipc

    enum Category: String, CaseIterable, Identifiable {
        case main, dylib, system, other
        var id: String { rawValue }
    }
    var categories: Set<Category> = [.main, .dylib]

    var lightMode: Bool = false

    // Fake stats for the toolbar's right side.
    var onCPUTime: TimeInterval = 0.003
    var offCPUTime: TimeInterval = 1.70
    var symbolCount: Int = 50
}
