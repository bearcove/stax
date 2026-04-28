import SwiftUI

struct MainPane: View {
    @Bindable var model: AppModel

    var body: some View {
        VSplitView {
            VStack(spacing: 0) {
                minimap
                Divider()
                flame
            }
            .frame(minHeight: 200)

            HSplitView {
                FunctionTable(model: model)
                    .frame(minWidth: 320)
                DetailTabs(model: model)
                    .frame(minWidth: 320)
            }
            .frame(minHeight: 160)
        }
    }

    private var minimap: some View {
        ZStack {
            Color(nsColor: .underPageBackgroundColor)
            Text("minimap")
                .font(.caption)
                .foregroundStyle(.tertiary)
        }
        .frame(height: 56)
    }

    private var flame: some View {
        ZStack {
            Color(nsColor: .textBackgroundColor).opacity(0.3)
            Text("flame goes here")
                .font(.mono(.callout))
                .foregroundStyle(.tertiary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
