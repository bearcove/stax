import SwiftUI

struct StatusBar: View {
    @Bindable var model: AppModel

    var body: some View {
        HStack {
            Spacer()
            Text(statsText)
                .font(.mono(.callout))
                .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.bar)
        .overlay(alignment: .top) { Divider() }
    }

    private var statsText: String {
        let onCPU = formatDuration(model.onCPUTime)
        let offCPU = formatDuration(model.offCPUTime)
        return "\(onCPU) on-CPU · \(offCPU) off-CPU · \(model.symbolCount) symbols"
    }
}

func formatDuration(_ seconds: TimeInterval) -> String {
    if seconds < 1 {
        return String(format: "%.1fms", seconds * 1000)
    }
    return String(format: "%.2fs", seconds)
}
