import SwiftUI

struct FunctionTable: View {
    @Bindable var model: AppModel
    @State private var sort: [KeyPathComparator<AppModel.FunctionEntry>] = [
        .init(\.selfTime, order: .reverse)
    ]
    @State private var selection: AppModel.FunctionEntry.ID?

    var body: some View {
        Table(model.functions.sorted(using: sort), selection: $selection, sortOrder: $sort) {
            TableColumn("function", value: \.name) { fn in
                VStack(alignment: .leading, spacing: 0) {
                    Text(fn.name)
                        .font(.mono(.callout))
                        .lineLimit(1)
                    Text(fn.binary)
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)
                }
            }
            TableColumn("self", value: \.selfTime) { fn in
                Text(fn.selfTime > 0 ? formatDuration(fn.selfTime) : "—")
                    .font(.mono(.caption))
                    .foregroundStyle(fn.selfTime > 0 ? .primary : .tertiary)
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(min: 70, ideal: 80, max: 110)
            TableColumn("total", value: \.totalTime) { fn in
                Text(fn.totalTime > 0 ? formatDuration(fn.totalTime) : "—")
                    .font(.mono(.caption))
                    .foregroundStyle(fn.totalTime > 0 ? .primary : .tertiary)
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(min: 70, ideal: 80, max: 110)
        }
    }
}
