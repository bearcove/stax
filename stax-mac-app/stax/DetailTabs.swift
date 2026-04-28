import SwiftUI

struct DetailTabs: View {
    @Bindable var model: AppModel
    @State private var tab: Tab = .disassembly

    enum Tab: String, CaseIterable, Identifiable {
        case disassembly = "disassembly"
        case familyTree = "family tree"
        case intervals = "intervals"
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
                case .disassembly: DisassemblyPlaceholder()
                case .familyTree:  FamilyTreePlaceholder()
                case .intervals:   IntervalsPlaceholder()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

private struct DisassemblyPlaceholder: View {
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                AsmRow(label: "validations.rs:133", lines: [
                    ("+0x0",  "subs",  "x9, x1, #0xf"),
                    ("+0x4",  "csel",  "x10, xzr, x9, lo"),
                ])
                AsmRow(label: "validations.rs:145", lines: [
                    ("+0x8",  "cbz",   "x1, $+0x204"),
                    ("+0xc",  "mov",   "x9, #0x0"),
                    ("+0x10", "add",   "x11, x0, #0x7"),
                    ("+0x14", "and",   "x11, x11, #0xfffffffffffffff8"),
                    ("+0x18", "sub",   "x11, x11, x0"),
                    ("+0x1c", "adrp",  "x12, $+0x292000"),
                    ("+0x20", "add",   "x12, x12, #0x18d"),
                    ("+0x24", "b",     "$+0x10"),
                ])
                AsmRow(label: "validations.rs:217", lines: [
                    ("+0x28", "add",   "x9, x13, #0x1"),
                ])
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
        .background(Color(nsColor: .textBackgroundColor).opacity(0.3))
    }
}

private struct AsmRow: View {
    let label: String
    let lines: [(addr: String, op: String, args: String)]

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(label)
                .font(.mono(.caption))
                .foregroundStyle(.secondary)
                .padding(.top, 4)
            Text("(source not on disk)")
                .font(.mono(.caption))
                .foregroundStyle(.tertiary)
                .padding(.bottom, 4)
            ForEach(lines, id: \.addr) { line in
                HStack(spacing: 8) {
                    Text(line.addr)
                        .foregroundStyle(.tertiary)
                        .frame(width: 50, alignment: .trailing)
                    Text(line.op)
                        .foregroundStyle(.primary)
                        .frame(width: 50, alignment: .leading)
                    Text(line.args)
                        .foregroundStyle(.secondary)
                }
                .font(.mono(.caption))
            }
        }
    }
}

private struct FamilyTreePlaceholder: View {
    var body: some View {
        Text("family tree")
            .font(.mono(.callout))
            .foregroundStyle(.tertiary)
    }
}

private struct IntervalsPlaceholder: View {
    var body: some View {
        Text("intervals")
            .font(.mono(.callout))
            .foregroundStyle(.tertiary)
    }
}
