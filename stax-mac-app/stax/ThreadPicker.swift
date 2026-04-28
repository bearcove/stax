import SwiftUI

struct ThreadPicker: View {
    @Bindable var model: AppModel
    @State private var open = false
    @State private var query = ""

    var body: some View {
        Button {
            open.toggle()
        } label: {
            HStack(spacing: 4) {
                Text(triggerLabel)
                    .lineLimit(1)
                Image(systemName: "chevron.down")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            .frame(minWidth: 110, alignment: .leading)
        }
        .buttonStyle(.bordered)
        .controlSize(.small)
        .popover(isPresented: $open, arrowEdge: .bottom) {
            popoverBody
                .frame(width: 320)
        }
    }

    private var triggerLabel: String {
        guard let tid = model.threadFilter else { return "all threads" }
        return model.thread(forTid: tid)?.displayName ?? "[\(tid)]"
    }

    private var filtered: [AppModel.ThreadInfo] {
        let q = query.trimmingCharacters(in: .whitespaces).lowercased()
        let sorted = model.threadsSorted
        guard !q.isEmpty else { return sorted }
        return sorted.filter { t in
            if String(t.tid).contains(q) { return true }
            if let n = t.name, n.lowercased().contains(q) { return true }
            return false
        }
    }

    private var popoverBody: some View {
        VStack(spacing: 0) {
            TextField("search threads…", text: $query)
                .textFieldStyle(.roundedBorder)
                .controlSize(.small)
                .padding(8)
                .onSubmit {
                    if filtered.count == 1 { pick(filtered[0].tid) }
                }

            Divider()

            ScrollView {
                LazyVStack(spacing: 0) {
                    ThreadRow(
                        name: "all threads",
                        tidSuffix: nil,
                        detail: formatDuration(model.totalThreadOnCPU),
                        ratio: nil,
                        isSelected: model.threadFilter == nil
                    ) { pick(nil) }

                    if filtered.isEmpty {
                        Text("no matches")
                            .font(.callout)
                            .foregroundStyle(.tertiary)
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 12)
                    } else {
                        ForEach(filtered) { t in
                            ThreadRow(
                                name: t.name ?? "[\(t.tid)]",
                                tidSuffix: t.name == nil ? nil : " [\(t.tid)]",
                                detail: formatDuration(t.onCPU),
                                ratio: t.onCPU / model.maxThreadOnCPU,
                                isSelected: model.threadFilter == t.tid
                            ) { pick(t.tid) }
                        }
                    }
                }
            }
            .frame(maxHeight: 280)

            Divider()

            Text("\(model.threads.count) threads · sorted by on-CPU time")
                .font(.caption)
                .foregroundStyle(.tertiary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(8)
        }
    }

    private func pick(_ tid: Int?) {
        model.threadFilter = tid
        open = false
        query = ""
    }
}

private struct ThreadRow: View {
    let name: String
    let tidSuffix: String?
    let detail: String
    let ratio: Double?
    let isSelected: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Image(systemName: "checkmark")
                    .font(.caption)
                    .opacity(isSelected ? 1 : 0)
                    .frame(width: 12)

                HStack(spacing: 0) {
                    Text(name)
                        .lineLimit(1)
                    if let tidSuffix {
                        Text(tidSuffix)
                            .foregroundStyle(.tertiary)
                            .lineLimit(1)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)

                if let r = ratio {
                    BarTrack(ratio: r)
                        .frame(width: 60, height: 4)
                }

                Text(detail)
                    .font(.mono(.caption))
                    .foregroundStyle(.secondary)
                    .frame(width: 56, alignment: .trailing)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 5)
            .contentShape(.rect)
            .background(isSelected ? Color.accentColor.opacity(0.12) : Color.clear)
        }
        .buttonStyle(.plain)
    }
}

struct BarTrack: View {
    let ratio: Double

    var body: some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                Capsule()
                    .fill(Color.gray.opacity(0.2))
                Capsule()
                    .fill(Color.accentColor)
                    .frame(width: max(2, geo.size.width * min(1, max(0, ratio))))
            }
        }
    }
}
