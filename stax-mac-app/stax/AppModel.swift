import Foundation
import Observation
import SwiftUI

@Observable
@MainActor
final class AppModel {
    var paused: Bool = false
    /// `nil` means all threads.
    var threadFilter: Int? = nil
    /// Quoted (`"foo"`) → exact substring. Slashed (`/foo/`) → regex.
    /// Plain text → fuzzy substring.
    var searchQuery: String = ""

    enum CPUMode: String, CaseIterable, Identifiable {
        case onCPU = "on-cpu"
        case offCPU = "off-cpu"
        case wall = "wall"
        var id: String { rawValue }

        var fakeStat: String {
            switch self {
            case .onCPU:  "3.0ms"
            case .offCPU: "1.70s"
            case .wall:   "1.71s"
            }
        }
    }
    var cpuMode: CPUMode = .onCPU

    enum EventMode: String, CaseIterable, Identifiable {
        case ipc = "ipc"
        case l1d = "l1d"
        case brMiss = "br-miss"
        var id: String { rawValue }

        var fakeStat: String {
            switch self {
            case .ipc:    "1.42"
            case .l1d:    "32k"
            case .brMiss: "1.1k"
            }
        }
    }
    var eventMode: EventMode? = .ipc

    enum Category: String, CaseIterable, Identifiable {
        case main, dylib, system, other
        var id: String { rawValue }

        var color: Color {
            switch self {
            case .main:   Color(red: 0.96, green: 0.78, blue: 0.27) // amber
            case .dylib:  Color(red: 0.36, green: 0.78, blue: 0.85) // cyan
            case .system: Color(red: 0.95, green: 0.55, blue: 0.43) // coral
            case .other:  Color(red: 0.74, green: 0.56, blue: 0.91) // violet
            }
        }

        var fakeCount: Int {
            switch self {
            case .main:   18
            case .dylib:  24
            case .system: 6
            case .other:  2
            }
        }
    }
    var categories: Set<Category> = [.main, .dylib]

    struct ThreadInfo: Identifiable, Hashable {
        var id: Int { tid }
        let tid: Int
        let name: String?
        let onCPU: TimeInterval

        var displayName: String {
            name ?? "[\(tid)]"
        }
    }
    var threads: [ThreadInfo] = [
        .init(tid: 1,    name: "main",     onCPU: 2.40),
        .init(tid: 1024, name: "worker-1", onCPU: 0.36),
        .init(tid: 1025, name: "worker-2", onCPU: 0.24),
        .init(tid: 1026, name: "worker-3", onCPU: 0.06),
    ]

    /// Threads sorted by on-CPU time, descending.
    var threadsSorted: [ThreadInfo] {
        threads.sorted { $0.onCPU > $1.onCPU }
    }

    var totalThreadOnCPU: TimeInterval {
        threads.reduce(0) { $0 + $1.onCPU }
    }

    var maxThreadOnCPU: TimeInterval {
        max(0.001, threads.map(\.onCPU).max() ?? 0)
    }

    func thread(forTid tid: Int) -> ThreadInfo? {
        threads.first { $0.tid == tid }
    }

    // Fake stats for the bottom status bar.
    var onCPUTime: TimeInterval = 0.003
    var offCPUTime: TimeInterval = 1.70
    var symbolCount: Int = 50

    struct FunctionEntry: Identifiable, Hashable {
        let id = UUID()
        let name: String
        let binary: String
        let selfTime: TimeInterval
        let totalTime: TimeInterval
    }
    var functions: [FunctionEntry] = [
        .init(name: "start_wqthread",                      binary: "libsystem_pthread.dylib", selfTime: 0.0012,    totalTime: 0.0024),
        .init(name: "core::str::converts::from_utf8",      binary: "transcribe-metal",        selfTime: 0,         totalTime: 0),
        .init(name: "__psynch_cvwait",                     binary: "libsystem_kernel.dylib",  selfTime: 0.0000766, totalTime: 0.0000766),
        .init(name: "write",                               binary: "libsystem_kernel.dylib",  selfTime: 0,         totalTime: 0),
        .init(name: "iokit_user_client_trap",              binary: "IOKit",                   selfTime: 0.0012,    totalTime: 0.0012),
        .init(name: "__kdebug_trace64",                    binary: "libsystem_kernel.dylib",  selfTime: 0.0000180, totalTime: 0.0000180),
        .init(name: "rustfft::algorithm::mixed_radix",     binary: "transcribe-metal",        selfTime: 0,         totalTime: 0),
        .init(name: "core::hash::BuildHasher::hash_one",   binary: "transcribe-metal",        selfTime: 0,         totalTime: 0),
        .init(name: "_xzm_xzone_malloc_tiny",              binary: "libsystem_malloc.dylib",  selfTime: 0,         totalTime: 0),
    ]
}
