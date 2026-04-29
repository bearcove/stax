import SwiftUI

struct DetailTabs: View {
    @Bindable var model: AppModel
    @State private var tab: Tab = .disassembly

    enum Tab: String, CaseIterable, Identifiable {
        case disassembly = "disassembly"
        case cfg         = "cfg"
        case intervals   = "intervals"
        var id: String { rawValue }
    }

    var body: some View {
        VStack(spacing: 0) {
            Picker("", selection: $tab) {
                ForEach(Tab.allCases) { t in
                    Text(t.rawValue).tag(t)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(8)

            Divider()

            ZStack {
                switch tab {
                case .disassembly: DisassemblyView(model: model)
                case .cfg:         CFGView()
                case .intervals:   IntervalsView(model: model)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

// MARK: - Disassembly

private struct DisassemblyView: View {
    @Bindable var model: AppModel

    var body: some View {
        Group {
            if let view = model.annotated {
                liveBody(view)
            } else if model.focusedAddress == nil {
                emptyState("Select a function to drill into.")
            } else {
                // Stream is subscribed but no update has arrived. Common
                // when the address is in an image stax-server hasn't
                // resolved (no DWARF / not yet seen / dlclosed) — the
                // server simply doesn't emit anything. Tell the user
                // why this might be quiet instead of pretending it's
                // about to load.
                let address = model.focusedAddress ?? 0
                emptyState(
                    """
                    no disassembly yet for 0x\(String(address, radix: 16))
                    (subscribed; the server may not have a binary for this address)
                    """
                )
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .textBackgroundColor).opacity(0.3))
    }

    @ViewBuilder
    private func liveBody(_ view: AnnotatedView) -> some View {
        let maxCost = max(
            1,
            view.lines.map(\.selfOnCpuNs).max() ?? 0
        )
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(Array(view.lines.enumerated()), id: \.offset) { _, line in
                    AnnotatedLineRow(line: line, baseAddress: view.baseAddress, maxCost: maxCost)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
    }

    private func emptyState(_ text: String) -> some View {
        VStack {
            Text(text)
                .font(.mono(.callout))
                .foregroundStyle(.tertiary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

private struct AnnotatedLineRow: View {
    let line: AnnotatedLine
    let baseAddress: UInt64
    let maxCost: UInt64

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let header = line.sourceHeader {
                Text("\(header.file):\(header.line)")
                    .font(.mono(.caption))
                    .foregroundStyle(.secondary)
                    .padding(.top, 4)
                Text(stripHTML(header.html))
                    .font(.mono(.caption))
                    .foregroundStyle(.tertiary)
                    .padding(.bottom, 2)
            }
            HStack(spacing: 8) {
                BarTrack(ratio: Double(line.selfOnCpuNs) / Double(maxCost))
                    .frame(width: 36, height: 3)
                Text(percentLabel(Double(line.selfOnCpuNs) / Double(maxCost)))
                    .foregroundStyle(.tertiary)
                    .frame(width: 32, alignment: .trailing)
                Text(addressOffset(line.address, base: baseAddress))
                    .foregroundStyle(.tertiary)
                    .frame(width: 64, alignment: .trailing)
                Text(stripHTML(line.html))
                    .foregroundStyle(.primary)
                    .lineLimit(1)
            }
            .font(.mono(.caption))
        }
    }
}

private func addressOffset(_ addr: UInt64, base: UInt64) -> String {
    if addr >= base {
        return String(format: "+0x%llx", addr - base)
    }
    return String(format: "0x%llx", addr)
}

private func stripHTML(_ html: String) -> String {
    var s = html.replacingOccurrences(of: "<[^>]+>", with: "", options: .regularExpression)
    s = s.replacingOccurrences(of: "&lt;", with: "<")
    s = s.replacingOccurrences(of: "&gt;", with: ">")
    s = s.replacingOccurrences(of: "&amp;", with: "&")
    s = s.replacingOccurrences(of: "&quot;", with: "\"")
    s = s.replacingOccurrences(of: "&#39;", with: "'")
    return s
}

private func percentLabel(_ ratio: Double) -> String {
    if ratio < 0.005 { return "" }
    return String(format: "%.1f%%", ratio * 100)
}

// MARK: - Intervals

private struct IntervalsView: View {
    @Bindable var model: AppModel
    @State private var selection: AppModel.Interval.ID?

    var body: some View {
        Group {
            if model.intervals.isEmpty {
                VStack(spacing: 6) {
                    Text("intervals not wired yet")
                        .font(.mono(.callout))
                        .foregroundStyle(.tertiary)
                    Text("needs a flame-graph stack-frame click → flame_key")
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                liveBody
            }
        }
        .background(Color(nsColor: .textBackgroundColor).opacity(0.3))
    }

    private var liveBody: some View {
        VStack(spacing: 0) {
            HStack {
                Text(
                    "\(model.intervalsTotalCount.formatted()) intervals · \(formatDuration(model.intervalsTotalDuration)) total"
                )
                .font(.mono(.caption))
                .foregroundStyle(.secondary)
                Spacer()
            }
            .padding(.horizontal, 12)
            .padding(.top, 8)
            .padding(.bottom, 6)

            Divider()

            Table(model.intervals, selection: $selection) {
                TableColumn("START") { i in
                    Text(String(format: "%.3fs", i.start))
                        .font(.mono(.caption))
                        .foregroundStyle(.secondary)
                }
                .width(min: 50, ideal: 60, max: 80)

                TableColumn("DURATION") { i in
                    Text(formatDuration(i.duration))
                        .font(.mono(.caption))
                        .frame(maxWidth: .infinity, alignment: .trailing)
                }
                .width(min: 60, ideal: 80, max: 110)

                TableColumn("REASON") { i in
                    Text(i.reason.rawValue)
                        .font(.mono(.caption))
                        .foregroundStyle(i.reason.color)
                }
                .width(min: 50, ideal: 70, max: 90)

                TableColumn("TID") { i in
                    Text(String(i.tid))
                        .font(.mono(.caption))
                        .foregroundStyle(.secondary)
                }
                .width(min: 60, ideal: 80, max: 100)

                TableColumn("WOKEN BY") { i in
                    Text(i.wokenBy.map { String($0) } ?? "(none)")
                        .font(.mono(.caption))
                        .foregroundStyle(.tertiary)
                }
            }
        }
    }
}

// MARK: - CFG (placeholder)

private struct CFGView: View {
    var body: some View {
        VStack(spacing: 8) {
            Text("control flow graph not wired yet")
                .font(.mono(.callout))
                .foregroundStyle(.tertiary)
            Text("needs a server-side subscribe_cfg(address)")
                .font(.caption)
                .foregroundStyle(.tertiary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .textBackgroundColor).opacity(0.3))
    }
}
