import SwiftUI

struct Toolbar: View {
    @Bindable var model: AppModel

    var body: some View {
        HStack(spacing: 8) {
            pauseButton
            threadMenu
            searchField
            Divider().frame(height: 16)
            cpuModePills
            eventModePills
            Divider().frame(height: 16)
            categoryPills
            themeToggle
            Spacer()
            statsLabel
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(.bar)
        .overlay(alignment: .bottom) {
            Divider()
        }
    }

    private var pauseButton: some View {
        Pill(
            label: model.paused ? "play" : "pause",
            systemImage: model.paused ? "play.fill" : "pause.fill",
            isOn: false,
            accent: .secondary
        ) {
            model.paused.toggle()
        }
    }

    private var threadMenu: some View {
        Menu {
            Button("all threads") { model.threadFilter = "all threads" }
            Button("main") { model.threadFilter = "main" }
        } label: {
            HStack(spacing: 4) {
                Text(model.threadFilter)
                Image(systemName: "chevron.down").font(.caption2)
            }
            .font(.system(.caption, design: .monospaced))
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(.quaternary, in: .rect(cornerRadius: 4))
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
    }

    private var searchField: some View {
        HStack(spacing: 4) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
                .font(.caption)
            TextField("search symbols", text: $model.searchQuery)
                .textFieldStyle(.plain)
                .font(.system(.caption, design: .monospaced))
            Toggle(isOn: $model.searchRegex) {
                Text(".*")
                    .font(.system(.caption, design: .monospaced))
            }
            .toggleStyle(PillToggleStyle(accent: .blue))
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .frame(width: 220)
        .background(.quaternary, in: .rect(cornerRadius: 4))
    }

    private var cpuModePills: some View {
        HStack(spacing: 4) {
            ForEach(AppModel.CPUMode.allCases) { mode in
                Pill(
                    label: mode.rawValue,
                    isOn: model.cpuMode == mode,
                    accent: .green
                ) {
                    model.cpuMode = mode
                }
            }
        }
    }

    private var eventModePills: some View {
        HStack(spacing: 4) {
            ForEach(AppModel.EventMode.allCases) { mode in
                Pill(
                    label: mode.rawValue,
                    isOn: model.eventMode == mode,
                    accent: .green
                ) {
                    model.eventMode = (model.eventMode == mode) ? nil : mode
                }
            }
        }
    }

    private var categoryPills: some View {
        HStack(spacing: 4) {
            ForEach(AppModel.Category.allCases) { cat in
                Pill(
                    label: cat.rawValue,
                    isOn: model.categories.contains(cat),
                    accent: .gray
                ) {
                    if model.categories.contains(cat) {
                        model.categories.remove(cat)
                    } else {
                        model.categories.insert(cat)
                    }
                }
            }
        }
    }

    private var themeToggle: some View {
        Button {
            model.lightMode.toggle()
        } label: {
            Image(systemName: model.lightMode ? "sun.max.fill" : "sun.max")
                .font(.caption)
                .padding(6)
                .background(.quaternary, in: .rect(cornerRadius: 4))
        }
        .buttonStyle(.plain)
    }

    private var statsLabel: some View {
        Text(statsText)
            .font(.system(.caption, design: .monospaced))
            .foregroundStyle(.secondary)
    }

    private var statsText: String {
        let onCPU = formatDuration(model.onCPUTime)
        let offCPU = formatDuration(model.offCPUTime)
        return "\(onCPU) on-CPU · \(offCPU) off-CPU · \(model.symbolCount) symbols"
    }
}

private struct Pill: View {
    var label: String
    var systemImage: String? = nil
    var isOn: Bool
    var accent: Color
    var action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 3) {
                if let systemImage {
                    Image(systemName: systemImage).font(.caption2)
                }
                Text(label)
            }
            .font(.system(.caption, design: .monospaced))
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .foregroundStyle(isOn ? accent : Color.secondary)
            .background {
                RoundedRectangle(cornerRadius: 4)
                    .fill(isOn ? accent.opacity(0.18) : Color.gray.opacity(0.12))
            }
        }
        .buttonStyle(.plain)
    }
}

private struct PillToggleStyle: ToggleStyle {
    var accent: Color

    func makeBody(configuration: Configuration) -> some View {
        Button {
            configuration.isOn.toggle()
        } label: {
            configuration.label
                .padding(.horizontal, 4)
                .padding(.vertical, 1)
                .foregroundStyle(configuration.isOn ? accent : Color.secondary)
                .background {
                    RoundedRectangle(cornerRadius: 3)
                        .fill(configuration.isOn ? accent.opacity(0.18) : Color.clear)
                }
        }
        .buttonStyle(.plain)
    }
}

func formatDuration(_ seconds: TimeInterval) -> String {
    if seconds < 1 {
        return String(format: "%.1fms", seconds * 1000)
    }
    return String(format: "%.2fs", seconds)
}
